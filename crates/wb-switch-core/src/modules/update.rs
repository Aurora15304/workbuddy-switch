//! 自动更新：检查公开 GitHub Releases 版本 + 更新源配置。
//!
//! 对照 server.py `load_github_config` / `save_github_config` /
//! `compare_versions` / `update_check`。下载安装走 tauri-plugin-updater（整包更新）。
//!
//! 版本检查不走 GitHub API（避免 60 次/小时/IP 限流）：
//! 1. 主端点：release 资产的 updater manifest（下载不计 API 配额）；
//! 2. 兜底端点：`/releases/latest` 的 302 `Location` 头解析 tag；
//! 3. 成功结果进程级缓存 6 小时，缓存命中不发网络请求。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::modules::config::{
    atomic_write, http_request_direct, http_request_raw, http_request_with_proxy, now_secs,
    store_dir,
};
use crate::modules::update_download::{is_mirror_url, mirror_url, MIRROR_PREFIXES};

/// 应用当前版本（来自 Cargo.toml package.version）。
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const GITHUB_OWNER: &str = "Binbin-miao";
pub const GITHUB_REPO: &str = "workbuddy-switch-binbinbin";

/// 成功结果缓存有效期（6 小时）。自动轮询（30 分钟）命中缓存，不发网络请求；
/// 设置页手动检查传 force=true 绕过缓存强制刷新。
const CACHE_TTL_SECS: i64 = 6 * 60 * 60;

/// 进程级内存缓存，只缓存 ok=true 的结果；失败不写缓存。
struct CachedCheck {
    checked_at: i64,
    value: Value,
}

static CACHE: Mutex<Option<CachedCheck>> = Mutex::new(None);

pub fn github_config_file() -> PathBuf {
    store_dir().join("github_config.json")
}

/// 读取更新源配置（兼容旧配置文件，但永不返回 token）。
pub fn load_github_config() -> Value {
    let mut owner = GITHUB_OWNER.to_string();
    let mut repo = GITHUB_REPO.to_string();
    let mut proxy = String::new();
    // 显式代理默认关闭：地址可以留着，但只有用户自己打开开关才生效。
    // 老配置没有这个键 → false，升级后不会继续把更新请求塞进一个可能已经失效的代理。
    let mut proxy_enabled = false;
    // 发现新版本后是否自动后台下载：默认开（用户的诉求就是"后台自动下"）。
    // 老配置没有这个键 → true，升级后立即可用；用户显式关掉才保留 false。
    let mut auto_download = true;
    // 用户明确跳过的版本号（如 "0.1.79"）；空 = 没有跳过任何版本。
    let mut skipped_version = String::new();
    let f = github_config_file();
    let mut should_normalize = false;
    if f.exists() {
        if let Ok(text) = std::fs::read_to_string(&f) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(value) = v.get("owner").and_then(|v| v.as_str()) {
                    if !value.trim().is_empty() {
                        owner = value.to_string();
                    }
                }
                if let Some(value) = v.get("repo").and_then(|v| v.as_str()) {
                    if !value.trim().is_empty() {
                        repo = value.to_string();
                    }
                }
                if let Some(value) = v.get("proxy").and_then(|v| v.as_str()) {
                    proxy = value.trim().to_string();
                }
                if let Some(value) = v.get("proxy_enabled").and_then(Value::as_bool) {
                    proxy_enabled = value;
                }
                if let Some(value) = v.get("auto_download").and_then(Value::as_bool) {
                    auto_download = value;
                }
                if let Some(value) = v.get("skipped_version").and_then(|v| v.as_str()) {
                    skipped_version = value.trim().to_string();
                }
                // 只在需要清理历史 token 时写盘；`proxy_enabled` 缺失按 false 处理即可，
                // 不在「读配置」时顺手改文件。
                should_normalize = v.get("token").is_some();
            }
        }
    }
    // 旧版本截图/配置曾使用 changexbc/wb-switch；迁移到实际公开仓库。
    if owner == "changexbc" && repo == "wb-switch" {
        repo = GITHUB_REPO.to_string();
        should_normalize = true;
    }
    // 归一化：空白版本号等于"没有跳过"，与 `set_skipped_version` 的写入口径一致。
    // 否则手改配置文件写 `"skipped_version": "  "` 会让跳过判断永远不命中。
    let skipped_version = skipped_version.trim().trim_start_matches('v').to_string();
    let normalized = json!({
        "owner": owner,
        "repo": repo,
        "proxy": proxy,
        "proxy_enabled": proxy_enabled,
        "auto_download": auto_download,
        "skipped_version": skipped_version,
    });
    if should_normalize {
        let _ = atomic_write(
            &f,
            &serde_json::to_string_pretty(&normalized).unwrap_or_default(),
        );
    }
    normalized
}

