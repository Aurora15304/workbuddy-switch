//! 纯压缩 / 解压：只做「字节 ↔ zip 条目」，不含任何业务分组知识。
//!
//! 为什么单独一层：
//! - 条目名校验（zip slip、盘符、反斜杠）与限额是安全边界，必须集中一处，
//!   任何调用方都无法绕过；
//! - 业务侧只关心「名字 → 字节」，换压缩实现（或给单条写入加自定义头）不必
//!   改分组代码。
//!
//! 解压一律读进内存后再交给调用方，绝不按 entry 名拼路径写盘：写盘目标由
//! 分组白名单决定（见 `group::relative_target`），条目名只用于匹配清单。

use std::collections::BTreeMap;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

/// 解压总量上限：整包备份是配置与日志，正常远低于该值；超限说明不是普通备份。
///
/// 与宿主对齐：webui 的 `ARCHIVE_BODY_LIMIT` 是 96MB（base64 膨胀 4/3，
/// 64MB × 4/3 ≈ 85.3MB + JSON 余量）。若这里调到 64MB 以上、或宿主把 body 上限
/// 降回 32MB，就会出现「导出成功、同一个包导不回来」（先被 axum 413，前端只看到
/// 「导入失败」）。两边必须一起看。
pub(crate) const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
/// 单文件上限：防止一个超大条目把内存打爆。
pub(crate) const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// 条目数上限：防止「几万个小文件」拖垮备份/还原。
pub(crate) const MAX_ENTRIES: usize = 5000;
/// manifest 体积上限：它只记录分组统计，超过 1 MB 必然是伪造/损坏。
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

/// 待写入 zip 的一个条目：要么是磁盘上的文件，要么是内存里的字节（manifest）。
#[derive(Debug, Clone)]
pub(crate) enum ZipPayload {
    /// 直接拷贝磁盘文件，避免导出时把大文件整体读进内存。
    File(PathBuf),
    Bytes(Vec<u8>),
}

/// 校验 zip 内的相对路径名，非法即拒。
///
/// 拒绝的形态都是「解压时可能写到目标目录之外」或「跨平台会变成绝对路径」：
/// - 空名、目录分隔符结尾（目录条目由调用方先行跳过）；
/// - 反斜杠（Windows 上等价于分隔符，Unix 上又是合法文件名字符，语义不一致）；
/// - 以 `/`、`\` 开头，或 `X:` 盘符前缀（绝对路径）；
/// - 任何一段为 `.`、`..` 或空段（`a//b`）；
/// - 控制字符（会让错误信息与日志无法阅读）。
pub(crate) fn validate_entry_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("归档中存在空的条目名".to_string());
    }
    if name.chars().any(|c| c.is_control()) {
        return Err(format!("归档条目名含控制字符：{name:?}"));
    }
    if name.contains('\\') {
        return Err(format!("归档条目名不允许反斜杠：{name}"));
    }
    if name.starts_with('/') {
        return Err(format!("归档条目名不允许绝对路径：{name}"));
    }
    // 盘符：`C:` / `c:`；Windows 上 Path::join 会把它当绝对路径。
    let bytes = name.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Err(format!("归档条目名不允许盘符：{name}"));
    }
    for part in name.split('/') {
        if part.is_empty() {
            return Err(format!("归档条目名含空路径段：{name}"));
        }
        if part == "." || part == ".." {
            return Err(format!("归档条目名不允许相对跳转：{name}"));
        }
    }
    Ok(())
}

