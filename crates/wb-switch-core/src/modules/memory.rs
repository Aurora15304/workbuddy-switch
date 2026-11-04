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
//!
//! 另一种互斥的方式是**复制覆盖**：把指定母本账号的 `MEMORY.md` 整份逐字节复制到其余
//! 账号，不做任何段落合并。它解决「用户手动精简记忆后，旧段落被只增不减的合并逻辑合回来」
//! 的冲突。覆盖更危险，因此护栏更硬：母本为空/不存在一律拒绝且零写入、源账号绝不改动、
//! 每个被写账号覆盖前先备份（备份失败就不覆盖）、内容逐字节一致则跳过不制造备份。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::modules::config::{atomic_write, now_ms, wb_data_root};

/// 长期记忆文件名：唯一参与合并的文件。
const MEMORY_FILE: &str = "MEMORY.md";
/// 人格与用户档案：只取一份，仅在缺失时补写。
const PERSONA_FILES: [&str; 3] = ["SOUL.md", "IDENTITY.md", "USER.md"];
/// WorkBuddy 数据根下的账号目录前缀。
const PERSONAL_PREFIX: &str = "user-";
const PERSONAL_SUFFIX: &str = "-personal";

/// 指定 uid 的人格目录：`{root}/user-{uid}-personal`。
pub fn personal_dir_at(root: &Path, uid: &str) -> PathBuf {
    root.join(format!("{PERSONAL_PREFIX}{uid}{PERSONAL_SUFFIX}"))
}

/// 工作记忆同步方式：切号弹窗三选一，两种会写盘的模式语义互斥。
///
/// - `Merge`：把本机所有账号的记忆按「只增不减」合成一份再分发（历史行为）；
/// - `Overwrite`：把指定母本账号的 `MEMORY.md` 整份逐字节复制到其余账号，不做任何合并。
///
/// 缺省（`None`）表示不同步。宿主在边界一次性把前端字符串解析成枚举，core 只消费枚举。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemorySyncMode {
    Merge,
    Overwrite,
}

