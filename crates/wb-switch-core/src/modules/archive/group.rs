//! 数据分组：每类「切换器管的东西」一个 [`DataGroupSpec`] 实现 + 一张注册表。
//!
//! 为什么做成 trait 而不是 `match group_id { ... }`：
//! - 新增一类数据 = 新增一个 struct + 在 [`registry`] 里加一行，导出/导入主流程
//!   （`super::export` / `super::import`）与宿主（Tauri / HTTP server / CLI）
//!   一行都不用改；
//! - 「收集什么」与「恢复到哪」被强制写在同一文件里，避免两边清单漂移。
//!
//! 安全边界：收集时我们生成 zip 条目名；导入时条目名**只用来匹配本文件的
//! [`relative_target`] 白名单**，实际写盘路径一律由白名单 + 固定根目录拼出。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::modules::{account, config, export_import};

use super::{ArchiveEnv, DATA_ROOT_AI, DATA_ROOT_CN, NAMESPACE_AI, NAMESPACE_CN};

// 分组 id 是前后端契约的一部分，不允许改名。
const ACCOUNTS_ID: &str = "accounts";
const SETTINGS_ID: &str = "settings";
const LOGS_ID: &str = "logs";
const STATS_ID: &str = "stats";
const MEMORY_ID: &str = "memory";
const EXPERTS_ID: &str = "experts";
const LINKS_ID: &str = "links";

/// 记忆与专家团都必须两个数据根都收：只玩国际版的用户数据全在 `.workbuddy-ai`，
/// 只收 `.workbuddy` 会导出一个空组，换机后记忆/专家团静默丢失且没有任何提示。
/// zip 条目名用 [`NAMESPACE_CN`] / [`NAMESPACE_AI`] 区分两个根，避免同名账号目录撞车。
const EXPERTS_DIR: &str = "experts";
const EXPERTS_FILE: &str = "experts.json";

/// `store_dir()` 下的单文件配置分组（存在的才收）。
const SETTINGS_FILES: [&str; 5] = [
    "auto_checkin_config.json",
    "auto_travel_config.json",
    "rate_limit_config.json",
    "github_config.json",
    "auto_rotate_config.json",
];
const LOG_FILES: [&str; 4] = [
    "auto_checkin_logs.json",
    "daily_chat_logs.json",
    "auto_rotate_logs.json",
    "notifications.json",
];
const STATS_FILES: [&str; 2] = [
    "credit_usage_snapshots.json",
    "official_usage_cache.json",
];
/// 会话关联：三份主表 + `baselines/`（行摘要，无路径）。
///
/// 刻意不收 `operations/`、`previews/`、`backup-lifecycle/`：
/// - `operations/*.json` 的 `backup` 字段是本机绝对路径，换机后指向不存在的目录；
///   未完成的 sync 操作会因此卡在「需要恢复」状态（宁可丢弃操作，也不带错路径）；
/// - previews / backup-lifecycle 都是过程态，迁移过去只会误导恢复逻辑。
const LINK_FILES: [&str; 3] = [
    "session_links.json",
    "vscode_session_links.json",
    "codebuddy_ide_session_links.json",
];
const LINK_DIR: &str = "session-links";
const LINK_BASELINES_DIR: &str = "baselines";
/// WorkBuddy 个人目录前缀 / 后缀：`user-{uid}-personal`（见 memory 模块）。
const PERSONAL_PREFIX: &str = "user-";
const PERSONAL_SUFFIX: &str = "-personal";
/// 记忆目录下的子目录名：只收其中的 Markdown。
const MEMORY_SUBDIR: &str = "memory";

/// 前端展示用的分组信息；`files` / `bytes` 是「当前磁盘」的实时统计。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveGroup {
    pub id: String,
    pub title: String,
    pub description: String,
    /// 含登录凭据：前端要打「明文导出」警告。
    pub sensitive: bool,
    pub files: u64,
    pub bytes: u64,
}

/// 一个待打包的文件：zip 内相对路径 + 磁盘来源。
#[derive(Debug, Clone)]
pub(crate) struct ArchiveEntry {
    /// 形如 `settings/auto_checkin_config.json`，分隔符固定为 `/`。
    pub(crate) name: String,
    pub(crate) source: PathBuf,
}