/// 写 zip：先写 `dest`，调用方负责「临时文件 + rename」的原子替换。
///
/// 所有条目名在写入前统一校验，避免我们自己生成出无法再读回的归档。
pub(crate) fn write_archive(
    dest: &Path,
    entries: &[(String, ZipPayload)],
) -> Result<(), String> {
    if entries.len() > MAX_ENTRIES {
        return Err(format!(
            "待导出条目过多（{} 个，上限 {MAX_ENTRIES}），请减少分组后再试",
            entries.len()
        ));
    }
    let file = std::fs::File::create(dest)
        .map_err(|e| format!("创建归档文件失败（{}）：{e}", dest.display()))?;
    let mut writer = ZipWriter::new(BufWriter::new(file));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    let mut total: u64 = 0;
    for (name, payload) in entries {
        validate_entry_name(name)?;
        // 与解压侧同一套限额：导出时不拦住，就会产出自己都读不回的归档。
        let declared = match payload {
            ZipPayload::Bytes(bytes) => bytes.len() as u64,
            ZipPayload::File(source) => std::fs::metadata(source)
                .map_err(|e| format!("读取待导出文件失败（{}）：{e}", source.display()))?
                .len(),
        };
        if declared > MAX_FILE_BYTES {
            return Err(format!(
                "待导出条目 {name} 过大（{declared} 字节，单文件上限 {MAX_FILE_BYTES}），请先清理或取消勾选对应分组"
            ));
        }
        total = total.saturating_add(declared);
        if total > MAX_TOTAL_BYTES {
            return Err(format!(
                "待导出数据总量超过上限（{MAX_TOTAL_BYTES} 字节），请取消勾选部分分组后再试"
            ));
        }
        writer
            .start_file(name.clone(), options)
            .map_err(|e| format!("写入归档条目 {name} 失败：{e}"))?;
        match payload {
            ZipPayload::Bytes(bytes) => {
                writer
                    .write_all(bytes)
                    .map_err(|e| format!("写入归档条目 {name} 失败：{e}"))?;
            }
            ZipPayload::File(source) => {
                let mut input = std::fs::File::open(source)
                    .map_err(|e| format!("读取待导出文件失败（{}）：{e}", source.display()))?;
                std::io::copy(&mut input, &mut writer)
                    .map_err(|e| format!("写入归档条目 {name} 失败：{e}"))?;
            }
        }
    }

    let mut buffered = writer
        .finish()
        .map_err(|e| format!("归档收尾失败：{e}"))?;
    buffered
        .flush()
        .map_err(|e| format!("归档刷盘失败：{e}"))?;
    // sync_all 是「导出成功」的持久化证明：rename 之前崩溃则只剩 .tmp 残留。
    let file = buffered
        .into_inner()
        .map_err(|e| format!("归档刷盘失败：{}", e.into_error()))?;
    file.sync_all()
        .map_err(|e| format!("归档同步失败：{e}"))?;
    Ok(())
}

/// 读整包：返回「条目名 → 字节」，并对每个条目做安全与限额校验。
///
/// 目录条目（以 `/` 结尾）直接跳过：我们导出的归档不含目录条目，出现也不承载数据。
pub(crate) fn read_archive(path: &Path) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let file = std::fs::File::open(path)
        .map_err(|e| format!("打开归档失败（{}）：{e}", path.display()))?;
    let mut archive = ZipArchive::new(BufReader::new(file))
        .map_err(|e| format!("归档格式无法识别（{}）：{e}", path.display()))?;

    if archive.len() > MAX_ENTRIES {
        return Err(format!(
            "归档条目过多（{} 个，上限 {MAX_ENTRIES}）",
            archive.len()
        ));
    }

    let mut out: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut total: u64 = 0;
    // 声明大小可以造假：再用实际读到的字节复核一次总量，否则「每个条目都谎报 1 KB」
    // 的 zip 能绕过总量上限把内存打爆。
    let mut actual_total: u64 = 0;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| format!("读取归档第 {index} 个条目失败：{e}"))?;
        let name = entry.name().to_string();
        if name.ends_with('/') {
            continue;
        }
        validate_entry_name(&name)?;

        // 先用声明的大小快速失败，再按实际读到的字节复核：声明可为假，实际不会。
        let declared = entry.size();
        if declared > MAX_FILE_BYTES {
            return Err(format!(
                "归档条目 {name} 过大（{declared} 字节，单文件上限 {MAX_FILE_BYTES}）"
            ));
        }
        total = total.saturating_add(declared);
        if total > MAX_TOTAL_BYTES {
            return Err(format!(
                "归档解压总量超过上限（{MAX_TOTAL_BYTES} 字节）"
            ));
        }

        let mut bytes = Vec::new();
        // take 再多读 1 字节：声明大小造假时也能发现「实际更大」。
        entry
            .by_ref()
            .take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("读取归档条目 {name} 失败：{e}"))?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(format!(
                "归档条目 {name} 实际大小超过单文件上限（{MAX_FILE_BYTES} 字节）"
            ));
        }
        actual_total = actual_total.saturating_add(bytes.len() as u64);
        if actual_total > MAX_TOTAL_BYTES {
            return Err(format!(
                "归档实际解压总量超过上限（{} 字节）",
                MAX_TOTAL_BYTES
            ));
        }
        if out.insert(name.clone(), bytes).is_some() {
            return Err(format!("归档中存在重复条目：{name}"));
        }
    }
    Ok(out)
}