/// 生效的显式代理：只有用户显式打开开关、且地址非空时才返回。
///
/// 所有「自动发生」的更新请求都必须走这里，别直接读 `proxy` 字段——否则一个
/// 已经失效的历史地址会把清单和镜像一起拖死。
pub fn effective_proxy() -> Option<String> {
    effective_proxy_from(&load_github_config())
}

/// 发现新版本后是否自动后台下载。缺省 `true`：用户要的就是"后台自动下"。
pub fn auto_download_enabled() -> bool {
    load_github_config()
        .get("auto_download")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

/// 用户明确跳过的版本号（已去空白并去掉前导 `v`）；`None` = 没有跳过任何版本。
pub fn skipped_version() -> Option<String> {
    load_github_config()
        .get("skipped_version")
        .and_then(|value| value.as_str())
        .map(|value| value.trim().trim_start_matches('v').to_string())
        .filter(|value| !value.is_empty())
}

/// 记录或清除「已跳过版本」。PATCH 语义：只改这一个键，不碰其余配置。
///
/// 传 `None`（或空白）即清除跳过标记，下次发现同一版本会重新提示。
pub fn set_skipped_version(version: Option<&str>) -> std::io::Result<()> {
    let mut cfg = load_github_config();
    let value = version
        .map(|value| value.trim().trim_start_matches('v'))
        .filter(|value| !value.is_empty())
        .unwrap_or("");
    cfg["skipped_version"] = json!(value);
    save_github_config(&cfg)
}

/// 开关「自动后台下载」。PATCH 语义：只改这一个键。
pub fn set_auto_download(enabled: bool) -> std::io::Result<()> {
    let mut cfg = load_github_config();
    cfg["auto_download"] = json!(enabled);
    save_github_config(&cfg)
}

/// 同一份配置已经在手上时用它，别为了取代理再读一次磁盘。
fn effective_proxy_from(cfg: &Value) -> Option<String> {
    let enabled = cfg.get("proxy_enabled").and_then(Value::as_bool) == Some(true);
    let proxy = cfg.get("proxy").and_then(Value::as_str).unwrap_or("");
    enabled_proxy(enabled, proxy)
}

/// 纯判定：开关打开且地址非空才算生效。
fn enabled_proxy(enabled: bool, proxy: &str) -> Option<String> {
    if !enabled {
        return None;
    }
    let proxy = proxy.trim();
    if proxy.is_empty() {
        return None;
    }
    Some(proxy.to_string())
}

/// 保存更新源配置；公开仓库不需要也不保存 GitHub token。
pub fn save_github_config(cfg: &Value) -> std::io::Result<()> {
    let owner = cfg
        .get("owner")
        .and_then(|v| v.as_str())
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(GITHUB_OWNER);
    let repo = cfg
        .get("repo")
        .and_then(|v| v.as_str())
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(GITHUB_REPO);
    let proxy = cfg
        .get("proxy")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    // 没带这个键（老客户端只发 owner/repo/proxy）时保留磁盘上的状态，
    // 不能按 PATCH 语义把用户已经打开的开关悄悄关掉。
    let disk = load_github_config();
    let proxy_enabled = match cfg.get("proxy_enabled").and_then(Value::as_bool) {
        Some(value) => value,
        None => disk.get("proxy_enabled").and_then(Value::as_bool) == Some(true),
    };
    // 同理：新增的两个键也必须是 PATCH 语义——只改调用方显式带来的那个键，
    // 否则「保存代理地址」会顺手把"自动下载"和"已跳过版本"重置掉。
    let auto_download = match cfg.get("auto_download").and_then(Value::as_bool) {
        Some(value) => value,
        None => disk
            .get("auto_download")
            .and_then(Value::as_bool)
            .unwrap_or(true),
    };
    let skipped_version = match cfg.get("skipped_version").and_then(|v| v.as_str()) {
        Some(value) => value.trim().to_string(),
        None => disk
            .get("skipped_version")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string(),
    };
    let clean = json!({
        "owner": owner,
        "repo": repo,
        "proxy": proxy,
        "proxy_enabled": proxy_enabled,
        "auto_download": auto_download,
        "skipped_version": skipped_version,
    });
    std::fs::create_dir_all(store_dir())?;
    atomic_write(
        &github_config_file(),
        &serde_json::to_string_pretty(&clean).unwrap_or_default(),
    )
}

fn version_tuple(v: &str) -> Vec<i64> {
    v.trim_start_matches('v')
        .split('.')
        .filter_map(|x| x.parse::<i64>().ok())
        .collect()
}

/// 版本比较：a > b 返回 1，a < b 返回 -1，相等返回 0。
pub fn compare_versions(a: &str, b: &str) -> i64 {
    let ta = version_tuple(a);
    let tb = version_tuple(b);
    for i in 0..ta.len().max(tb.len()) {
        let x = ta.get(i).copied().unwrap_or(0);
        let y = tb.get(i).copied().unwrap_or(0);
        if x != y {
            return if x > y { 1 } else { -1 };
        }
    }
    0
}

/// updater manifest 候选 URL（按优先级）。
///
/// 1. 合并后的 `latest.json`（含各平台）；
/// 2. 当前系统的 `latest-<os>-<arch>.json`；
/// 3. 兼容旧 Windows 安装包：它们仍请求 `latest-macos-<arch>.json`。
///
/// 每个地址都先走内置镜像、再走直连：manifest 只有 1 KB 出头，但 github.com 被墙时
/// 直连会直接失败，用户连「有新版本」都发现不了。镜像报错就换下一个，不会卡住。
pub fn updater_manifest_urls(owner: &str, repo: &str, os: &str, arch: &str) -> Vec<String> {
    let os_slug = match os {
        "macos" | "darwin" => "macos",
        other => other,
    };
    let mut github_urls = vec![format!(
        "https://github.com/{owner}/{repo}/releases/latest/download/latest.json"
    )];
    github_urls.push(format!(
        "https://github.com/{owner}/{repo}/releases/latest/download/latest-{os_slug}-{arch}.json"
    ));
    if os_slug != "macos" {
        github_urls.push(format!(
            "https://github.com/{owner}/{repo}/releases/latest/download/latest-macos-{arch}.json"
        ));
    }
    github_urls.dedup();

    let mut urls = Vec::with_capacity(github_urls.len() * (MIRROR_PREFIXES.len() + 1));
    for url in &github_urls {
        for prefix in MIRROR_PREFIXES {
            urls.push(mirror_url(url, prefix));
        }
        urls.push(url.clone());
    }
    urls.dedup();
    urls
}

/// 主端点：拉取 updater manifest。成功返回解析后的 JSON（含 version / pub_date），
/// 失败返回可读错误信息。
async fn fetch_manifest_version(
    owner: &str,
    repo: &str,
    proxy: Option<&str>,
) -> Result<Value, String> {
    let mut headers = HashMap::new();
    headers.insert("Accept".to_string(), "application/json".to_string());
    headers.insert("User-Agent".to_string(), "wb-switch".to_string());
    let mut last_err = "更新清单解析失败".to_string();
    for url in updater_manifest_urls(owner, repo, std::env::consts::OS, std::env::consts::ARCH) {
        // 镜像直连：系统代理会把镜像也带去海外节点，反而更慢甚至不通。
        let resp = if is_mirror_url(&url) {
            http_request_direct(&url, "GET", None, Some(&headers)).await
        } else {
            http_request_with_proxy(&url, "GET", None, Some(&headers), proxy).await
        };
        let version = resp.get("version").and_then(|v| v.as_str()).unwrap_or("");
        if !version.trim().is_empty() {
            return Ok(resp);
        }
        last_err = resp
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("更新清单解析失败")
            .to_string();
    }
    Err(last_err)
}

/// 兜底端点：请求 `/releases/latest`，读 302 `Location` 头（形如
/// `.../releases/tag/v0.1.13`）解析 tag。不跟随重定向，避免拉到 HTML 页面。
/// 成功返回 tag，失败返回可读错误 + code（状态码或 -1）。
async fn fetch_latest_tag(
    owner: &str,
    repo: &str,
    proxy: Option<&str>,
) -> Result<String, (String, i64)> {
    let url = format!("https://github.com/{owner}/{repo}/releases/latest");
    let mut headers = HashMap::new();
    headers.insert("Accept".to_string(), "text/html".to_string());
    headers.insert("User-Agent".to_string(), "wb-switch".to_string());
    let (status, resp_headers, body) =
        http_request_raw(&url, "GET", None, Some(&headers), proxy, false).await;

    if status == 0 {
        let msg = if body.trim().is_empty() {
            "网络请求失败".to_string()
        } else {
            body
        };
        return Err((msg, -1));
    }
    if status == 404 {
        // 无正式 release 或仓库不存在。
        return Err(("未找到可用的发布版本".to_string(), 404));
    }
    let location = resp_headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("location"))
        .map(|(_, v)| v.clone());
    let location = match location {
        Some(location) => location,
        None => {
            return Err((
                format!("无法获取发布页跳转地址（HTTP {status}）"),
                status as i64,
            ))
        }
    };
    let tag = location.rsplit('/').next().unwrap_or("").trim().to_string();
    if tag.is_empty() || !location.contains("/releases/tag/") {
        return Err(("无法解析发布版本标签".to_string(), -1));
    }
    Ok(tag)
}