/// 导入上下文：已解压的条目 + 目标环境 + 计数。
///
/// 不持有备份目录：备份在 `import` 主流程里「先全量备份、后逐组写盘」，
/// 组实现拿不到备份目录也就不会写出「备份到一半又去改文件」的乱序。
pub(crate) struct ImportContext<'a> {
    pub(crate) env: &'a ArchiveEnv,
    pub(crate) files: BTreeMap<String, Vec<u8>>,
    pub(crate) accounts_added: u64,
    pub(crate) accounts_updated: u64,
}

/// 一类可迁移数据的完整定义：元信息 + 收集 + 恢复。
pub(crate) trait DataGroupSpec {
    /// (id, 中文标题, 一句话说明, 是否敏感)
    fn meta(&self) -> (&'static str, &'static str, &'static str, bool);

    /// 收集当前磁盘上属于该组的文件；不存在的不返回（空组即「本机没有」）。
    fn collect(&self, env: &ArchiveEnv) -> Vec<ArchiveEntry>;

    /// 把 `ctx.files` 中属于该组的条目恢复到本机。
    fn apply(&self, ctx: &mut ImportContext<'_>) -> Result<(), String>;

    fn id(&self) -> &'static str {
        self.meta().0
    }
}

/// 全部内置分组。新增一类数据：实现 trait，然后在下面加一行。
pub(crate) fn registry() -> Vec<Box<dyn DataGroupSpec>> {
    vec![
        Box::new(AccountsGroup),
        Box::new(SettingsGroup),
        Box::new(LogsGroup),
        Box::new(StatsGroup),
        Box::new(MemoryGroup),
        Box::new(ExpertsGroup),
        Box::new(LinksGroup),
    ]
}

/// 分组数据的根目录。
///
/// - 记忆：组内相对路径自带档位目录（`cn/...` → `.workbuddy`、`ai/...` → `.workbuddy-ai`），
///   两个根不可能共用同一个 group_root，所以根取家目录，由白名单决定档位目录；
/// - 专家团：固定写到 `.workbuddy/experts/custom/<uid>/experts.json`，同样以家目录为根；
/// - 其余：`{store_dir}`。
///
/// 与 `config::wb_data_root()` / `config::store_dir()` / `variant::data_root()` 同源；
/// 这里用注入的 env 取路径，测试才能落在临时目录而不污染真实用户数据。
pub(crate) fn group_root(env: &ArchiveEnv, group_id: &str) -> PathBuf {
    match group_id {
        MEMORY_ID | EXPERTS_ID => env.home.clone(),
        _ => env.store.clone(),
    }
}

/// 白名单：把 zip 内「组内相对路径」映射为「相对组根的真实路径」。
///
/// 返回 `None` 表示该名字不属于这个组，导入方必须报错而不是猜一个位置写下去。
pub(crate) fn relative_target(group_id: &str, rel: &str) -> Option<PathBuf> {
    if rel.is_empty() || rel.contains('\\') || rel.starts_with('/') {
        return None;
    }
    match group_id {
        ACCOUNTS_ID => (rel == "accounts.json").then(|| PathBuf::from(rel)),
        SETTINGS_ID => flat_target(&SETTINGS_FILES, rel),
        LOGS_ID => flat_target(&LOG_FILES, rel),
        STATS_ID => flat_target(&STATS_FILES, rel),
        LINKS_ID => link_target(rel),
        MEMORY_ID => memory_target(rel),
        EXPERTS_ID => experts_target(rel),
        _ => None,
    }
}

fn flat_target(names: &[&str], rel: &str) -> Option<PathBuf> {
    names.contains(&rel).then(|| PathBuf::from(rel))
}

fn link_target(rel: &str) -> Option<PathBuf> {
    if LINK_FILES.contains(&rel) {
        return Some(PathBuf::from(rel));
    }
    // 只认 `session-links/baselines/<文件名>.json` 一层：operations / previews /
    // backup-lifecycle 一律不在白名单（见 LINK_FILES 上方注释）。
    let tail = rel.strip_prefix(&format!("{LINK_DIR}/{LINK_BASELINES_DIR}/"))?;
    (!tail.contains('/') && is_plain_component(tail) && tail.ends_with(".json"))
        .then(|| PathBuf::from(rel))
}

/// zip 命名空间 → 数据根目录名（两者必须一一对应，未知命名空间直接拒绝）。
fn data_root_for_namespace(namespace: &str) -> Option<&'static str> {
    match namespace {
        NAMESPACE_CN => Some(DATA_ROOT_CN),
        NAMESPACE_AI => Some(DATA_ROOT_AI),
        _ => None,
    }
}

