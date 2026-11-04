//! 更新包下载加速：GitHub 镜像改写 + Range 分段并发 + 签名校验。
//!
//! 为什么不用 `tauri-plugin-updater` 自带的 `Update::download`：它是一条连接从头拉到尾，
//! 国内直连 github.com 常年几十 KB/s，甚至直接超时。插件把 `download_url` / `signature`
//! 都暴露成 pub 字段，`install(bytes)` 也接受外部字节，所以这里自己下载、自己验签，
//! 再把同一份字节交给 `install`，安装语义与插件完全一致。
//!
//! 三层保障：
//! 1. **镜像优先**：前缀改写 GitHub 下载地址，直连放最后兜底；
//! 2. **分段并发**：4 路 Range 请求并行拉取，按「空闲超时」而不是总超时判死，
//!    避免慢速但可用的线路被误杀；
//! 3. **签名兜底**：任何一个镜像返回脏数据 / 半截文件，都会在验签这步被拒，
//!    绝不可能装进用户机器。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;

use crate::modules::config::DEFAULT_HTTP_USER_AGENT;

/// 进度回调：`(累计已下载字节, 总字节)`。多路分段共用一个回调，调用方需自行去抖。
pub type Progress = Arc<dyn Fn(u64, Option<u64>) + Send + Sync>;

/// 内置 GitHub 镜像前缀（按实测速度排序，直连放最后）。
///
/// 只放实测过「能下完整文件 + 支持 Range」的站点；镜像挂掉时自动降级，
/// 不会影响更新成功率。
pub const MIRROR_PREFIXES: [&str; 2] = ["https://gh-proxy.com", "https://ghproxy.net"];

/// 分段并发数。安装包约 8.5 MB，4 路已能跑满带宽；再多容易被镜像限流。
pub const SEGMENT_CONNECTIONS: usize = 4;

/// 小于这个体积不值得分段（小文件握手开销比并发收益大）。
const MIN_SEGMENT_BYTES: u64 = 1 << 20;

/// 建连超时。镜像或代理不可达时快速失败，别让用户干等。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// 空闲超时：连续这么久没收到任何字节就判定这条线路已死。
/// 用「空闲」而不是「总时长」，慢速线路才能跑完（8.5 MB 走 50 KB/s 需要近 3 分钟）。
const IDLE_TIMEOUT: Duration = Duration::from_secs(20);

/// 单个分片的重试次数（不含首次）。重试只针对同一分片，不会从头再来。
const SEGMENT_RETRIES: usize = 2;

/// 探测请求（`Range: bytes=0-0`）的超时。
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// 把 GitHub 下载地址改写成镜像地址；前缀为空时原样返回。
pub fn mirror_url(url: &str, prefix: &str) -> String {
    let prefix = prefix.trim().trim_end_matches('/');
    if prefix.is_empty() {
        return url.to_string();
    }
    format!("{prefix}/{url}")
}

/// 候选下载地址：先镜像、后直连。非 GitHub 地址不做改写（第三方自建更新源）。
pub fn download_candidates(url: &str) -> Vec<String> {
    if !url.starts_with("https://github.com/") {
        return vec![url.to_string()];
    }
    let mut candidates: Vec<String> = MIRROR_PREFIXES
        .iter()
        .map(|prefix| mirror_url(url, prefix))
        .collect();
    candidates.push(url.to_string());
    candidates.dedup();
    candidates
}

/// 把总字节切成 `connections` 段，返回闭区间 `(start, end)`。
pub fn plan_segments(total: u64, connections: usize) -> Vec<(u64, u64)> {
    if total == 0 || connections == 0 {
        return Vec::new();
    }
    let chunk = total.div_ceil(connections as u64);
    let mut segments = Vec::new();
    let mut start = 0;
    while start < total {
        let end = (start + chunk).min(total) - 1;
        segments.push((start, end));
        start = end + 1;
    }
    segments
}