/// 查询最新 Release，与本地版本对比。对照 server.py `update_check`。
///
/// `force=true` 绕过缓存强制刷新（设置页手动检查）；否则 6 小时内成功结果直接返回，
/// 不发网络请求。主端点 manifest 失败时自动走 302 兜底；两个端点都失败返回可读错误。
pub async fn update_check(proxy: Option<&str>, force: bool) -> Value {
    if !force {
        if let Some(cached) = CACHE.lock().unwrap().as_ref() {
            if now_secs() - cached.checked_at < CACHE_TTL_SECS {
                return cached.value.clone();
            }
        }
    }

    let cfg = load_github_config();
    let owner = cfg
        .get("owner")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let repo = cfg
        .get("repo")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // 已保存但未开启的代理一律不参与：地址失效时不该拖死整条更新链。
    let configured_proxy = effective_proxy_from(&cfg);
    let proxy = proxy.or(configured_proxy.as_deref());
    let release_url = format!("https://github.com/{owner}/{repo}/releases/latest");
    let current = APP_VERSION.to_string();

    // 主端点：updater manifest（release 资产下载，不计 GitHub API 配额）。
    if let Ok(manifest) = fetch_manifest_version(&owner, &repo, proxy).await {
        let version = manifest
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let latest = version.strip_prefix('v').unwrap_or(version).to_string();
        let tag = format!("v{latest}");
        let release_name = manifest
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(&tag)
            .to_string();
        let published_at = manifest
            .get("pub_date")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let value = json!({
            "ok": true,
            "current": current,
            "latest": latest,
            "latestTag": tag,
            "hasUpdate": compare_versions(&latest, &current) > 0,
            "releaseName": release_name,
            "releaseUrl": release_url,
            "publishedAt": published_at,
            "checkedAt": now_secs(),
        });
        *CACHE.lock().unwrap() = Some(CachedCheck {
            checked_at: now_secs(),
            value: value.clone(),
        });
        return value;
    }

    // 兜底端点：`/releases/latest` 的 302 Location 头解析 tag（仅 manifest 失败时）。
    match fetch_latest_tag(&owner, &repo, proxy).await {
        Ok(tag) => {
            let latest = tag.strip_prefix('v').unwrap_or(&tag).to_string();
            let value = json!({
                "ok": true,
                "current": current,
                "latest": latest,
                "latestTag": tag,
                "hasUpdate": compare_versions(&latest, &current) > 0,
                "releaseName": tag.clone(),
                "releaseUrl": release_url,
                "checkedAt": now_secs(),
            });
            *CACHE.lock().unwrap() = Some(CachedCheck {
                checked_at: now_secs(),
                value: value.clone(),
            });
            value
        }
        Err((msg, code)) => failure_value(proxy, &msg, code, &release_url),
    }
}