fn memory_target(rel: &str) -> Option<PathBuf> {
    let (namespace, rest) = rel.split_once('/')?;
    let data_root = data_root_for_namespace(namespace)?;
    let mut parts = rest.split('/');
    let dir = parts.next()?;
    if !(dir.starts_with(PERSONAL_PREFIX) && dir.ends_with(PERSONAL_SUFFIX)) {
        return None;
    }
    if !dir
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    let rest: Vec<&str> = parts.collect();
    // 目标 = 数据根目录 + 个人目录 + 剩余层级（`memory/` 子树允许继续嵌套）。
    let target = PathBuf::from(data_root)
        .join(dir)
        .join(rest.join("/"));
    match rest.as_slice() {
        // 个人目录顶层的 Markdown（MEMORY.md / SOUL.md / ...）：只收 .md，
        // 不收 workspace-state.json 与 `*.md.bak-*`（扩展名不是 md，天然排除）。
        [file] if is_markdown(file) && is_plain_component(file) => Some(target),
        // `memory/` 子树：末段必须是 Markdown。
        [subdir, tail @ ..]
            if *subdir == MEMORY_SUBDIR
                && !tail.is_empty()
                && tail.iter().all(|part| is_plain_component(part))
                && tail.last().is_some_and(|part| is_markdown(part)) =>
        {
            Some(target)
        }
        _ => None,
    }
}

/// 专家团名单白名单：`{ns}/<uid>/experts.json`，uid 必须是单段普通目录名。
///
/// 不收 `experts.json.bak-*`（历史备份），也不收 `storage/` 下的使用痕迹。
fn experts_target(rel: &str) -> Option<PathBuf> {
    let (namespace, rest) = rel.split_once('/')?;
    let data_root = data_root_for_namespace(namespace)?;
    let (uid, file) = rest.split_once('/')?;
    (is_plain_component(uid) && file == EXPERTS_FILE).then(|| {
        PathBuf::from(data_root)
            .join(EXPERTS_DIR)
            .join("custom")
            .join(uid)
            .join(EXPERTS_FILE)
    })
}

/// 单个路径段的宽松白名单：拒绝空段、`.`、`..`，只允许常用文件名字符。
fn is_plain_component(part: &str) -> bool {
    !part.is_empty()
        && part != "."
        && part != ".."
        && part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn is_markdown(file_name: &str) -> bool {
    file_name
        .rsplit_once('.')
        .is_some_and(|(stem, ext)| !stem.is_empty() && ext.eq_ignore_ascii_case("md"))
}

// ---------------------------------------------------------------------------
// 收集工具
// ---------------------------------------------------------------------------

/// 收集 `store_dir` 下若干固定文件名（不存在的跳过）。
fn collect_flat(env: &ArchiveEnv, group_id: &str, names: &[&str]) -> Vec<ArchiveEntry> {
    names
        .iter()
        .filter_map(|name| {
            let source = env.store.join(name);
            source.is_file().then(|| ArchiveEntry {
                name: format!("{group_id}/{name}"),
                source,
            })
        })
        .collect()
}

/// 递归收集目录下的普通文件，按名字排序保证导出结果可复现。
fn collect_tree(
    dir: &Path,
    zip_prefix: &str,
    accept: &dyn Fn(&Path) -> bool,
    out: &mut Vec<ArchiveEntry>,
) {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        // 读不到的目录按「不存在」处理：备份是尽力而为，单点权限问题不该让导出整体失败。
        return;
    };
    let mut children: Vec<PathBuf> = read_dir
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .collect();
    children.sort();

    for child in children {
        let Some(file_name) = child.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !is_plain_component(file_name) {
            continue;
        }
        let zip_name = format!("{zip_prefix}/{file_name}");
        let Ok(meta) = std::fs::symlink_metadata(&child) else {
            continue;
        };
        if meta.is_dir() {
            collect_tree(&child, &zip_name, accept, out);
        } else if meta.is_file() && accept(&child) {
            out.push(ArchiveEntry {
                name: zip_name,
                source: child,
            });
        }
    }
}

// ---------------------------------------------------------------------------
// 组实现
// ---------------------------------------------------------------------------

struct AccountsGroup;

impl DataGroupSpec for AccountsGroup {
    fn meta(&self) -> (&'static str, &'static str, &'static str, bool) {
        (
            ACCOUNTS_ID,
            "账号",
            "账号列表与登录凭据（含 access_token），导入时按 uid 合并，不覆盖本地其它账号",
            true,
        )
    }