/// 解析 `Content-Range: bytes 0-0/8815103` 里的总长度。
fn parse_content_range(value: &str) -> Option<u64> {
    let (_, total) = value.rsplit_once('/')?;
    total.trim().parse::<u64>().ok()
}

/// 校验更新包签名（minisign，与 `tauri-plugin-updater` 同一套）。
///
/// 公钥与签名在配置里都是「base64 编码的 minisign 文本」，这里按同样方式先解一层 base64。
pub fn verify_signature(data: &[u8], signature: &str, pubkey: &str) -> Result<(), String> {
    let public_key = minisign_verify::PublicKey::decode(&decode_base64(pubkey, "更新公钥")?)
        .map_err(|error| format!("更新公钥无效：{error}"))?;
    let signature = minisign_verify::Signature::decode(&decode_base64(signature, "更新签名")?)
        .map_err(|error| format!("更新签名格式无效：{error}"))?;
    public_key.verify(data, &signature, true).map_err(|error| {
        format!("更新包签名校验失败（文件可能被篡改或没下载完整）：{error}")
    })
}

fn decode_base64(value: &str, what: &str) -> Result<String, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value.trim())
        .map_err(|error| format!("{what}编码无效：{error}"))?;
    String::from_utf8(bytes).map_err(|_| format!("{what}不是合法的 UTF-8 文本"))
}

/// 把 `reqwest` 的错误连同底层原因一起拼出来。
///
/// 只打 `error.to_string()` 时永远只有一句 "error sending request"，用户和日志都看不出
/// 到底是 DNS、代理还是 TLS 挂了；这里顺着 `source()` 一路展开。
fn describe(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(inner) = source {
        text.push_str("：");
        text.push_str(&inner.to_string());
        source = inner.source();
    }
    text
}

fn build_client(proxy: Option<&str>) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .user_agent(DEFAULT_HTTP_USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT);
    if let Some(proxy) = proxy.map(str::trim).filter(|value| !value.is_empty()) {
        builder = builder.proxy(
            reqwest::Proxy::all(proxy).map_err(|error| format!("更新代理地址无效：{error}"))?,
        );
    }
    builder
        .build()
        .map_err(|error| format!("初始化下载器失败：{error}"))
}

struct Probe {
    total: Option<u64>,
    ranged: bool,
}

async fn probe(client: &reqwest::Client, url: &str) -> Result<Probe, String> {
    let response = client
        .get(url)
        .header(reqwest::header::RANGE, "bytes=0-0")
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
        .map_err(|error| format!("连接失败：{}", describe(&error)))?;
    let status = response.status();
    if status == reqwest::StatusCode::PARTIAL_CONTENT {
        let total = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_content_range);
        return Ok(Probe {
            total,
            ranged: true,
        });
    }
    if status.is_success() {
        // 不支持 Range：整包单连接拉，总量取 Content-Length。
        return Ok(Probe {
            total: response.content_length(),
            ranged: false,
        });
    }
    Err(format!("返回 HTTP {}", status.as_u16()))
}

/// 从单个候选地址下载整包字节。分段是否启用由探测结果决定。
pub async fn fetch(
    url: &str,
    proxy: Option<&str>,
    progress: Progress,
) -> Result<Vec<u8>, String> {
    let client = build_client(proxy)?;
    let probe = probe(&client, url).await?;
    let total = probe.total;
    let use_segments =
        probe.ranged && total.is_some_and(|size| size >= MIN_SEGMENT_BYTES);
    if !use_segments {
        return download_stream(&client, url, total, &progress).await;
    }
    let total = total.unwrap_or_default();
    match download_segmented(&client, url, total, &progress).await {
        Ok(bytes) => Ok(bytes),
        Err(error) => {
            // 探测说支持 Range、实际分段却失败了（镜像前后不一致）：退回单连接再试一次，
            // 别因为一次抖动把整条通道判死。
            eprintln!("[更新] 分段下载失败（{error}），改用单连接重试");
            download_stream(&client, url, Some(total), &progress).await
        }
    }
}

