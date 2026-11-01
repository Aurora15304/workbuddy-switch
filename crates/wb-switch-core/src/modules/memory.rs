//! 跨账号「工作记忆」合并与分发。
//!
//! WorkBuddy 把每个登录账号的人格与长期记忆落在
//! `{home}/.workbuddy/user-{uid}-personal/` 下的 Markdown 文件里：`MEMORY.md` 是长期记忆，
//! `SOUL.md` / `IDENTITY.md` / `USER.md` 是人格与用户档案。切号后目标账号读的是自己 uid
//! 的目录，于是「换账号 = 换一个人」。本模块把多个账号的记忆合成一份，再写回每一个账号。
//!
//! 三条硬约束：
//! 1. **只增不减**：合并只做去重，不删除任何一段原文；目标侧原有文件先备份成
//!    `<name>.bak-<毫秒时间戳>`，随时可回滚。
//! 2. **人格不合并**：`SOUL/IDENTITY/USER` 参与合并会生成互相矛盾的人格设定，因此只取
//!    「最完整的一份」，且仅在目标账号**缺少该文件**时写入，绝不覆盖已有设定。
//! 3. **内容一致不写**：合并结果与目标现有内容完全相同则跳过，不制造无意义备份。
//!
//! 合成规则（确定性、可解释）：以 `## ` 二级标题切章节；章节按标题归组并保持首次出现
//! 的顺序；章节正文按空行切段、归一化空白后去重。首个 `## ` 之前的内容作为前言同样去重。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::modules::config::{atomic_write, home_dir, now_ms};

/// 长期记忆文件名：唯一参与合并的文件。
const MEMORY_FILE: &str = "MEMORY.md";
/// 人格与用户档案：只取一份，仅在缺失时补写。
const PERSONA_FILES: [&str; 3] = ["SOUL.md", "IDENTITY.md", "USER.md"];
/// WorkBuddy 数据根下的账号目录前缀。
const PERSONAL_PREFIX: &str = "user-";
const PERSONAL_SUFFIX: &str = "-personal";

/// WorkBuddy 数据根（`{home}/.workbuddy`）。
pub fn data_root() -> PathBuf {
    home_dir().join(".workbuddy")
}

/// 指定 uid 的人格目录：`{root}/user-{uid}-personal`。
pub fn personal_dir_at(root: &Path, uid: &str) -> PathBuf {
    root.join(format!("{PERSONAL_PREFIX}{uid}{PERSONAL_SUFFIX}"))
}

/// 把本机所有账号的记忆合成一份并分发回去。
///
/// `accounts` 是 `[{"uid": "...", "label": "..."}]` 的形状（调用方从账号库投影），
/// 缺 uid 的账号会被跳过——uid 是目录名的唯一依据，猜不出来。
pub fn merge_and_distribute(accounts: &[(String, String)]) -> Value {
    merge_and_distribute_at(&data_root(), accounts)
}

fn merge_and_distribute_at(root: &Path, accounts: &[(String, String)]) -> Value {
    let mut sources: Vec<MemorySource> = Vec::new();
    let mut errors: Vec<Value> = Vec::new();
    for (uid, label) in accounts {
        if uid.trim().is_empty() {
            errors.push(json!({ "label": label, "error": "账号缺少 uid，无法定位人格目录" }));
            continue;
        }
        let dir = personal_dir_at(root, uid);
        let path = dir.join(MEMORY_FILE);
        if !path.is_file() {
            // 全新账号还没有记忆文件：不是错误，参与分发即可。
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                if !text.trim().is_empty() {
                    sources.push(MemorySource {
                        uid: uid.clone(),
                        label: label.clone(),
                        text,
                    });
                }
            }
            Err(error) => errors.push(json!({
                "label": label,
                "error": format!("读取 {MEMORY_FILE} 失败：{error}"),
            })),
        }
    }

    if sources.is_empty() {
        return json!({
            "accounts": accounts.len(),
            "sources": [],
            "mergedChars": 0,
            "written": [],
            "unchanged": [],
            "persona": { "written": [], "unchanged": [] },
            "errors": errors,
            "note": "本机账号都没有可合并的记忆内容，未做任何写入",
        });
    }

    let merged = merge_documents(&sources);
    let (written, unchanged, write_errors) = distribute(root, accounts, &merged, &sources);
    errors.extend(write_errors);
    let persona = distribute_persona(root, accounts);

    json!({
        "accounts": accounts.len(),
        "sources": sources
            .iter()
            .map(|s| json!({ "label": s.label, "chars": s.text.chars().count() }))
            .collect::<Vec<_>>(),
        "mergedChars": merged.chars().count(),
        "written": written,
        "unchanged": unchanged,
        "persona": persona,
        "errors": errors,
    })
}