    fn collect(&self, env: &ArchiveEnv) -> Vec<ArchiveEntry> {
        collect_flat(env, ACCOUNTS_ID, &["accounts.json"])
    }

    fn apply(&self, ctx: &mut ImportContext<'_>) -> Result<(), String> {
        let name = format!("{ACCOUNTS_ID}/accounts.json");
        let Some(bytes) = ctx.files.get(&name) else {
            return Ok(());
        };
        let text = std::str::from_utf8(bytes)
            .map_err(|_| "归档中的 accounts.json 不是 UTF-8 文本".to_string())?;
        // 复用导出/导入账号的全套规则（凭据判定、id 补全、备注保留、按 uid 覆盖），
        // 不在这里另写一份合并逻辑。
        let parsed = export_import::parse_accounts_json(text)?;
        if parsed.is_empty() {
            return Ok(());
        }
        let indexes: Vec<usize> = (0..parsed.len()).collect();
        let (added, updated) = account::with_accounts_write(&ctx.env.accounts_file(), |accounts| {
            let result = export_import::merge_import_records(accounts, text, &indexes)?;
            Ok((
                result.imported.saturating_sub(result.overwritten),
                result.overwritten,
            ))
        })?;
        ctx.accounts_added = added as u64;
        ctx.accounts_updated = updated as u64;
        Ok(())
    }
}

pub(crate) struct SettingsGroup;

impl DataGroupSpec for SettingsGroup {
    fn meta(&self) -> (&'static str, &'static str, &'static str, bool) {
        (
            SETTINGS_ID,
            "设置",
            "自动签到 / 旅行 / 限流 / 自动轮换 / GitHub 更新等配置",
            false,
        )
    }

    fn collect(&self, env: &ArchiveEnv) -> Vec<ArchiveEntry> {
        collect_flat(env, SETTINGS_ID, &SETTINGS_FILES)
    }

    fn apply(&self, ctx: &mut ImportContext<'_>) -> Result<(), String> {
        restore_plain_files(ctx, SETTINGS_ID)
    }
}

struct LogsGroup;

impl DataGroupSpec for LogsGroup {
    fn meta(&self) -> (&'static str, &'static str, &'static str, bool) {
        (
            LOGS_ID,
            "日志与通知",
            "签到 / 每日对话 / 自动轮换日志与本机通知记录",
            false,
        )
    }

    fn collect(&self, env: &ArchiveEnv) -> Vec<ArchiveEntry> {
        collect_flat(env, LOGS_ID, &LOG_FILES)
    }

    fn apply(&self, ctx: &mut ImportContext<'_>) -> Result<(), String> {
        restore_plain_files(ctx, LOGS_ID)
    }
}

struct StatsGroup;

impl DataGroupSpec for StatsGroup {
    fn meta(&self) -> (&'static str, &'static str, &'static str, bool) {
        (
            STATS_ID,
            "用量统计",
            "额度快照与官网套餐缓存（删除后只会重新拉取，不影响账号）",
            false,
        )
    }

    fn collect(&self, env: &ArchiveEnv) -> Vec<ArchiveEntry> {
        collect_flat(env, STATS_ID, &STATS_FILES)
    }

    fn apply(&self, ctx: &mut ImportContext<'_>) -> Result<(), String> {
        restore_plain_files(ctx, STATS_ID)
    }
}

struct MemoryGroup;

impl DataGroupSpec for MemoryGroup {
    fn meta(&self) -> (&'static str, &'static str, &'static str, bool) {
        (
            MEMORY_ID,
            "工作记忆",
            "各账号个人目录下的 Markdown（MEMORY.md 等人格与长期记忆），不含其它文件",
            false,
        )
    }

    fn collect(&self, env: &ArchiveEnv) -> Vec<ArchiveEntry> {
        // 两个档位都收：条目名带命名空间前缀，同名账号目录不会在 zip 里撞车。
        let mut out = Vec::new();
        for (root, namespace) in env
            .memory_roots()
            .into_iter()
            .zip([NAMESPACE_CN, NAMESPACE_AI])
        {
            collect_memory_root(&root, namespace, &mut out);
        }
        out
    }

    fn apply(&self, ctx: &mut ImportContext<'_>) -> Result<(), String> {
        restore_plain_files(ctx, MEMORY_ID)
    }
}