async fn download_segmented(
    client: &reqwest::Client,
    url: &str,
    total: u64,
    progress: &Progress,
) -> Result<Vec<u8>, String> {
    let downloaded = Arc::new(AtomicU64::new(0));
    let mut tasks = tokio::task::JoinSet::new();
    for (index, (start, end)) in plan_segments(total, SEGMENT_CONNECTIONS).into_iter().enumerate()
    {
        let client = client.clone();
        let url = url.to_string();
        let downloaded = Arc::clone(&downloaded);
        let progress = Arc::clone(progress);
        tasks.spawn(async move {
            download_range(&client, &url, (start, end), total, &downloaded, &progress)
                .await
                .map(|bytes| (index, bytes))
        });
    }

    let mut parts: Vec<(usize, Vec<u8>)> = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        let (index, bytes) =
            joined.map_err(|error| format!("分段下载任务异常退出：{error}"))??;
        parts.push((index, bytes));
    }
    parts.sort_by_key(|(index, _)| *index);

    let mut out = Vec::with_capacity(total as usize);
    for (_, part) in parts {
        out.extend_from_slice(&part);
    }
    if out.len() as u64 != total {
        return Err(format!(
            "下载不完整：期望 {total} 字节，实得 {} 字节",
            out.len()
        ));
    }
    Ok(out)
}

async fn download_range(
    client: &reqwest::Client,
    url: &str,
    (start, end): (u64, u64),
    total: u64,
    downloaded: &AtomicU64,
    progress: &Progress,
) -> Result<Vec<u8>, String> {
    let mut last_error = String::new();
    for attempt in 0..=SEGMENT_RETRIES {
        let mut counted = 0u64;
        match stream_range(client, url, start, end, total, downloaded, &mut counted, progress).await
        {
            Ok(bytes) => return Ok(bytes),
            Err(error) => {
                // 这一段已计入的字节要退回，否则重试后进度会虚高。
                downloaded.fetch_sub(counted, Ordering::Relaxed);
                last_error = error;
            }
        }
        if attempt < SEGMENT_RETRIES {
            tokio::time::sleep(Duration::from_millis(300 * (attempt as u64 + 1))).await;
        }
    }
    Err(format!("分片 {start}-{end} 下载失败：{last_error}"))
}

#[allow(clippy::too_many_arguments)]
async fn stream_range(
    client: &reqwest::Client,
    url: &str,
    start: u64,
    end: u64,
    total: u64,
    downloaded: &AtomicU64,
    counted: &mut u64,
    progress: &Progress,
) -> Result<Vec<u8>, String> {
    let response = client
        .get(url)
        .header(reqwest::header::RANGE, format!("bytes={start}-{end}"))
        .send()
        .await
        .map_err(|error| format!("请求失败：{}", describe(&error)))?;
    if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(format!(
            "不支持分段下载（HTTP {}）",
            response.status().as_u16()
        ));
    }
    let expected = end - start + 1;
    let mut buffer = Vec::with_capacity(expected as usize);
    for_each_chunk(response, |chunk| {
        buffer.extend_from_slice(&chunk[..]);
        *counted += chunk.len() as u64;
        let done = downloaded.fetch_add(chunk.len() as u64, Ordering::Relaxed)
            + chunk.len() as u64;
        progress(done, Some(total));
    })
    .await?;
    if buffer.len() as u64 != expected {
        return Err(format!(
            "分段长度不符：期望 {expected} 字节，实得 {} 字节",
            buffer.len()
        ));
    }
    Ok(buffer)
}