struct MemorySource {
    uid: String,
    label: String,
    text: String,
}

/// 单一来源的合并结果：直接原文返回，避免给只有一份记忆的用户平白加上合成头。
/// 多来源但内容完全一致时同样走这条——没有可合并的东西，不该制造写入。
fn merge_documents(sources: &[MemorySource]) -> String {
    let normalized: Vec<String> = sources
        .iter()
        .map(|s| s.text.replace("\r\n", "\n"))
        .collect();
    let first = normalized[0].trim();
    if normalized.iter().all(|text| text.trim() == first) {
        return normalized[0].clone();
    }
    let mut prelude: Vec<Block> = Vec::new();
    // 章节标题 -> 该章节内去重后的块；另用 Vec 维持首次出现顺序。
    let mut order: Vec<String> = Vec::new();
    let mut sections: HashMap<String, Vec<Block>> = HashMap::new();

    for source in sources {
        let (source_prelude, source_sections) = split_sections(&source.text);
        for block in source_prelude {
            push_unique(&mut prelude, block);
        }
        for (title, body) in source_sections {
            if !sections.contains_key(&title) {
                order.push(title.clone());
            }
            let bucket = sections.entry(title).or_default();
            for block in body {
                push_unique(bucket, block);
            }
        }
    }

    let labels: Vec<String> = sources.iter().map(|s| s.label.clone()).collect();
    let mut out = String::new();
    out.push_str(&format!(
        "<!-- 由 workbuddy-switch 合并生成：{} 个账号（{}）。原文逐段保留，仅去除完全重复的段落与列表项。 -->\n",
        sources.len(),
        labels.join("、")
    ));
    render_blocks(&prelude, &mut out);
    for title in order {
        out.push_str(&title);
        out.push('\n');
        if let Some(blocks) = sections.get(&title) {
            render_blocks(blocks, &mut out);
        }
    }
    out.trim_end().to_string()
}

/// 合并的最小单元：普通段落整段比对，列表项逐项比对。
///
/// 按空行整段比对时，一份 20 行的清单会和另一份只差一行的清单被当成两个不同的段，
/// 结果两份都留、内容重复。拆成列表项后只留下真正不同的条目。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Block {
    /// 普通段落：前后各留一个空行。
    Para(String),
    /// 列表项：与相邻列表项之间只换行，保持列表紧凑。
    Item(String),
}

/// 把一个章节的正文切成块：列表块按行拆成条目，其余整段保留。
fn split_blocks(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    for para in text.split("\n\n") {
        let lines: Vec<&str> = para
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        if lines.is_empty() {
            continue;
        }
        if lines.iter().all(|line| is_list_line(line)) {
            blocks.extend(
                lines
                    .into_iter()
                    .map(|line| Block::Item(line.trim_end().to_string())),
            );
        } else {
            blocks.push(Block::Para(para.trim_matches('\n').trim_end().to_string()));
        }
    }
    blocks
}