/// 收集单个数据根下的记忆 Markdown：`{root}/user-{uid}-personal/**.md`。
fn collect_memory_root(root: &Path, namespace: &str, out: &mut Vec<ArchiveEntry>) {
    let Ok(read_dir) = std::fs::read_dir(root) else {
        // 只装了一个档位的用户，另一个根本来就不存在，不是错误。
        return;
    };
    let mut dirs: Vec<PathBuf> = read_dir
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.is_dir()
                && path.file_name().and_then(|n| n.to_str()).is_some_and(|name| {
                    name.starts_with(PERSONAL_PREFIX) && name.ends_with(PERSONAL_SUFFIX)
                })
        })
        .collect();
    dirs.sort();

    for dir in dirs {
        let Some(dir_name) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !is_plain_component(dir_name) {
            continue;
        }
        let prefix = format!("{MEMORY_ID}/{namespace}/{dir_name}");

        // 顶层只收 Markdown：workspace-state.json 是客户端缓存，迁移它没有意义。
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok().map(|e| e.path()))
                    .filter(|path| {
                        path.is_file()
                            && path
                                .file_name()
                                .and_then(|n| n.to_str())
                                .is_some_and(|name| is_markdown(name) && is_plain_component(name))
                    })
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        for file in files {
            let Some(file_name) = file.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            out.push(ArchiveEntry {
                name: format!("{prefix}/{file_name}"),
                source: file,
            });
        }

        // `memory/` 子树同样只收 Markdown（保持层级）。
        let subdir = dir.join(MEMORY_SUBDIR);
        if subdir.is_dir() {
            collect_tree(
                &subdir,
                &format!("{prefix}/{MEMORY_SUBDIR}"),
                &|path| {
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(is_markdown)
                },
                out,
            );
        }
    }
}

struct ExpertsGroup;

impl DataGroupSpec for ExpertsGroup {
    fn meta(&self) -> (&'static str, &'static str, &'static str, bool) {
        (
            EXPERTS_ID,
            "自定义专家团",
            "各账号的自定义专家与专家团清单",
            false,
        )
    }

    fn collect(&self, env: &ArchiveEnv) -> Vec<ArchiveEntry> {
        // 与记忆同理：国际版用户在 `.workbuddy-ai` 下也有自己的一份清单。
        let mut out = Vec::new();
        for (root, namespace) in env
            .memory_roots()
            .into_iter()
            .zip([NAMESPACE_CN, NAMESPACE_AI])
        {
            out.extend(collect_experts_root(&root, namespace));
        }
        out
    }

    fn apply(&self, ctx: &mut ImportContext<'_>) -> Result<(), String> {
        restore_plain_files(ctx, EXPERTS_ID)
    }
}

/// 收集单个数据根下的专家团清单：`{root}/experts/custom/<uid>/experts.json`。
fn collect_experts_root(root: &Path, namespace: &str) -> Vec<ArchiveEntry> {
    let custom = root.join(EXPERTS_DIR).join("custom");
    let Ok(read_dir) = std::fs::read_dir(&custom) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = read_dir
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();

    let mut out = Vec::new();
    for dir in dirs {
        let Some(uid) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !is_plain_component(uid) {
            continue;
        }
        let source = dir.join(EXPERTS_FILE);
        if source.is_file() {
            out.push(ArchiveEntry {
                name: format!("{EXPERTS_ID}/{namespace}/{uid}/{EXPERTS_FILE}"),
                source,
            });
        }
    }
    out
}

struct LinksGroup;

impl DataGroupSpec for LinksGroup {
    fn meta(&self) -> (&'static str, &'static str, &'static str, bool) {
        (
            LINKS_ID,
            "会话关联",
            "会话跨账号关联的主表与基线（不含中断操作/预览；需配合会话正文才有意义）",
            false,
        )
    }

    fn collect(&self, env: &ArchiveEnv) -> Vec<ArchiveEntry> {
        let mut entries = collect_flat(env, LINKS_ID, &LINK_FILES);
        let baselines = env.store.join(LINK_DIR).join(LINK_BASELINES_DIR);
        if baselines.is_dir() {
            collect_tree(
                &baselines,
                &format!("{LINKS_ID}/{LINK_DIR}/{LINK_BASELINES_DIR}"),
                &|_| true,
                &mut entries,
            );
        }
        entries
    }

    fn apply(&self, ctx: &mut ImportContext<'_>) -> Result<(), String> {
        restore_plain_files(ctx, LINKS_ID)
    }
}

// ---------------------------------------------------------------------------
// 通用恢复
// ---------------------------------------------------------------------------