async fn download_stream(
    client: &reqwest::Client,
    url: &str,
    total: Option<u64>,
    progress: &Progress,
) -> Result<Vec<u8>, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| format!("请求失败：{}", describe(&error)))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("返回 HTTP {}", status.as_u16()));
    }
    let total = total.or_else(|| response.content_length());
    let mut downloaded = 0u64;
    let mut buffer = Vec::with_capacity(total.unwrap_or_default() as usize);
    for_each_chunk(response, |chunk| {
        buffer.extend_from_slice(&chunk[..]);
        downloaded += chunk.len() as u64;
        progress(downloaded, total);
    })
    .await?;
    if let Some(total) = total {
        if buffer.len() as u64 != total {
            return Err(format!(
                "下载不完整：期望 {total} 字节，实得 {} 字节",
                buffer.len()
            ));
        }
    }
    Ok(buffer)
}

/// 逐块读取响应体，每块都套一层空闲超时。
///
/// 用 `timeout(IDLE_TIMEOUT, response.chunk())` 而不是给整个请求设总超时：慢速但仍在
/// 出数据的线路不该被杀，只有「连着 20 秒一个字都没收到」才算死。
async fn for_each_chunk(
    mut response: reqwest::Response,
    mut on_chunk: impl FnMut(&[u8]),
) -> Result<(), String> {
    loop {
        match tokio::time::timeout(IDLE_TIMEOUT, response.chunk()).await {
            Err(_) => return Err("连接空闲超时（20 秒没有收到数据）".to_string()),
            Ok(Err(error)) => return Err(format!("传输中断：{}", describe(&error))),
            Ok(Ok(None)) => return Ok(()),
            Ok(Ok(Some(chunk))) => on_chunk(&chunk),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_url_trims_trailing_slash_and_keeps_original_on_empty_prefix() {
        assert_eq!(
            mirror_url("https://github.com/o/r/releases/download/a.exe", "https://gh-proxy.com/"),
            "https://gh-proxy.com/https://github.com/o/r/releases/download/a.exe"
        );
        assert_eq!(
            mirror_url("https://github.com/o/r/a.exe", "  "),
            "https://github.com/o/r/a.exe"
        );
    }

    #[test]
    fn download_candidates_put_mirrors_first_and_direct_last() {
        let url = "https://github.com/o/r/releases/download/v1/a.exe";
        let candidates = download_candidates(url);
        assert_eq!(candidates.len(), MIRROR_PREFIXES.len() + 1);
        assert!(candidates[0].starts_with(MIRROR_PREFIXES[0]));
        assert_eq!(candidates.last().map(String::as_str), Some(url));
    }

    #[test]
    fn download_candidates_leave_third_party_sources_untouched() {
        let url = "https://example.com/updates/a.exe";
        assert_eq!(download_candidates(url), vec![url.to_string()]);
    }

    #[test]
    fn plan_segments_covers_every_byte_exactly_once() {
        let total = 8_815_103u64;
        let segments = plan_segments(total, SEGMENT_CONNECTIONS);
        assert_eq!(segments.len(), SEGMENT_CONNECTIONS);
        assert_eq!(segments[0].0, 0);
        assert_eq!(segments[segments.len() - 1].1, total - 1);
        for pair in segments.windows(2) {
            assert_eq!(pair[0].1 + 1, pair[1].0, "分片之间不能有缝隙或重叠");
        }
        let covered: u64 = segments.iter().map(|(start, end)| end - start + 1).sum();
        assert_eq!(covered, total);
    }

    #[test]
    fn plan_segments_handles_tiny_and_empty_inputs() {
        assert!(plan_segments(0, SEGMENT_CONNECTIONS).is_empty());
        assert!(plan_segments(1024, 0).is_empty());
        assert_eq!(plan_segments(3, SEGMENT_CONNECTIONS), vec![(0, 0), (1, 1), (2, 2)]);
    }

    #[test]
    fn parse_content_range_reads_total_length() {
        assert_eq!(parse_content_range("bytes 0-0/8815103"), Some(8_815_103));
        assert_eq!(parse_content_range("bytes 0-0/*"), None);
        assert_eq!(parse_content_range("garbage"), None);
    }

    /// 真签名必须验得过，动一个字节就必须失败 —— 这是「镜像可以不可信，但包不能」的底线。
    const FIXTURE_PAYLOAD: &str = "wb-switch update signature fixture\n";
    const FIXTURE_PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDM2ODhEOUNEMkMzMTJCRjIKUldUeUt6RXN6ZG1JTmtjUU11aGxKZG9hdnQvTVpLNWdvd09XVk1TSmpvRjdqcllLZHpDOWZMUloK";
    const FIXTURE_SIG: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVUeUt6RXN6ZG1JTnNWTjN1ZGRCcUJubG9pcW5DOE5xVVQzdElhUjRSZmhCcG1JaytlVmZTcXpLc05PNWROZGFJTjBwOWhXNW9aWnYrMkl2bWV3cVloKzFJblBxY2NSS2c0PQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzkzNzkyMzU1CWZpbGU6cGF5bG9hZC5iaW4KdDlEYlgrS21GQjFLakMvUTJINy9oU0RaRm1KelRXQUdFTGMxUzdYdGFCSlU1M2t2aWszWm5qUTVMKy9HQW4xVWpvYlFhdktxTU9yZzNSZWJaV2FWQlE9PQo=";

    #[test]
    fn verify_signature_accepts_matching_payload() {
        assert!(verify_signature(FIXTURE_PAYLOAD.as_bytes(), FIXTURE_SIG, FIXTURE_PUBKEY).is_ok());
    }

    #[test]
    fn verify_signature_rejects_tampered_payload() {
        let tampered = FIXTURE_PAYLOAD.replace("fixture", "tamper!");
        let error = verify_signature(tampered.as_bytes(), FIXTURE_SIG, FIXTURE_PUBKEY)
            .expect_err("改过的内容必须验签失败");
        assert!(error.contains("签名校验失败"), "{error}");
    }

    #[test]
    fn verify_signature_reports_broken_encoding() {
        let error = verify_signature(b"payload", "not-base64!!", FIXTURE_PUBKEY)
            .expect_err("非法 base64 必须报错");
        assert!(error.contains("编码无效"), "{error}");
    }

    // ---------------------------------------------------------------------
    // 分段下载端到端验证：本地起一个只认 Range 的迷你 HTTP 服务
    // ---------------------------------------------------------------------

    /// 迷你 HTTP 服务：只会「按 Range 回 206」和「整包回 200」两件事。
    ///
    /// 用它验证真实网络路径（探测 → 分段 → 拼装 → 长度校验），不依赖外网和 TLS，
    /// 沙箱里也能稳定跑。
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum RangeMode {
        /// 老实支持 Range。
        Honest,
        /// 完全无视 Range，永远整包 200。
        Ignore,
        /// 只在探测请求（bytes=0-0）上回 206，其余分段请求回 200 —— 模拟「说支持、
        /// 实际不支持」的镜像。
        ProbeOnly,
    }

    struct RangeServer {
        address: String,
        stop: Arc<std::sync::atomic::AtomicBool>,
        range_requests: Arc<AtomicU64>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl RangeServer {
        fn start(body: Vec<u8>, mode: RangeMode) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定端口失败");
            listener.set_nonblocking(true).expect("设置非阻塞失败");
            let address = format!("http://{}", listener.local_addr().expect("读取地址失败"));
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let range_requests = Arc::new(AtomicU64::new(0));
            let body = Arc::new(body);
            let handle = {
                let stop = Arc::clone(&stop);
                let range_requests = Arc::clone(&range_requests);
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        match listener.accept() {
                            Ok((mut stream, _)) => {
                                let body = Arc::clone(&body);
                                let range_requests = Arc::clone(&range_requests);
                                // 每个连接一个线程：分段是并发的，串行服务会把彼此堵住。
                                std::thread::spawn(move || {
                                    respond(&mut stream, &body, mode, &range_requests)
                                });
                            }
                            Err(error)
                                if error.kind() == std::io::ErrorKind::WouldBlock =>
                            {
                                std::thread::sleep(Duration::from_millis(2));
                            }
                            Err(_) => break,
                        }
                    }
                })
            };
            Self {
                address,
                stop,
                range_requests,
                handle: Some(handle),
            }
        }

        fn url(&self) -> String {
            format!("{}/package.exe", self.address)
        }

        fn range_requests(&self) -> u64 {
            self.range_requests.load(Ordering::Relaxed)
        }
    }

    impl Drop for RangeServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn respond(
        stream: &mut std::net::TcpStream,
        body: &[u8],
        mode: RangeMode,
        range_requests: &AtomicU64,
    ) {
        use std::io::{Read, Write};

        let mut buffer = [0u8; 4096];
        let read = stream.read(&mut buffer).unwrap_or(0);
        let request = String::from_utf8_lossy(&buffer[..read]).to_string();
        let range = match mode {
            RangeMode::Ignore => None,
            RangeMode::Honest => parse_request_range(&request),
            RangeMode::ProbeOnly => parse_request_range(&request)
                .filter(|(start, end)| *start == 0 && *end == 0),
        };
        let response = match range {
            Some((start, end)) => {
                range_requests.fetch_add(1, Ordering::Relaxed);
                let start = start as usize;
                let end = (end as usize).min(body.len().saturating_sub(1));
                let slice = &body[start..=end];
                let mut head = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                    slice.len(),
                    start,
                    end,
                    body.len()
                )
                .into_bytes();
                head.extend_from_slice(slice);
                head
            }
            None => {
                let mut head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                head.extend_from_slice(body);
                head
            }
        };
        let _ = stream.write_all(&response);
        let _ = stream.flush();
    }

    fn parse_request_range(request: &str) -> Option<(u64, u64)> {
        let line = request
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("range:"))?;
        let spec = line.split_once(':')?.1.trim().strip_prefix("bytes=")?;
        let (start, end) = spec.split_once('-')?;
        Some((start.trim().parse().ok()?, end.trim().parse().ok()?))
    }

    #[tokio::test]
    async fn fetch_uses_parallel_ranges_and_reassembles_exact_bytes() {
        let body: Vec<u8> = (0..3 * 1024 * 1024)
            .map(|index| (index % 251) as u8)
            .collect();
        let server = RangeServer::start(body.clone(), RangeMode::Honest);
        let progress: Progress = Arc::new(|_, _| {});
        let downloaded = fetch(&server.url(), None, progress)
            .await
            .expect("分段下载失败");
        assert_eq!(downloaded, body, "拼装结果必须与原始字节完全一致");
        assert!(
            server.range_requests() >= SEGMENT_CONNECTIONS as u64,
            "应当真的并发走了多段，实际只有 {} 段",
            server.range_requests()
        );
    }

    #[tokio::test]
    async fn fetch_falls_back_to_single_stream_when_server_ignores_range() {
        let body: Vec<u8> = (0..2 * 1024 * 1024)
            .map(|index| (index % 97) as u8)
            .collect();
        let server = RangeServer::start(body.clone(), RangeMode::Ignore);
        let progress: Progress = Arc::new(|_, _| {});
        let downloaded = fetch(&server.url(), None, progress)
            .await
            .expect("整包下载失败");
        assert_eq!(downloaded, body);
        assert_eq!(server.range_requests(), 0, "服务端不支持 Range 时不该尝试分段");
    }

    /// 镜像「探测时回 206、真分段时回 200」是常见坑：不能因此整条通道判死，
    /// 应该退回单连接把包拉完。
    #[tokio::test]
    async fn fetch_falls_back_to_single_stream_when_ranges_lie() {
        let body: Vec<u8> = (0..2 * 1024 * 1024)
            .map(|index| (index % 131) as u8)
            .collect();
        let server = RangeServer::start(body.clone(), RangeMode::ProbeOnly);
        let progress: Progress = Arc::new(|_, _| {});
        let downloaded = fetch(&server.url(), None, progress)
            .await
            .expect("分段失败后应该退回单连接");
        assert_eq!(downloaded, body);
    }
}