/// 传输层失败的特征串（reqwest / rustls 的英文原文）。
///
/// 只认这些标记：404、清单解析失败、标签解析失败都是业务错误，必须原样透出，
/// 否则会把「仓库没有发布」误报成「你断网了」。
const TRANSPORT_MARKERS: [&str; 8] = [
    "error sending request",
    "connection refused",
    "connection reset",
    "failed to lookup address",
    "dns error",
    "tls handshake",
    "timed out",
    "网络请求失败",
];

fn is_transport_error(raw: &str) -> bool {
    let lower = raw.to_ascii_lowercase();
    TRANSPORT_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

/// 代理地址脱敏：只保留 `scheme://host:port`，丢掉 userinfo 与路径。
///
/// 这段文案会进 toast、托盘 tooltip 和更新弹窗——投屏或截图时会连带泄露
/// 代理账号密码（`http://user:pass@host:port` 是合法配置）。
fn mask_proxy(raw: &str) -> String {
    let (scheme, rest) = match raw.split_once("://") {
        Some((scheme, rest)) => (Some(scheme), rest),
        None => (None, raw),
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    let host = authority.rsplit('@').next().unwrap_or(authority);
    match scheme {
        Some(scheme) => format!("{scheme}://{host}"),
        None => host.to_string(),
    }
}

/// 把英文传输错误翻译成用户能照做的提示；业务错误返回 `None` 表示不改写。
///
/// `message` 走界面 toast 与托盘 tooltip，必须短；原始英文错误留在 `error`
/// 字段，供日志与排障复制。
fn transport_hint(proxy: Option<&str>, raw: &str) -> Option<String> {
    if !is_transport_error(raw) {
        return None;
    }
    Some(match proxy {
        Some(proxy) => format!(
            "连不上 GitHub：代理（{}）未生效。请确认代理已启动、端口正确，或到「设置 → 更新」关掉代理再试（镜像下载不需要代理）。",
            mask_proxy(proxy)
        ),
        None => "连不上 GitHub（网络不通或被拦截）。镜像也不通时，可到「设置 → 自动更新」打开「启用更新代理」并填入地址后重试。".to_string(),
    })
}

/// 检查失败的结果对象。传输层错误换成可操作中文，其余保持「原文 + code」。
fn failure_value(proxy: Option<&str>, raw: &str, code: i64, release_url: &str) -> Value {
    match transport_hint(proxy, raw) {
        Some(hint) => json!({
            "ok": false,
            "error": raw,
            "message": hint,
            "releaseUrl": release_url,
        }),
        None => json!({
            "ok": false,
            "error": raw,
            "message": format!("{raw}（code={code}）"),
            "releaseUrl": release_url,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updater_manifest_urls_macos_skips_duplicate_fallback() {
        let urls = updater_manifest_urls("Binbin-miao", "workbuddy-switch-binbinbin", "macos", "aarch64");
        // 两个 github 地址（latest.json + latest-macos-aarch64.json），各自 2 镜像 + 1 直连。
        assert_eq!(urls.len(), 6);
        assert_eq!(
            urls.iter().filter(|url| url.ends_with("/latest.json")).count(),
            3
        );
        assert_eq!(
            urls.iter()
                .filter(|url| url.ends_with("/latest-macos-aarch64.json"))
                .count(),
            3
        );
        // 直连必须排在镜像之后，且是最后一个候选。
        assert_eq!(
            urls.last().map(String::as_str),
            Some("https://github.com/Binbin-miao/workbuddy-switch-binbinbin/releases/latest/download/latest-macos-aarch64.json")
        );
    }

    #[test]
    fn updater_manifest_urls_windows_keeps_macos_compat() {
        let urls = updater_manifest_urls("Binbin-miao", "workbuddy-switch-binbinbin", "windows", "x86_64");
        // 三个 github 地址（latest / windows / macos 兼容），各自 2 镜像 + 1 直连。
        assert_eq!(urls.len(), 9);
        assert_eq!(
            urls.iter()
                .filter(|url| url.ends_with("/latest-macos-x86_64.json"))
                .count(),
            3
        );
        assert_eq!(
            urls[0],
            format!(
                "{}/https://github.com/Binbin-miao/workbuddy-switch-binbinbin/releases/latest/download/latest.json",
                MIRROR_PREFIXES[0]
            )
        );
        assert_eq!(
            urls[2],
            "https://github.com/Binbin-miao/workbuddy-switch-binbinbin/releases/latest/download/latest.json"
        );
    }

    #[test]
    fn compare_versions_orders_semver() {
        assert_eq!(compare_versions("0.1.18", "0.1.18"), 0);
        assert_eq!(compare_versions("0.1.19", "0.1.18"), 1);
        assert_eq!(compare_versions("v0.1.17", "0.1.18"), -1);
    }

    /// 传输层错误要变成可操作中文：配了代理点名代理地址，没配就指向设置页；
    /// 原始英文错误必须留在 `error` 里，否则排障时无从下手。
    #[test]
    fn transport_error_becomes_actionable_hint() {
        let raw = "error sending request for url (https://github.com/o/r/releases/latest)";

        let with_proxy = failure_value(Some("http://127.0.0.1:7897"), raw, -1, "https://r");
        let message = with_proxy["message"].as_str().unwrap();
        assert!(message.contains("http://127.0.0.1:7897"), "{message}");
        assert!(message.contains("代理"), "{message}");
        assert_eq!(with_proxy["error"].as_str(), Some(raw));

        let without_proxy = failure_value(None, raw, -1, "https://r");
        let message = without_proxy["message"].as_str().unwrap();
        assert!(message.contains("更新代理"), "{message}");
        assert!(
            !message.contains("code=-1"),
            "网络错误不该再把 code 抛给用户"
        );
    }

    #[test]
    fn enabled_proxy_requires_both_switch_and_address() {
        assert_eq!(enabled_proxy(false, "http://127.0.0.1:7897"), None);
        assert_eq!(enabled_proxy(true, "   "), None);
        assert_eq!(
            enabled_proxy(true, " http://127.0.0.1:7897 ").as_deref(),
            Some("http://127.0.0.1:7897")
        );
    }

    /// 代理账号密码不能进 UI 文案：这段文字会出现在 toast、托盘和截图里。
    #[test]
    fn proxy_credentials_never_reach_the_hint() {
        assert_eq!(
            mask_proxy("http://alice:s3cret@127.0.0.1:7897"),
            "http://127.0.0.1:7897"
        );
        assert_eq!(mask_proxy("127.0.0.1:7897"), "127.0.0.1:7897");
        assert_eq!(
            mask_proxy("socks5://alice:s3cret@proxy.local:1080/"),
            "socks5://proxy.local:1080"
        );

        let raw = "error sending request for url (https://github.com/o/r/releases/latest)";
        let value = failure_value(
            Some("http://alice:s3cret@127.0.0.1:7897"),
            raw,
            -1,
            "https://r",
        );
        let message = value["message"].as_str().unwrap();
        assert!(message.contains("127.0.0.1:7897"), "{message}");
        assert!(!message.contains("s3cret"), "凭据泄露：{message}");
        assert!(!message.contains("alice"), "用户名泄露：{message}");
    }

    /// 404 / 标签解析失败是业务错误，不能被改写成「你断网了」。
    #[test]
    fn business_error_keeps_original_text() {
        let not_found = failure_value(None, "未找到可用的发布版本", 404, "https://r");
        assert_eq!(
            not_found["message"].as_str(),
            Some("未找到可用的发布版本（code=404）")
        );
        assert!(transport_hint(None, "无法解析发布版本标签").is_none());
        assert!(transport_hint(None, "更新清单解析失败").is_none());
    }
}