/// 按白名单把本组条目逐文件原子写回。
///
/// 写盘目标 = `group_root` + 白名单映射后的相对路径，不使用条目名本身拼路径；
/// 条目名只做「前缀匹配 + 白名单查询」。白名单外的条目直接报错，避免把
/// 恶意归档里的额外文件悄悄放进用户目录。
fn restore_plain_files(ctx: &mut ImportContext<'_>, group_id: &str) -> Result<(), String> {
    let prefix = format!("{group_id}/");
    let root = group_root(ctx.env, group_id);
    let mut plan: Vec<(PathBuf, String)> = Vec::new();
    // Windows 文件系统大小写不敏感：两个只差大小写的条目会写同一路径，
    // 这里显式拒绝，避免「后写的赢」这种不可解释的结果。
    let mut seen: Vec<String> = Vec::new();

    for name in ctx.files.keys() {
        let Some(rel) = name.strip_prefix(&prefix) else {
            continue;
        };
        let Some(rel_path) = relative_target(group_id, rel) else {
            return Err(format!("归档中的条目不属于可恢复范围：{name}"));
        };
        let target = root.join(&rel_path);
        let dedupe_key = target.to_string_lossy().to_lowercase();
        if seen.contains(&dedupe_key) {
            return Err(format!("归档中存在重复的目标文件：{name}"));
        }
        seen.push(dedupe_key);
        plan.push((target, name.clone()));
    }

    for (target, name) in plan {
        write_text(&target, &ctx.files[&name])?;
    }
    Ok(())
}