/// 只读一个条目（`inspect` 用）：不碰其它条目，也不写盘。
pub(crate) fn read_entry(path: &Path, name: &str) -> Result<Vec<u8>, String> {
    validate_entry_name(name)?;
    let file = std::fs::File::open(path)
        .map_err(|e| format!("打开归档失败（{}）：{e}", path.display()))?;
    let mut archive = ZipArchive::new(BufReader::new(file))
        .map_err(|e| format!("归档格式无法识别（{}）：{e}", path.display()))?;
    let mut entry = archive
        .by_name(name)
        .map_err(|_| format!("归档缺少 {name}，不是本工具导出的备份"))?;
    if entry.size() > MAX_MANIFEST_BYTES {
        return Err(format!("归档条目 {name} 异常过大"));
    }
    let mut bytes = Vec::new();
    entry
        .by_ref()
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("读取归档条目 {name} 失败：{e}"))?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(format!("归档条目 {name} 异常过大"));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wb-switch-archive-zip-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn entry_names_reject_traversal_and_absolute_forms() {
        for bad in [
            "../evil",
            "a/../../evil",
            "/abs/path.json",
            "\\abs",
            "C:\\x",
            "c:/x",
            "a\\b",
            "a//b",
            "./a",
            "",
            "a/./b",
        ] {
            assert!(
                validate_entry_name(bad).is_err(),
                "应拒绝非法条目名：{bad:?}"
            );
        }
        for good in [
            "accounts/accounts.json",
            "session-links/baselines/ab.json",
            "memory/user-1-personal/memory/a.md",
        ] {
            assert!(
                validate_entry_name(good).is_ok(),
                "应接受合法条目名：{good:?}"
            );
        }
    }

    #[test]
    fn round_trip_bytes_and_files() {
        let dir = temp_dir("roundtrip");
        let source = dir.join("source.txt");
        std::fs::write(&source, b"file-payload").unwrap();
        let archive = dir.join("out.zip");
        write_archive(
            &archive,
            &[
                (
                    "settings/a.json".to_string(),
                    ZipPayload::Bytes(br#"{"a":1}"#.to_vec()),
                ),
                ("logs/b.json".to_string(), ZipPayload::File(source.clone())),
            ],
        )
        .unwrap();

        let read = read_archive(&archive).unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!(read["settings/a.json"], br#"{"a":1}"#);
        assert_eq!(read["logs/b.json"], b"file-payload");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_rejects_traversal_entries_from_foreign_archives() {
        let dir = temp_dir("evil");
        let archive = dir.join("evil.zip");
        // 手动构造：真实恶意归档不受我们写出侧的校验保护。
        {
            let file = std::fs::File::create(&archive).unwrap();
            let mut writer = ZipWriter::new(BufWriter::new(file));
            let options =
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
            for name in ["../evil.txt", "/abs.txt", "C:\\x.txt"] {
                writer.start_file(name, options).unwrap();
                writer.write_all(b"boom").unwrap();
            }
            writer.finish().unwrap();
        }
        let err = read_archive(&archive).unwrap_err();
        assert!(
            err.contains("相对跳转") || err.contains("绝对路径") || err.contains("盘符"),
            "应指明拒绝原因：{err}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_enforces_limits() {
        let dir = temp_dir("limit");
        let archive = dir.join("big.zip");
        {
            let file = std::fs::File::create(&archive).unwrap();
            let mut writer = ZipWriter::new(BufWriter::new(file));
            let options =
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
            writer.start_file("big.bin", options).unwrap();
            // 17 MB 全零：压缩后很小，但解压后超过单文件上限。
            let chunk = vec![0u8; 1024 * 1024];
            for _ in 0..17 {
                writer.write_all(&chunk).unwrap();
            }
            writer.finish().unwrap();
        }
        let err = read_archive(&archive).unwrap_err();
        assert!(err.contains("上限"), "应命中限额：{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_entry_only_touches_requested_name() {
        let dir = temp_dir("entry");
        let archive = dir.join("a.zip");
        write_archive(
            &archive,
            &[("m.json".to_string(), ZipPayload::Bytes(b"{}".to_vec()))],
        )
        .unwrap();
        assert_eq!(read_entry(&archive, "m.json").unwrap(), b"{}");
        assert!(read_entry(&archive, "missing.json").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