/// 列表行：`- ` / `* ` / `+ ` 开头，或「数字. 」开头的有序列表。缩进子项一并算。
fn is_list_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
        return true;
    }
    match trimmed.split_once(". ") {
        Some((prefix, _)) => !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

fn block_key(block: &Block) -> String {
    match block {
        Block::Para(text) => normalize_key(text),
        // 列表项按「去首尾空白的整行」比对：缩进差异不该让同一条被当成两条，
        // 但条目内部的空格（如 `- 用 VS Code`）必须保留，否则会误判成不同项。
        Block::Item(text) => text.trim().to_string(),
    }
}

fn push_unique(target: &mut Vec<Block>, block: Block) {
    let key = block_key(&block);
    if key.is_empty() {
        return;
    }
    if target.iter().any(|existing| block_key(existing) == key) {
        return;
    }
    target.push(block);
}

/// 输出：列表项之间单换行、列表结束后留空行，普通段落之间空行。
fn render_blocks(blocks: &[Block], out: &mut String) {
    let mut prev_was_item = false;
    for block in blocks {
        match block {
            Block::Item(text) => {
                out.push_str(text);
                out.push('\n');
                prev_was_item = true;
            }
            Block::Para(text) => {
                if prev_was_item {
                    out.push('\n');
                }
                out.push_str(text);
                out.push_str("\n\n");
                prev_was_item = false;
            }
        }
    }
}

/// 以 `## ` 为界切出前言与章节；章节正文切成段落/列表项块。
fn split_sections(text: &str) -> (Vec<Block>, Vec<(String, Vec<Block>)>) {
    let normalized = text.replace("\r\n", "\n");
    let mut prelude: Vec<Block> = Vec::new();
    let mut sections: Vec<(String, Vec<Block>)> = Vec::new();
    let mut buffer: Vec<&str> = Vec::new();
    let mut in_prelude = true;

    for line in normalized.lines() {
        if line.starts_with("## ") {
            // 先收尾上一块：正文属于上一个章节，首块则归前言。
            flush_body(&mut buffer, &mut prelude, &mut sections, in_prelude);
            sections.push((line.trim_end().to_string(), Vec::new()));
            in_prelude = false;
            continue;
        }
        buffer.push(line);
    }
    flush_body(&mut buffer, &mut prelude, &mut sections, in_prelude);

    (prelude, sections)
}

fn flush_body(
    buffer: &mut Vec<&str>,
    prelude: &mut Vec<Block>,
    sections: &mut Vec<(String, Vec<Block>)>,
    is_prelude: bool,
) {
    let text = buffer.join("\n");
    buffer.clear();
    for block in split_blocks(&text) {
        if is_prelude {
            prelude.push(block);
        } else if let Some(entry) = sections.last_mut() {
            entry.1.push(block);
        }
    }
}

/// 去重键：忽略行内多余空白与缩进差异，避免同一段因空格不同被当成两段。
fn normalize_key(para: &str) -> String {
    para.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// 把合并结果写到每个账号；内容相同的跳过，已有文件先备份。
fn distribute(
    root: &Path,
    accounts: &[(String, String)],
    merged: &str,
    sources: &[MemorySource],
) -> (Vec<Value>, Vec<Value>, Vec<Value>) {
    let mut written: Vec<Value> = Vec::new();
    let mut unchanged: Vec<Value> = Vec::new();
    let mut errors: Vec<Value> = Vec::new();
    let known: Vec<&str> = sources.iter().map(|s| s.uid.as_str()).collect();

    for (uid, label) in accounts {
        if uid.trim().is_empty() {
            continue;
        }
        let dir = personal_dir_at(root, uid);
        let path = dir.join(MEMORY_FILE);
        if let Ok(current) = std::fs::read_to_string(&path) {
            if current.replace("\r\n", "\n").trim() == merged.trim() {
                unchanged.push(json!({ "label": label }));
                continue;
            }
        }
        if let Err(error) = std::fs::create_dir_all(&dir) {
            errors.push(json!({ "label": label, "error": format!("创建人格目录失败：{error}") }));
            continue;
        }
        let backup = if path.is_file() {
            let backup = backup_path(&path);
            match std::fs::copy(&path, &backup) {
                Ok(_) => Some(backup.to_string_lossy().to_string()),
                Err(error) => {
                    // 备份失败就不覆盖：宁可这份记忆不合并，也不能让原文不可恢复。
                    errors.push(json!({
                        "label": label,
                        "error": format!("备份 {MEMORY_FILE} 失败，已跳过：{error}"),
                    }));
                    continue;
                }
            }
        } else {
            None
        };
        match atomic_write(&path, merged) {
            Ok(()) => written.push(json!({
                "label": label,
                "backup": backup,
                "hadContent": known.contains(&uid.as_str()),
            })),
            Err(error) => errors.push(json!({
                "label": label,
                "error": format!("写入 {MEMORY_FILE} 失败：{error}"),
            })),
        }
    }
    (written, unchanged, errors)
}

/// 人格文件只取一份（字节数最大的那份视为最完整），且只在目标缺失时补写。
fn distribute_persona(root: &Path, accounts: &[(String, String)]) -> Value {
    let mut written: Vec<Value> = Vec::new();
    let mut unchanged: Vec<Value> = Vec::new();
    let mut errors: Vec<Value> = Vec::new();

    for file in PERSONA_FILES {
        let mut best: Option<(usize, String)> = None;
        for (uid, _) in accounts {
            if uid.trim().is_empty() {
                continue;
            }
            let path = personal_dir_at(root, uid).join(file);
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let size = text.len();
            if best.as_ref().is_none_or(|(best_size, _)| size > *best_size) {
                best = Some((size, text));
            }
        }
        let Some((_, content)) = best else {
            continue;
        };
        for (uid, label) in accounts {
            if uid.trim().is_empty() {
                continue;
            }
            let dir = personal_dir_at(root, uid);
            let path = dir.join(file);
            if path.is_file() {
                // 已有设定一律不动：人格取一份，不是覆盖。
                unchanged.push(json!({ "label": label, "file": file }));
                continue;
            }
            if std::fs::create_dir_all(&dir).is_err() {
                errors.push(json!({ "label": label, "file": file, "error": "创建人格目录失败" }));
                continue;
            }
            match atomic_write(&path, &content) {
                Ok(()) => written.push(json!({ "label": label, "file": file })),
                Err(error) => errors.push(json!({
                    "label": label,
                    "file": file,
                    "error": format!("写入失败：{error}"),
                })),
            }
        }
    }

    json!({ "written": written, "unchanged": unchanged, "errors": errors })
}

fn backup_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| MEMORY_FILE.to_string());
    path.with_file_name(format!("{name}.bak-{}", now_ms()))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "wb_switch_memory_{}_{name}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, uid: &str, file: &str, text: &str) {
            let dir = personal_dir_at(self.path(), uid);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(file), text).unwrap();
        }

        fn read(&self, uid: &str, file: &str) -> Option<String> {
            std::fs::read_to_string(personal_dir_at(self.path(), uid).join(file)).ok()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn pair(uid: &str) -> (String, String) {
        (uid.to_string(), format!("acc-{uid}"))
    }

    #[test]
    fn single_source_is_written_verbatim() {
        let dir = TempDir::new("single");
        dir.write("a", MEMORY_FILE, "# 我的记忆\n\n- 只喜欢深色模式\n");
        let report = merge_and_distribute_at(dir.path(), &[pair("a"), pair("b")]);
        // 单来源不平白加合成头，原文照搬：源账号内容已经是结果，只写目标账号。
        let written = dir.read("b", MEMORY_FILE).unwrap();
        assert!(written.contains("只喜欢深色模式"));
        assert!(!written.contains("workbuddy-switch 合并生成"));
        assert_eq!(report["written"].as_array().unwrap().len(), 1);
        assert_eq!(report["unchanged"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn merge_dedupes_identical_sections_and_keeps_unique_ones() {
        let dir = TempDir::new("merge");
        dir.write(
            "a",
            MEMORY_FILE,
            "# 长期记忆\n\n## 偏好\n\n- 喜欢简体中文\n\n## 项目\n\n- HomeCLOUD\n",
        );
        dir.write(
            "b",
            MEMORY_FILE,
            "# 长期记忆\n\n## 偏好\n\n- 喜欢简体中文\n\n## 环境\n\n- Debian 13\n",
        );
        merge_and_distribute_at(dir.path(), &[pair("a"), pair("b")]);
        let merged = dir.read("a", MEMORY_FILE).unwrap();
        assert_eq!(merged.matches("- 喜欢简体中文").count(), 1, "{merged}");
        assert!(merged.contains("- HomeCLOUD"), "{merged}");
        assert!(merged.contains("- Debian 13"), "{merged}");
        assert_eq!(dir.read("b", MEMORY_FILE).unwrap(), merged);
    }

    #[test]
    fn list_items_are_deduped_one_by_one() {
        let dir = TempDir::new("list-merge");
        dir.write(
            "a",
            MEMORY_FILE,
            "## 环境\n\n- Windows 11\n- Debian 13\n- 只用简体中文\n",
        );
        dir.write(
            "b",
            MEMORY_FILE,
            "## 环境\n\n- Windows 11\n- macOS 15\n- 只用简体中文\n",
        );
        merge_and_distribute_at(dir.path(), &[pair("a"), pair("b")]);
        let merged = dir.read("a", MEMORY_FILE).unwrap();
        // 重复条目只留一条，独有条目都要保留。
        assert_eq!(merged.matches("- Windows 11").count(), 1, "{merged}");
        assert_eq!(merged.matches("- 只用简体中文").count(), 1, "{merged}");
        assert!(merged.contains("- Debian 13"), "{merged}");
        assert!(merged.contains("- macOS 15"), "{merged}");
        // 列表项之间保持紧凑，不被空行拆成多个列表。
        assert!(merged.contains("- Debian 13\n- 只用简体中文"), "{merged}");
    }

    #[test]
    fn existing_target_is_backed_up_before_overwrite() {
        let dir = TempDir::new("backup");
        dir.write("a", MEMORY_FILE, "## 甲\n\n- 来自 A\n");
        dir.write("b", MEMORY_FILE, "## 乙\n\n- 来自 B\n");
        merge_and_distribute_at(dir.path(), &[pair("a"), pair("b")]);
        let dir_path = personal_dir_at(dir.path(), "b");
        let backups: Vec<_> = std::fs::read_dir(&dir_path)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("MEMORY.md.bak-")
            })
            .collect();
        assert_eq!(backups.len(), 1);
        let backup = std::fs::read_to_string(backups[0].path()).unwrap();
        assert!(backup.contains("来自 B"));
    }

    #[test]
    fn identical_content_is_not_rewritten() {
        let dir = TempDir::new("idempotent");
        dir.write("a", MEMORY_FILE, "## 偏好\n\n- 简体中文\n");
        dir.write("b", MEMORY_FILE, "## 偏好\n\n- 简体中文\n");
        let report = merge_and_distribute_at(dir.path(), &[pair("a"), pair("b")]);
        assert!(report["written"].as_array().unwrap().is_empty(), "{report}");
        assert_eq!(report["unchanged"].as_array().unwrap().len(), 2);
        let dir_path = personal_dir_at(dir.path(), "a");
        let has_backup = std::fs::read_dir(&dir_path).unwrap().flatten().any(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("MEMORY.md.bak-")
        });
        assert!(!has_backup, "无变化时不该产生备份");
    }

    #[test]
    fn persona_is_copied_only_into_missing_accounts() {
        let dir = TempDir::new("persona");
        dir.write("a", MEMORY_FILE, "## 甲\n\n- A\n");
        dir.write("a", "SOUL.md", "长的人格设定，字数最多的一份。\n");
        dir.write("b", MEMORY_FILE, "## 乙\n\n- B\n");
        dir.write("b", "SOUL.md", "短\n");
        let report = merge_and_distribute_at(dir.path(), &[pair("a"), pair("b"), pair("c")]);
        // 已有 SOUL 的账号不被覆盖。
        assert_eq!(dir.read("b", "SOUL.md").unwrap(), "短\n");
        // 缺失的账号补写最完整的那份。
        assert_eq!(
            dir.read("c", "SOUL.md").unwrap(),
            "长的人格设定，字数最多的一份。\n"
        );
        assert!(report["persona"]["unchanged"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["label"] == "acc-b"));
    }

    #[test]
    fn accounts_without_memory_still_receive_copy() {
        let dir = TempDir::new("empty-target");
        dir.write("a", MEMORY_FILE, "## 甲\n\n- A\n");
        let report = merge_and_distribute_at(dir.path(), &[pair("a"), pair("b")]);
        assert!(dir.read("b", MEMORY_FILE).is_some());
        // 只有缺记忆的 b 被写入；a 已是合并结果，不该被重写。
        assert_eq!(report["written"].as_array().unwrap().len(), 1);
        assert_eq!(report["written"][0]["label"], "acc-b");
    }

    #[test]
    fn no_memory_anywhere_writes_nothing() {
        let dir = TempDir::new("blank");
        let report = merge_and_distribute_at(dir.path(), &[pair("a"), pair("b")]);
        assert!(report["written"].as_array().unwrap().is_empty());
        assert!(dir.read("a", MEMORY_FILE).is_none());
    }

    #[test]
    fn missing_uid_is_reported_not_guessed() {
        let dir = TempDir::new("no-uid");
        dir.write("a", MEMORY_FILE, "## 甲\n\n- A\n");
        let report = merge_and_distribute_at(
            dir.path(),
            &[pair("a"), (String::new(), "无 uid 账号".to_string())],
        );
        assert_eq!(report["errors"].as_array().unwrap().len(), 1);
        assert_eq!(report["errors"][0]["label"], "无 uid 账号");
    }
}