/// 原子写文本：先校验 UTF-8（配置/日志都是文本，二进制写进去只会让下次读取失败）。
fn write_text(target: &Path, bytes: &[u8]) -> Result<(), String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| format!("归档中的文件不是 UTF-8 文本：{}", target.display()))?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建目录失败（{}）：{e}", parent.display()))?;
    }
    config::atomic_write(target, text)
        .map_err(|e| format!("写入文件失败（{}）：{e}", target.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::update;
    use crate::modules::archive::testutil::TempRoot;

    #[test]
    fn static_names_match_config_accessors() {
        // 这里不直接用 config::*_file() 收集，是为了让测试可以注入临时根目录；
        // 用这条断言防止两边文件名漂移。
        assert_eq!(
            config::accounts_file().file_name().unwrap(),
            "accounts.json"
        );
        assert_eq!(
            config::checkin_config_file().file_name().unwrap(),
            SETTINGS_FILES[0]
        );
        assert_eq!(
            config::travel_config_file().file_name().unwrap(),
            SETTINGS_FILES[1]
        );
        assert_eq!(
            config::rate_limit_config_file().file_name().unwrap(),
            SETTINGS_FILES[2]
        );
        assert_eq!(
            update::github_config_file().file_name().unwrap(),
            SETTINGS_FILES[3]
        );
        assert_eq!(
            config::auto_rotate_config_file().file_name().unwrap(),
            SETTINGS_FILES[4]
        );
        assert_eq!(
            config::checkin_logs_file().file_name().unwrap(),
            LOG_FILES[0]
        );
        assert_eq!(LOG_FILES[1], "daily_chat_logs.json");
        assert_eq!(
            config::auto_rotate_logs_file().file_name().unwrap(),
            LOG_FILES[2]
        );
        assert_eq!(LOG_FILES[3], "notifications.json");
        assert_eq!(
            config::credit_usage_snapshots_file().file_name().unwrap(),
            STATS_FILES[0]
        );
        assert_eq!(
            config::official_usage_cache_file().file_name().unwrap(),
            STATS_FILES[1]
        );
    }

    #[test]
    fn whitelist_rejects_foreign_paths() {
        assert!(relative_target(ACCOUNTS_ID, "accounts.json").is_some());
        assert!(relative_target(ACCOUNTS_ID, "../accounts.json").is_none());
        assert!(relative_target(ACCOUNTS_ID, "other.json").is_none());
        assert!(relative_target(SETTINGS_ID, "hook.sh").is_none());
        assert!(relative_target(LOGS_ID, "auto_checkin_logs.json").is_some());
        assert!(relative_target(STATS_ID, "official_usage_cache.json").is_some());

        assert!(relative_target(LINKS_ID, "session-links/baselines/x.json").is_some());
        assert!(relative_target(LINKS_ID, "session-links/../../evil.json").is_none());
        assert!(relative_target(LINKS_ID, "vscode-session-links/x.json").is_none());
        // 中断操作带本机绝对路径，导出不收、导入也不认。
        assert!(relative_target(LINKS_ID, "session-links/operations/op.json").is_none());
        assert!(relative_target(LINKS_ID, "session-links/previews/p.json").is_none());
        assert!(relative_target(LINKS_ID, "session-links/baselines/sub/x.json").is_none());

        assert!(relative_target(MEMORY_ID, "workbuddy/user-1-personal/MEMORY.md").is_some());
        assert_eq!(
            relative_target(MEMORY_ID, "workbuddy/user-1-personal/MEMORY.md").unwrap(),
            PathBuf::from(".workbuddy").join("user-1-personal").join("MEMORY.md")
        );
        assert_eq!(
            relative_target(MEMORY_ID, "workbuddy-ai/user-1-personal/MEMORY.md").unwrap(),
            PathBuf::from(".workbuddy-ai").join("user-1-personal").join("MEMORY.md")
        );
        assert!(relative_target(MEMORY_ID, "workbuddy-ai/user-1-personal/memory/a/b.md").is_some());
        assert!(relative_target(MEMORY_ID, "workbuddy/user-1-personal/workspace-state.json").is_none());
        assert!(relative_target(MEMORY_ID, "other/user-1-personal/MEMORY.md").is_none());
        assert!(relative_target(MEMORY_ID, "workbuddy/other/MEMORY.md").is_none());
        assert!(relative_target(MEMORY_ID, "workbuddy/user-1-personal/notes.txt").is_none());
        // 旧格式（无命名空间前缀）不再接受：写入位置无法判定。
        assert!(relative_target(MEMORY_ID, "user-1-personal/MEMORY.md").is_none());

        assert!(relative_target(
            EXPERTS_ID,
            "workbuddy/u1/experts.json"
        )
        .is_some());
        assert_eq!(
            relative_target(EXPERTS_ID, "workbuddy/u1/experts.json").unwrap(),
            PathBuf::from(".workbuddy")
                .join("experts")
                .join("custom")
                .join("u1")
                .join("experts.json")
        );
        assert_eq!(
            relative_target(EXPERTS_ID, "workbuddy-ai/u9/experts.json").unwrap(),
            PathBuf::from(".workbuddy-ai")
                .join("experts")
                .join("custom")
                .join("u9")
                .join("experts.json")
        );
        assert!(relative_target(EXPERTS_ID, "workbuddy/u1/experts.json.bak-1").is_none());
        assert!(relative_target(EXPERTS_ID, "workbuddy/../experts.json").is_none());
        assert!(relative_target(EXPERTS_ID, "other/u1/experts.json").is_none());
        assert!(relative_target(EXPERTS_ID, "workbuddy/custom/u1/experts.json").is_none());
    }

    #[test]
    fn group_root_splits_store_and_data_roots() {
        let root = TempRoot::new("group-root");
        let env = root.env();
        assert_eq!(group_root(&env, SETTINGS_ID), env.store);
        // 两个数据根的组：相对路径里带根目录名，group_root 就是家目录。
        assert_eq!(group_root(&env, MEMORY_ID), env.home);
        assert_eq!(group_root(&env, EXPERTS_ID), env.home);
        assert_eq!(
            env.memory_roots(),
            [
                env.home.join(DATA_ROOT_CN),
                env.home.join(DATA_ROOT_AI)
            ]
        );
        // 根目录名必须与 variant 的取值一致，否则换版本会写错根。
        assert!(config::wb_data_root().ends_with(DATA_ROOT_CN));
        assert!(crate::modules::variant::WbVariant::Ai
            .data_root()
            .ends_with(DATA_ROOT_AI));
    }

    #[test]
    fn collect_flat_skips_missing_and_tree_sorted() {
        let root = TempRoot::new("collect");
        let env = root.env();
        std::fs::create_dir_all(&env.store).unwrap();
        std::fs::write(env.store.join("auto_checkin_config.json"), b"{}").unwrap();
        std::fs::write(env.store.join("hook.sh"), b"x").unwrap();
        let entries = SettingsGroup.collect(&env);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "settings/auto_checkin_config.json");

        let links_dir = env.store.join(LINK_DIR).join("baselines");
        std::fs::create_dir_all(&links_dir).unwrap();
        std::fs::write(links_dir.join("b.json"), b"{}").unwrap();
        std::fs::write(links_dir.join("a.json"), b"{}").unwrap();
        // 过程态目录不该进包（operations 里存的是本机绝对路径）。
        let operations = env.store.join(LINK_DIR).join("operations");
        std::fs::create_dir_all(&operations).unwrap();
        std::fs::write(operations.join("op.json"), b"{}").unwrap();

        let names: Vec<String> = LinksGroup
            .collect(&env)
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(
            names,
            vec![
                "links/session-links/baselines/a.json",
                "links/session-links/baselines/b.json"
            ]
        );
    }

    #[test]
    fn memory_collects_both_variant_roots() {
        let root = TempRoot::new("memory");
        let env = root.env();
        let cn = env
            .home
            .join(DATA_ROOT_CN)
            .join("user-abc-personal");
        std::fs::create_dir_all(cn.join("memory").join("day")).unwrap();
        std::fs::write(cn.join("MEMORY.md"), b"# m").unwrap();
        std::fs::write(cn.join("MEMORY.md.bak-1"), b"old").unwrap();
        std::fs::write(cn.join("workspace-state.json"), b"{}").unwrap();
        std::fs::write(cn.join("memory").join("day").join("a.md"), b"# a").unwrap();
        std::fs::write(cn.join("memory").join("b.txt"), b"# b").unwrap();

        let ai = env
            .home
            .join(DATA_ROOT_AI)
            .join("user-xyz-personal");
        std::fs::create_dir_all(&ai).unwrap();
        std::fs::write(ai.join("SOUL.md"), b"# soul").unwrap();

        let names: Vec<String> = MemoryGroup
            .collect(&env)
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(
            names,
            vec![
                "memory/workbuddy/user-abc-personal/MEMORY.md",
                "memory/workbuddy/user-abc-personal/memory/day/a.md",
                "memory/workbuddy-ai/user-xyz-personal/SOUL.md"
            ]
        );
    }

    #[test]
    fn memory_collects_ai_root_when_cn_absent() {
        // 只装国际版的用户：国内版根不存在，绝不能因此导出空组。
        let root = TempRoot::new("memory-ai-only");
        let env = root.env();
        let ai = env.home.join(DATA_ROOT_AI).join("user-only-ai-personal");
        std::fs::create_dir_all(&ai).unwrap();
        std::fs::write(ai.join("MEMORY.md"), b"# ai").unwrap();
        assert!(!env.home.join(DATA_ROOT_CN).exists());

        let names: Vec<String> = MemoryGroup
            .collect(&env)
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(
            names,
            vec!["memory/workbuddy-ai/user-only-ai-personal/MEMORY.md"]
        );
    }

    #[test]
    fn experts_collect_only_manifest_not_backups() {
        let root = TempRoot::new("experts");
        let env = root.env();
        let cn = env
            .home
            .join(DATA_ROOT_CN)
            .join(EXPERTS_DIR)
            .join("custom");
        std::fs::create_dir_all(cn.join("u1")).unwrap();
        std::fs::write(cn.join("u1").join("experts.json"), b"[\"a\"]").unwrap();
        std::fs::write(cn.join("u1").join("experts.json.bak-1"), b"[\"old\"]").unwrap();
        // 没有 experts.json 的账号目录不应产出空条目。
        std::fs::create_dir_all(cn.join("u2")).unwrap();

        // 国际版根同样要收，且命名空间必须区分开。
        let ai = env
            .home
            .join(DATA_ROOT_AI)
            .join(EXPERTS_DIR)
            .join("custom");
        std::fs::create_dir_all(ai.join("u1")).unwrap();
        std::fs::write(ai.join("u1").join("experts.json"), b"[\"b\"]").unwrap();

        let names: Vec<String> = ExpertsGroup
            .collect(&env)
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(
            names,
            vec![
                "experts/workbuddy/u1/experts.json",
                "experts/workbuddy-ai/u1/experts.json"
            ]
        );
    }

    #[test]
    fn apply_rejects_whitelist_violation() {
        let root = TempRoot::new("apply-bad");
        let env = root.env();
        let mut files = BTreeMap::new();
        files.insert(
            "settings/evil.exe".to_string(),
            b"boom".to_vec(),
        );
        let mut ctx = ImportContext {
            env: &env,
            files,
            accounts_added: 0,
            accounts_updated: 0,
        };
        let err = SettingsGroup.apply(&mut ctx).unwrap_err();
        assert!(err.contains("不属于可恢复范围"), "{err}");
        assert!(!env.store.join("evil.exe").exists());
    }
}