impl MemorySyncMode {
    /// 边界一次性归一化：字符串 → 枚举。缺省 = 不同步；非法值明确报错，绝不静默回退。
    pub fn parse(raw: Option<&str>) -> Result<Option<Self>, String> {
        let Some(raw) = raw else { return Ok(None) };
        match raw.trim() {
            "merge" => Ok(Some(Self::Merge)),
            "overwrite" => Ok(Some(Self::Overwrite)),
            other => Err(format!(
                "非法的工作记忆同步模式：{other:?}；只支持 \"merge\" / \"overwrite\"，不传表示不同步"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Overwrite => "overwrite",
        }
    }
}

/// 报告边界归一化：合并与覆盖共用同一形状，`mode` 是新增的判别字段，`sourceUid` 只属于覆盖。
fn with_mode(mut report: Value, mode: MemorySyncMode, source_uid: Option<&str>) -> Value {
    report["mode"] = json!(mode.as_str());
    if let Some(uid) = source_uid.filter(|uid| !uid.trim().is_empty()) {
        report["sourceUid"] = json!(uid);
    }
    report
}

/// 把指定母本账号的 `MEMORY.md` 整份复制覆盖到所有其他账号，不做任何段落合并。
///
/// `source_uid` 由前端算好后传入（core 只认 uid，不猜「当前账号」）。返回与合并模式同形的
/// 报告；`persona` 恒为空——覆盖模式只动 `MEMORY.md`，不碰人设、专家团与云端缓存。
pub fn copy_and_overwrite(source_uid: Option<&str>, accounts: &[(String, String)]) -> Value {
    copy_and_overwrite_at(&wb_data_root(), source_uid, accounts)
}

/// 路径注入接缝：真实落盘测试传临时 root，不污染用户真实目录。
fn copy_and_overwrite_at(
    root: &Path,
    source_uid: Option<&str>,
    accounts: &[(String, String)],
) -> Value {
    overwrite_at(root, source_uid, accounts, |from, to| std::fs::copy(from, to))
}

/// 备份原语接缝：注入失败即可确定性验证「备份失败就不覆盖」，核心流程无需脆弱 Mock。
type BackupFn = fn(&Path, &Path) -> std::io::Result<u64>;

/// 覆盖报告骨架：既有字段一个不删，`persona` 恒为空（本模式不碰人格文件）。
fn overwrite_report(
    accounts: usize,
    sources: Vec<Value>,
    merged_chars: usize,
    written: Vec<Value>,
    unchanged: Vec<Value>,
    errors: Vec<Value>,
) -> Value {
    json!({
        "accounts": accounts,
        "sources": sources,
        "mergedChars": merged_chars,
        "written": written,
        "unchanged": unchanged,
        "persona": { "written": [], "unchanged": [], "errors": [] },
        "errors": errors,
    })
}

fn overwrite_at(
    root: &Path,
    source_uid: Option<&str>,
    accounts: &[(String, String)],
    backup: BackupFn,
) -> Value {
    // 报告在唯一出口贴 mode/sourceUid，成功与拒绝都自描述，注入测试也走同一条路。
    let finish = |report: Value| {
        with_mode(report, MemorySyncMode::Overwrite, source_uid.map(str::trim))
    };
    let reject = |error: String| {
        finish(overwrite_report(
            accounts.len(),
            vec![],
            0,
            vec![],
            vec![],
            vec![json!({ "label": "母本", "error": error })],
        ))
    };

    // 卫语句：uid 在入口一次性归一化并过信任边界；core 只认 uid，不猜「当前账号」。
    let Some(uid) = source_uid.map(str::trim).filter(|uid| !uid.is_empty()) else {
        return reject("复制覆盖必须指定母本账号 uid（memorySourceUid），已跳过且未写入任何文件".to_string());
    };
    if !crate::modules::vscode_session::is_safe_uid(uid) {
        return reject(format!("母本账号 uid 非法（含路径分隔符或保留名）：{uid:?}"));
    }

    // 卫语句：母本缺失 / 为空 / 不可读一律报错且零写入——绝不能用空记忆抹掉所有账号。
    let mother_path = personal_dir_at(root, uid).join(MEMORY_FILE);
    let mother = match std::fs::read_to_string(&mother_path) {
        Ok(text) if !text.trim().is_empty() => text,
        Ok(_) => return reject(format!("母本账号 {uid} 的 {MEMORY_FILE} 为空，已跳过且未写入任何文件")),
        Err(error) => {
            return reject(format!("读取母本账号 {uid} 的 {MEMORY_FILE} 失败：{error}，已跳过且未写入任何文件"))
        }
    };

    let mother_label = accounts
        .iter()
        .find(|(account_uid, _)| account_uid.trim() == uid)
        .map(|(_, label)| label.clone())
        .unwrap_or_else(|| uid.to_string());
    let merged_chars = mother.chars().count();
    let sources = vec![json!({ "label": mother_label, "chars": merged_chars })];
    let mother_bytes = mother.as_bytes();

    let mut written: Vec<Value> = Vec::new();
    let mut unchanged: Vec<Value> = Vec::new();
    let mut errors: Vec<Value> = Vec::new();

    for (account_uid, label) in accounts {
        if account_uid.trim().is_empty() {
            errors.push(json!({ "label": label, "error": "账号缺少 uid，无法定位人格目录" }));
            continue;
        }
        if account_uid.trim() == uid {
            // 源账号文件绝不改动：母本自身就是目标内容。
            unchanged.push(json!({ "label": label }));
            continue;
        }
        let dir = personal_dir_at(root, account_uid);
        let path = dir.join(MEMORY_FILE);
        // 内容逐字节一致则跳过：不写、不制造无意义备份。
        if std::fs::read(&path).ok().as_deref() == Some(mother_bytes) {
            unchanged.push(json!({ "label": label }));
            continue;
        }
        if let Err(error) = std::fs::create_dir_all(&dir) {
            errors.push(json!({ "label": label, "error": format!("创建人格目录失败：{error}") }));
            continue;
        }
        let had_content = path.is_file();
        let backup_saved = if had_content {
            let backup_dest = backup_path(&path);
            match backup(&path, &backup_dest) {
                Ok(_) => Some(backup_dest.to_string_lossy().to_string()),
                Err(error) => {
                    // 备份失败就不覆盖：原文不可恢复比不同步更糟。
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
        match atomic_write(&path, &mother) {
            Ok(()) => written.push(json!({
                "label": label,
                "backup": backup_saved,
                "hadContent": had_content,
            })),
            Err(error) => errors.push(json!({
                "label": label,
                "error": format!("写入 {MEMORY_FILE} 失败：{error}"),
            })),
        }
    }

    finish(overwrite_report(
        accounts.len(),
        sources,
        merged_chars,
        written,
        unchanged,
        errors,
    ))
}

/// 把本机所有账号的记忆合成一份并分发回去。
///
/// `accounts` 是 `[{"uid": "...", "label": "..."}]` 的形状（调用方从账号库投影），
/// 缺 uid 的账号会被跳过——uid 是目录名的唯一依据，猜不出来。
pub fn merge_and_distribute(accounts: &[(String, String)]) -> Value {
    // 只贴一层自描述元数据；合并算法与 `_at` 的既有字段一字未动。
    with_mode(
        merge_and_distribute_at(&wb_data_root(), accounts),
        MemorySyncMode::Merge,
        None,
    )
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
        // 记忆没什么可合并的，但人设照样按安全模式补一遍：
        // 「新账号没有 MEMORY.md」不该顺手把人设同步一起掐掉。
        let persona = distribute_persona(root, accounts, PersonaMode::Safe);
        return json!({
            "accounts": accounts.len(),
            "sources": [],
            "mergedChars": 0,
            "written": [],
            "unchanged": [],
            "persona": persona,
            "errors": errors,
            "note": "本机账号都没有可合并的记忆内容，未做任何写入",
        });
    }

    let merged = merge_documents(&sources);
    let (written, unchanged, write_errors) = distribute(root, accounts, &merged, &sources);
    errors.extend(write_errors);
    let persona = distribute_persona(root, accounts, PersonaMode::Safe);

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

/// 人设同步模式：由用户在切号弹窗里二选一，两种语义互斥。
///
/// - `Safe`：只覆盖「仍是客户端默认模板」的目标文件，手改过的一律不动；
/// - `Overwrite`：以所有账号里最完整的一份为准，内容不同的统一覆盖（先备份）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersonaMode {
    Safe,
    Overwrite,
}

impl PersonaMode {
    /// 只有显式传 `"overwrite"` 才强制覆盖；缺省、拼错一律走安全模式。
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim) {
            Some("overwrite") => Self::Overwrite,
            _ => Self::Safe,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Overwrite => "overwrite",
        }
    }
}

/// 只同步人设，不碰 `MEMORY.md`。
///
/// 独立入口：人设同步不再依附「记忆合并」——记忆没什么可合并的时候，人设照样要能补。
pub fn sync_persona(accounts: &[(String, String)], mode: PersonaMode) -> Value {
    sync_persona_at(&wb_data_root(), accounts, mode)
}

fn sync_persona_at(root: &Path, accounts: &[(String, String)], mode: PersonaMode) -> Value {
    let mut report = distribute_persona(root, accounts, mode);
    report["mode"] = json!(mode.as_str());
    report
}

/// 模板基准：根部 `~/.workbuddy/<file>` 是客户端首次生成的原始模板。
///
/// 读它而不是硬编码大小或 md5——客户端升级换了模板，这里的判定自动跟着变。
/// 根部文件缺失时返回 `None`，安全模式退化为「只在目标缺失时补写」。
fn template_baseline(root: &Path, file: &str) -> Option<String> {
    std::fs::read_to_string(root.join(file))
        .ok()
        .map(|text| text.replace("\r\n", "\n"))
}

/// 人格文件只取一份（字节数最大的那份视为最完整），按 `mode` 决定写不写。
fn distribute_persona(root: &Path, accounts: &[(String, String)], mode: PersonaMode) -> Value {
    let mut written: Vec<Value> = Vec::new();
    let mut unchanged: Vec<Value> = Vec::new();
    let mut errors: Vec<Value> = Vec::new();

    for file in PERSONA_FILES {
        let baseline = template_baseline(root, file);
        // 模板副本不算人设：否则全新机器会把 A 的默认模板当成「最完整人格」推给 B。
        let mut best: Option<(usize, String, String)> = None;
        for (uid, label) in accounts {
            if uid.trim().is_empty() {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(personal_dir_at(root, uid).join(file)) else {
                continue;
            };
            let normalized = text.replace("\r\n", "\n");
            if baseline.as_deref() == Some(normalized.as_str()) {
                continue;
            }
            if best
                .as_ref()
                .is_none_or(|(best_size, _, _)| text.len() > *best_size)
            {
                best = Some((text.len(), text, label.clone()));
            }
        }
        let Some((_, content, source)) = best else {
            continue;
        };

        for (uid, label) in accounts {
            if uid.trim().is_empty() {
                continue;
            }
            let dir = personal_dir_at(root, uid);
            let path = dir.join(file);
            let existing = std::fs::read_to_string(&path).ok();
            let replaceable = match &existing {
                // 目标缺失：两种模式都补。
                None => true,
                Some(text) => match mode {
                    // 安全模式：只有「内容仍等于模板」才当作空壳可换。
                    PersonaMode::Safe => baseline
                        .as_deref()
                        .is_some_and(|base| base == text.replace("\r\n", "\n")),
                    PersonaMode::Overwrite => text != &content,
                },
            };
            if !replaceable {
                // 已有自定义设定：安全模式下如实报 unchanged，不做任何写入。
                unchanged.push(json!({ "label": label, "file": file }));
                continue;
            }
            if std::fs::create_dir_all(&dir).is_err() {
                errors.push(json!({ "label": label, "file": file, "error": "创建人格目录失败" }));
                continue;
            }
            // 覆盖前必须备份：人格是手写内容，不可恢复比不同步更糟。
            let backup = if existing.is_some() {
                let backup = backup_path(&path);
                match std::fs::copy(&path, &backup) {
                    Ok(_) => Some(backup.to_string_lossy().to_string()),
                    Err(error) => {
                        errors.push(json!({
                            "label": label,
                            "file": file,
                            "error": format!("备份失败，已跳过：{error}"),
                        }));
                        continue;
                    }
                }
            } else {
                None
            };
            match atomic_write(&path, &content) {
                Ok(()) => written.push(json!({
                    "label": label,
                    "file": file,
                    "backup": backup,
                    "source": source,
                })),
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

    fn read_bytes(dir: &TempDir, uid: &str, file: &str) -> Option<Vec<u8>> {
        std::fs::read(personal_dir_at(dir.path(), uid).join(file)).ok()
    }

    fn backup_paths(dir: &TempDir, uid: &str) -> Vec<PathBuf> {
        let personal = personal_dir_at(dir.path(), uid);
        let mut found: Vec<PathBuf> = std::fs::read_dir(&personal)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| name.starts_with("MEMORY.md.bak-"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        found.sort();
        found
    }

    fn never_backup(_: &Path, _: &Path) -> std::io::Result<u64> {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "模拟备份失败",
        ))
    }

    /// 带 CRLF 与非 ASCII，用来钉死「逐字节复制」而非归一化合并。
    const MOTHER: &str = "# 精简后的记忆\r\n\r\n- 只保留这一条\r\n- 中文 🚀\n";

    #[test]
    fn overwrite_copies_source_verbatim_and_backs_up_each_target() {
        let dir = TempDir::new("overwrite-verbatim");
        dir.write("a", MEMORY_FILE, MOTHER);
        dir.write("b", MEMORY_FILE, "## 乙的旧记忆\n\n- 这段应被删掉\n");
        dir.write("d", MEMORY_FILE, MOTHER); // 与母本完全一致
        dir.write("b", "SOUL.md", "乙的人格，不得改动\n");
        let b_before = read_bytes(&dir, "b", MEMORY_FILE).unwrap();
        let accounts = vec![pair("a"), pair("b"), pair("c"), pair("d")];

        let report = copy_and_overwrite_at(dir.path(), Some("a"), &accounts);

        assert_eq!(report["mode"], "overwrite");
        assert_eq!(report["sourceUid"], "a");
        assert_eq!(report["accounts"].as_u64().unwrap(), 4);
        assert!(report["errors"].as_array().unwrap().is_empty(), "{report}");
        let sources = report["sources"].as_array().unwrap();
        assert_eq!(sources.len(), 1, "覆盖模式 sources 只列母本一份");
        assert_eq!(sources[0]["label"], "acc-a");
        assert_eq!(
            sources[0]["chars"].as_u64().unwrap(),
            MOTHER.chars().count() as u64
        );
        assert_eq!(
            report["mergedChars"].as_u64().unwrap(),
            MOTHER.chars().count() as u64
        );
        for key in ["written", "unchanged", "errors"] {
            assert!(report["persona"][key].as_array().unwrap().is_empty(), "{report}");
        }

        // 写入后目标逐字节等于母本；源账号逐字节不变；人格文件不碰。
        assert_eq!(read_bytes(&dir, "b", MEMORY_FILE).unwrap(), MOTHER.as_bytes());
        assert_eq!(read_bytes(&dir, "c", MEMORY_FILE).unwrap(), MOTHER.as_bytes());
        assert_eq!(read_bytes(&dir, "d", MEMORY_FILE).unwrap(), MOTHER.as_bytes());
        assert_eq!(read_bytes(&dir, "a", MEMORY_FILE).unwrap(), MOTHER.as_bytes());
        assert_eq!(
            read_bytes(&dir, "b", "SOUL.md").unwrap(),
            "乙的人格，不得改动\n".as_bytes()
        );

        // 不同账号先备份，且备份逐字节等于原文；一致账号与源账号不产生备份。
        let b_backups = backup_paths(&dir, "b");
        assert_eq!(b_backups.len(), 1, "{b_backups:?}");
        assert_eq!(std::fs::read(&b_backups[0]).unwrap(), b_before);
        assert!(backup_paths(&dir, "a").is_empty(), "源账号不得备份");
        assert!(backup_paths(&dir, "c").is_empty(), "全新账号无原文可备份");
        assert!(backup_paths(&dir, "d").is_empty(), "内容一致不制造无意义备份");

        let written = report["written"].as_array().unwrap();
        assert_eq!(written.len(), 2, "{report}");
        let written_labels: Vec<&str> = written
            .iter()
            .map(|item| item["label"].as_str().unwrap())
            .collect();
        assert!(written_labels.contains(&"acc-b"));
        assert!(written_labels.contains(&"acc-c"));
        let unchanged = report["unchanged"].as_array().unwrap();
        let unchanged_labels: Vec<&str> = unchanged
            .iter()
            .map(|item| item["label"].as_str().unwrap())
            .collect();
        assert!(unchanged_labels.contains(&"acc-a"));
        assert!(unchanged_labels.contains(&"acc-d"));
    }

    #[test]
    fn overwrite_rejects_missing_or_blank_mother_with_zero_writes() {
        for source_uid in [None, Some(""), Some("   ")] {
            let dir = TempDir::new("overwrite-no-uid");
            dir.write("b", MEMORY_FILE, "## 乙的旧记忆\n");
            let before = read_bytes(&dir, "b", MEMORY_FILE).unwrap();
            let report = copy_and_overwrite_at(dir.path(), source_uid, &[pair("a"), pair("b")]);
            assert_eq!(report["mode"], "overwrite");
            assert!(!report["errors"].as_array().unwrap().is_empty(), "{source_uid:?}: {report}");
            assert!(report["written"].as_array().unwrap().is_empty(), "{report}");
            assert!(report["sources"].as_array().unwrap().is_empty());
            assert_eq!(report["mergedChars"].as_u64().unwrap(), 0);
            assert_eq!(read_bytes(&dir, "b", MEMORY_FILE).unwrap(), before);
            assert!(backup_paths(&dir, "b").is_empty());
            assert!(!personal_dir_at(dir.path(), "a").exists(), "零写入不得新建目录");
        }

        // uid 合法但 MEMORY.md 不存在。
        let dir = TempDir::new("overwrite-no-file");
        dir.write("b", MEMORY_FILE, "## 乙的旧记忆\n");
        let before = read_bytes(&dir, "b", MEMORY_FILE).unwrap();
        let report = copy_and_overwrite_at(dir.path(), Some("a"), &[pair("a"), pair("b")]);
        assert!(!report["errors"].as_array().unwrap().is_empty(), "{report}");
        assert!(report["written"].as_array().unwrap().is_empty());
        assert_eq!(read_bytes(&dir, "b", MEMORY_FILE).unwrap(), before);

        // 母本存在但为空 / 只有空白。
        for blank in ["", "   \n\n  \t"] {
            let dir = TempDir::new("overwrite-blank");
            dir.write("a", MEMORY_FILE, blank);
            dir.write("b", MEMORY_FILE, "## 乙的旧记忆\n");
            let before = read_bytes(&dir, "b", MEMORY_FILE).unwrap();
            let report = copy_and_overwrite_at(dir.path(), Some("a"), &[pair("a"), pair("b")]);
            assert!(!report["errors"].as_array().unwrap().is_empty(), "{blank:?}: {report}");
            assert!(report["written"].as_array().unwrap().is_empty());
            assert_eq!(read_bytes(&dir, "b", MEMORY_FILE).unwrap(), before);
        }
    }

    #[test]
    fn overwrite_rejects_unsafe_source_uid_with_zero_writes() {
        for uid in ["../evil", "a/b", "a\\b", "..", "default", "Public"] {
            let dir = TempDir::new("overwrite-unsafe-uid");
            dir.write("b", MEMORY_FILE, "## 乙的旧记忆\n");
            let before = read_bytes(&dir, "b", MEMORY_FILE).unwrap();
            let report = copy_and_overwrite_at(dir.path(), Some(uid), &[pair("a"), pair("b")]);
            assert!(!report["errors"].as_array().unwrap().is_empty(), "{uid}: {report}");
            assert!(report["written"].as_array().unwrap().is_empty(), "{report}");
            assert_eq!(read_bytes(&dir, "b", MEMORY_FILE).unwrap(), before);
        }
    }

    #[test]
    fn overwrite_backup_failure_blocks_overwrite_and_reports_error() {
        let dir = TempDir::new("overwrite-backup-fail");
        dir.write("a", MEMORY_FILE, MOTHER);
        dir.write("b", MEMORY_FILE, "## 乙的旧记忆\n\n- 不能丢\n");
        dir.write("c", MEMORY_FILE, "## 丙的旧记忆\n");
        let b_before = read_bytes(&dir, "b", MEMORY_FILE).unwrap();
        let c_before = read_bytes(&dir, "c", MEMORY_FILE).unwrap();

        let report = overwrite_at(
            dir.path(),
            Some("a"),
            &[pair("a"), pair("b"), pair("c")],
            never_backup,
        );

        assert_eq!(report["mode"], "overwrite");
        assert!(report["written"].as_array().unwrap().is_empty(), "{report}");
        let errors = report["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 2, "{report}");
        assert!(
            errors
                .iter()
                .all(|item| item["error"].as_str().unwrap().contains("备份")),
            "错误必须出现在报告：{report}"
        );
        assert_eq!(read_bytes(&dir, "b", MEMORY_FILE).unwrap(), b_before, "备份失败绝不覆盖");
        assert_eq!(read_bytes(&dir, "c", MEMORY_FILE).unwrap(), c_before);
        assert_eq!(read_bytes(&dir, "a", MEMORY_FILE).unwrap(), MOTHER.as_bytes());
        assert!(backup_paths(&dir, "b").is_empty());
    }

    #[test]
    fn overwrite_is_idempotent_when_every_target_matches_mother() {
        let dir = TempDir::new("overwrite-idempotent");
        for uid in ["a", "b", "c"] {
            dir.write(uid, MEMORY_FILE, MOTHER);
        }
        let accounts = vec![pair("a"), pair("b"), pair("c")];
        for _ in 0..2 {
            let report = copy_and_overwrite_at(dir.path(), Some("a"), &accounts);
            assert!(report["written"].as_array().unwrap().is_empty(), "{report}");
            assert!(report["errors"].as_array().unwrap().is_empty(), "{report}");
            assert_eq!(report["unchanged"].as_array().unwrap().len(), 3);
        }
        assert!(backup_paths(&dir, "b").is_empty());
        assert!(backup_paths(&dir, "c").is_empty());
    }

    #[test]
    fn memory_sync_mode_parse_normalizes_and_rejects_illegal_values() {
        assert_eq!(MemorySyncMode::parse(None).unwrap(), None);
        assert_eq!(
            MemorySyncMode::parse(Some("merge")).unwrap(),
            Some(MemorySyncMode::Merge)
        );
        assert_eq!(
            MemorySyncMode::parse(Some("overwrite")).unwrap(),
            Some(MemorySyncMode::Overwrite)
        );
        assert_eq!(
            MemorySyncMode::parse(Some("  overwrite  ")).unwrap(),
            Some(MemorySyncMode::Overwrite)
        );
        for illegal in ["", "MERGE", "sync", "overwrite-all", "true", "0", "merge overwrite"] {
            let error = MemorySyncMode::parse(Some(illegal)).unwrap_err();
            assert!(error.contains("非法"), "{illegal}: {error}");
            assert!(error.contains("merge"), "{illegal}: {error}");
        }
        assert_eq!(MemorySyncMode::Merge.as_str(), "merge");
        assert_eq!(MemorySyncMode::Overwrite.as_str(), "overwrite");
    }

    #[test]
    fn merge_report_keeps_legacy_fields_and_only_gains_mode() {
        let dir = TempDir::new("merge-regression");
        dir.write("a", MEMORY_FILE, "## 偏好\n\n- 简体中文\n\n## 甲独有\n\n- A\n");
        dir.write("b", MEMORY_FILE, "## 环境\n\n- Debian 13\n\n## 乙独有\n\n- B\n");
        let seam = merge_and_distribute_at(dir.path(), &[pair("a"), pair("b")]);
        // 「只增不减」回归：双方独有章节都保留，写回同一份。
        let merged = dir.read("a", MEMORY_FILE).unwrap();
        assert!(merged.contains("甲独有") && merged.contains("乙独有"), "{merged}");
        assert!(merged.contains("- A") && merged.contains("- B"), "{merged}");
        assert_eq!(dir.read("b", MEMORY_FILE).unwrap(), merged);
        for key in [
            "accounts",
            "sources",
            "mergedChars",
            "written",
            "unchanged",
            "persona",
            "errors",
        ] {
            assert!(seam.get(key).is_some(), "合并报告缺字段 {key}");
        }
        assert!(seam.get("mode").is_none(), "接缝层合并报告逐字不变");

        let report = with_mode(seam.clone(), MemorySyncMode::Merge, None);
        assert_eq!(report["mode"], "merge");
        assert!(report.get("sourceUid").is_none());
        assert_eq!(report["written"], seam["written"]);
        let overwrite = with_mode(json!({ "written": [] }), MemorySyncMode::Overwrite, Some("u1"));
        assert_eq!(overwrite["mode"], "overwrite");
        assert_eq!(overwrite["sourceUid"], "u1");
    }

    #[test]
    fn overwrite_concurrent_calls_are_independent_and_never_touch_source() {
        // 生产环境由 auth_transaction 锁串行化切号（另有集成测试覆盖）；
        // 这里钉死模块本身无全局可变状态、并发落盘互不串扰。
        let handles: Vec<_> = (0..6)
            .map(|index| {
                std::thread::spawn(move || {
                    let dir = TempDir::new(&format!("overwrite-concurrent-{index}"));
                    dir.write("a", MEMORY_FILE, MOTHER);
                    dir.write("b", MEMORY_FILE, "## 旧记忆\n");
                    let accounts = vec![pair("a"), pair("b"), pair("c")];
                    for _ in 0..3 {
                        let report = copy_and_overwrite_at(dir.path(), Some("a"), &accounts);
                        assert!(report["errors"].as_array().unwrap().is_empty(), "{report}");
                    }
                    assert_eq!(read_bytes(&dir, "a", MEMORY_FILE).unwrap(), MOTHER.as_bytes());
                    assert_eq!(read_bytes(&dir, "b", MEMORY_FILE).unwrap(), MOTHER.as_bytes());
                    assert_eq!(read_bytes(&dir, "c", MEMORY_FILE).unwrap(), MOTHER.as_bytes());
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("并发覆盖不得 panic");
        }
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

    /// 安全模式：目标内容仍是客户端默认模板时允许替换。
    ///
    /// 这正是「默认模板挡住人设同步」的修复点：客户端首次登录会种一份模板，
    /// 旧逻辑只要 `path.is_file()` 就跳过，导致自定义人格永远同步不过去。
    #[test]
    fn persona_safe_mode_replaces_client_template() {
        let dir = TempDir::new("persona-template");
        // 根部文件 = 客户端首次生成的原文模板。
        std::fs::write(dir.path().join("SOUL.md"), "默认模板\n").unwrap();
        dir.write("a", MEMORY_FILE, "## 甲\n\n- A\n");
        dir.write("a", "SOUL.md", "自定义人格：审讯官\n");
        dir.write("b", MEMORY_FILE, "## 乙\n\n- B\n");
        dir.write("b", "SOUL.md", "默认模板\n");
        dir.write("c", MEMORY_FILE, "## 丙\n\n- C\n");

        let report = merge_and_distribute_at(dir.path(), &[pair("a"), pair("b"), pair("c")]);

        // 仍是模板的 b 被替换，缺失的 c 直接补写，源账号保持原样。
        assert_eq!(dir.read("b", "SOUL.md").unwrap(), "自定义人格：审讯官\n");
        assert_eq!(dir.read("c", "SOUL.md").unwrap(), "自定义人格：审讯官\n");
        assert_eq!(dir.read("a", "SOUL.md").unwrap(), "自定义人格：审讯官\n");
        let written = report["persona"]["written"].as_array().unwrap();
        assert!(
            written
                .iter()
                .any(|item| item["label"] == "acc-b" && item["backup"].is_string()),
            "覆盖默认模板前必须留下备份：{written:?}"
        );
    }

    /// 模板本身不算人设：三个账号都只有默认模板时，一个都不写（避免把模板当人格互推）。
    #[test]
    fn persona_safe_mode_skips_when_every_account_is_template() {
        let dir = TempDir::new("persona-all-template");
        std::fs::write(dir.path().join("SOUL.md"), "默认模板\n").unwrap();
        for uid in ["a", "b"] {
            dir.write(uid, MEMORY_FILE, "## 甲\n\n- A\n");
            dir.write(uid, "SOUL.md", "默认模板\n");
        }

        let report = merge_and_distribute_at(dir.path(), &[pair("a"), pair("b")]);

        assert!(report["persona"]["written"].as_array().unwrap().is_empty());
        assert_eq!(dir.read("a", "SOUL.md").unwrap(), "默认模板\n");
        assert_eq!(dir.read("b", "SOUL.md").unwrap(), "默认模板\n");
    }

    /// 强制模式：所有账号统一成最完整的一份，手改过的也覆盖，但必须先备份。
    #[test]
    fn persona_overwrite_mode_unifies_every_account() {
        let dir = TempDir::new("persona-overwrite");
        std::fs::write(dir.path().join("SOUL.md"), "默认模板\n").unwrap();
        dir.write("a", "SOUL.md", "最长的一份人设设定，字数明显更多。\n");
        dir.write("b", "SOUL.md", "短人设\n");

        let report = sync_persona_at(dir.path(), &[pair("a"), pair("b")], PersonaMode::Overwrite);

        assert_eq!(
            dir.read("b", "SOUL.md").unwrap(),
            "最长的一份人设设定，字数明显更多。\n"
        );
        assert_eq!(report["mode"], "overwrite");
        let written = report["written"].as_array().unwrap();
        assert!(
            written
                .iter()
                .any(|item| item["label"] == "acc-b" && item["backup"].is_string()),
            "强制覆盖必须备份原人设：{written:?}"
        );
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
