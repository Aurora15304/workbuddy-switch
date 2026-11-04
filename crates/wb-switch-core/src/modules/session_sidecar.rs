//! 会话附属目录（sidecar）的复制：客户端渲染依赖的子 agent 记录与工具结果文件。
//!
//! # 这个目录是什么
//!
//! WorkBuddy 客户端的一个会话在磁盘上不只有主正文 `{会话id}.jsonl`，还有一个
//! **同名附属目录** `{会话id}/`：
//!
//! ```text
//! {工作区名}/
//!   {主会话id}.jsonl                  ← 主正文（复制/同步本来就只搬这一个）
//!   {主会话id}/                       ← 附属目录（本模块负责）
//!     subagents/
//!       agent-{uuid}.jsonl            ← 子 agent 的完整 transcript
//!       agent-{uuid}/
//!         tool-results/*.txt
//!     tool-results/
//!       call_xxx-render.txt           ← 主会话自己的大输出渲染文件
//! ```
//!
//! # 为什么必须复制
//!
//! 客户端渲染子 agent 面板时按 `childDir = resolveContainedPath(projectDir,
//! conversationId, 'subagents')` 定位目录，再按 `resolveJsonlPath(childDir,
//! storeId, 'storeId')` 取 `{会话id}/subagents/{storeId}.jsonl`。文件缺失时
//! `loadTailSummary` 抛出的错被 catch 成 `{records: [], hasMore: false}`——
//! **子 agent 面板空白且不报错**，用户只会看到「子 agent 的过程丢了」。
//!
//! # 三条硬约束
//!
//! 1. **目录必须按目标会话 id 建**：主正文里 `providerData.toolResult.renderer.value`
//!    内嵌了绝对路径 `.../{主会话id}/tool-results/call_xxx-render.txt`，复制时
//!    主会话 id 会被整体替换成新 id，所以附属目录必须落在 `{新主会话id}/` 下，
//!    否则正文指向的工具结果路径会指回源会话的目录。
//! 2. **文件内容同样要做 id 替换**：子 agent 的 transcript 里也嵌着主会话 id
//!    （渲染文本中的路径），照搬源内容会让副本里的路径指回源会话。
//! 3. **绝不覆盖目标已有文件**：目标副本自己后续也会产生子 agent 记录，
//!    覆盖会破坏目标侧自己的历史。
//!
//! # 失败语义
//!
//! 附属目录**不是**主正文之外的必需产物：正文是业务完成门禁，附属目录只是渲染
//! 增强材料。因此单个文件失败只记录进 [`SidecarOutcome::errors`]、不中断整体，
//! 也不让复制/同步被判死——缺失的只是子 agent 面板内容，主会话照常可用。

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::modules::session_backup;

/// 子 agent transcript 所在子目录名。
const SUBDIR_SUBAGENTS: &str = "subagents";
/// 工具结果渲染文件所在子目录名。
const SUBDIR_TOOL_RESULTS: &str = "tool-results";

/// 属于会话材料的子目录白名单（唯一真相源，表驱动）。
///
/// 客户端只读这两个目录；表外内容既不属于会话附属材料，也是路径逃逸
/// （`..` 之类）的入口。客户端将来新增目录（例如 `plans/`）时**必须在这里补
/// 一行**——漏补不会静默消失：它会被计入 [`SidecarOutcome::ignored_entries`]
/// 并在报告的 `notes` 里留名。
const SIDECAR_SUBDIRS: &[(&str, FileKind)] = &[
    (SUBDIR_SUBAGENTS, FileKind::Agent),
    (SUBDIR_TOOL_RESULTS, FileKind::ToolResult),
];

/// 一次附属目录复制的结果摘要（供报告与一次性补齐工具使用）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SidecarOutcome {
    /// 源侧存在附属目录（绝大多数会话没有子 agent）。
    pub source_present: bool,
    /// 复制的子 agent transcript 文件数（`subagents/*.jsonl`）。
    pub agents_copied: usize,
    /// 复制的工具结果文件数（任何层级下的 `tool-results/`）。
    pub tool_results_copied: usize,
    /// 复制的其它文件数。
    pub other_copied: usize,
    /// 目标已存在而跳过（绝不覆盖）的文件数。
    pub skipped: usize,
    /// 其中「目标已存在但内容与源（替换后）不一致」的个数。
    pub mismatched: usize,
    /// 附属目录根下存在、但没有复制的条目数（白名单之外的目录、根下的散落
    /// 文件、符号链接）。
    ///
    /// 存在的意义是「不静默」：客户端新增一种子目录时，这里会跳起来，而不是让
    /// 复制长期漏掉一半材料却永远绿灯。
    pub ignored_entries: usize,
    /// 跳过/忽略的说明。
    pub notes: Vec<String>,
    /// 单个文件失败的原因（不中断整体）。
    pub errors: Vec<String>,
}

impl SidecarOutcome {
    /// 本次实际写入的文件总数。
    pub fn copied_total(&self) -> usize {
        self.agents_copied + self.tool_results_copied + self.other_copied
    }

    /// 是否有失败项。
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// 是否什么都没做（源侧没有附属目录）。
    pub fn is_noop(&self) -> bool {
        !self.source_present && self.copied_total() == 0 && self.skipped == 0
    }
}

/// 文件类别：决定复制计数归到哪一栏。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileKind {
    /// `subagents/` 直属的子 agent transcript。
    Agent,
    /// 任意层级 `tool-results/` 下的工具结果文件。
    ToolResult,
    /// 其余文件。
    Other,
}

/// 复制过程中的可变状态。
struct CopyContext<'a> {
    source_cid: &'a str,
    target_cid: &'a str,
    outcome: &'a mut SidecarOutcome,
}

/// 附属目录位置：与主正文同级、以会话 id 命名的目录。
///
/// 主正文是 `{工作区}/{会话id}.jsonl`，附属目录是 `{工作区}/{会话id}/`。
fn sidecar_dir_of(body_path: &Path, cid: &str) -> Result<PathBuf, String> {
    let parent = body_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| "会话正文路径缺少上级目录，无法定位附属目录".to_string())?;
    Ok(parent.join(cid))
}

/// 复制一个会话的附属目录（`subagents/` 与 `tool-results/`）到目标会话名下。
///
/// `source_body_path` / `target_body_path` 是**主正文**路径；附属目录由它们的
/// 上级目录加会话 id 推导（见模块文档的目录布局）。
///
/// 返回复制结果摘要：源侧没有附属目录是**成功且计数为 0** 的正常情况；
/// 单个文件失败记进 [`SidecarOutcome::errors`] 并继续处理其余文件。
pub fn copy_sidecar_dir(
    source_body_path: &Path,
    target_body_path: &Path,
    source_cid: &str,
    target_cid: &str,
) -> Result<SidecarOutcome, String> {
    let source_sidecar = sidecar_dir_of(source_body_path, source_cid)?;
    let target_sidecar = sidecar_dir_of(target_body_path, target_cid)?;
    debug_assert_sidecar_named_after(&target_sidecar, target_cid);
    let mut outcome = SidecarOutcome::default();
    if !source_sidecar.is_dir() {
        // 绝大多数会话没有子 agent：没有附属目录是正常情况，不是错误。
        return Ok(outcome);
    }
    outcome.source_present = true;
    let entries = match std::fs::read_dir(&source_sidecar) {
        Ok(entries) => entries,
        Err(error) => {
            // 根读不了是「读失败」，不能当成「没有附属材料」。
            outcome.errors.push(format!(
                "附属目录无法读取（{}）：{error}",
                display_name(&source_sidecar)
            ));
            return Ok(outcome);
        }
    };
    // 稳定顺序：同样的输入产出同样的报告顺序，便于人工核对。
    let mut names: Vec<OsString> = entries.flatten().map(|entry| entry.file_name()).collect();
    names.sort();
    let mut ctx = CopyContext {
        source_cid,
        target_cid,
        outcome: &mut outcome,
    };
    for name in names {
        // 表驱动：命中白名单才处理，表外条目计数 + 说明（绝不静默丢弃）。
        let Some(kind) = sidecar_subdir_kind(&name) else {
            ctx.outcome.ignored_entries += 1;
            ctx.outcome.notes.push(format!(
                "忽略白名单之外的附属条目：{}",
                name.to_string_lossy()
            ));
            continue;
        };
        let source_dir = source_sidecar.join(&name);
        let metadata = match std::fs::symlink_metadata(&source_dir) {
            Ok(metadata) => metadata,
            Err(error) => {
                ctx.outcome.errors.push(format!(
                    "附属条目无法读取（{}）：{error}",
                    name.to_string_lossy()
                ));
                continue;
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            // 白名单名字但形态不对（根下的散落文件、指向别处的符号链接）：
            // 不跟随、不复制，如实计入忽略。
            ctx.outcome.ignored_entries += 1;
            ctx.outcome.notes.push(format!(
                "忽略非目录或符号链接条目：{}",
                name.to_string_lossy()
            ));
            continue;
        }
        let target_dir = target_sidecar.join(&name);
        if let Err(error) = std::fs::create_dir_all(&target_dir) {
            ctx.outcome.errors.push(format!(
                "附属目录创建失败（{}）：{error}",
                name.to_string_lossy()
            ));
            continue;
        }
        copy_dir(&source_dir, &target_dir, kind, &mut ctx);
    }
    Ok(outcome)
}

/// 表驱动查找：名字命中白名单就返回该子目录的文件类别。
fn sidecar_subdir_kind(name: &OsStr) -> Option<FileKind> {
    SIDECAR_SUBDIRS
        .iter()
        .find(|entry| OsStr::new(entry.0) == name)
        .map(|entry| entry.1)
}

/// 目标附属目录必须以目标会话 id 为前缀（模块文档硬约束 1）。
///
/// 为什么用 `debug_assert` 而不是测试断言：这条不变量由**每一个**调用方
/// （复制 / 同步 / 补齐三条路径）共享，debug 下当场炸掉能立刻定位到调用点；
/// 测试只能复验 [`sidecar_dir_of`] 自己的拼接，抓不到「有人给目标目录加了后缀」
/// 这类改动。release 下 `cfg!(debug_assertions)` 为 false，零运行时成本。
fn debug_assert_sidecar_named_after(dir: &Path, cid: &str) {
    debug_assert!(
        dir.file_name()
            .and_then(OsStr::to_str)
            .map_or(false, |name| name.starts_with(cid)),
        "附属目录必须以目标会话 id 为前缀：{}",
        dir.display()
    );
}

/// 复制一个目录的内容（递归）。
///
/// `kind` 描述的是**本目录直属文件**的类别；子目录按名字重新判定。
fn copy_dir(source: &Path, target: &Path, kind: FileKind, ctx: &mut CopyContext) {
    let entries = match std::fs::read_dir(source) {
        Ok(entries) => entries,
        Err(error) => {
            ctx.outcome.errors.push(format!(
                "附属目录无法读取（{}）：{error}",
                display_name(source)
            ));
            return;
        }
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    // 稳定顺序：同样的输入产出同样的报告顺序，便于人工核对。
    paths.sort();
    for path in paths {
        let Some(name) = path.file_name().map(OsStr::to_os_string) else {
            continue;
        };
        if !is_safe_component(&name) {
            // 路径逃逸防护：名字里带分隔符或 `.` / `..` 的条目一律不碰。
            ctx.outcome
                .notes
                .push(format!("忽略不安全条目名：{}", name.to_string_lossy()));
            continue;
        }
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                ctx.outcome.errors.push(format!(
                    "附属条目无法读取（{}）：{error}",
                    name.to_string_lossy()
                ));
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            // 不跟随符号链接：它可能指向目标目录之外。
            ctx.outcome
                .notes
                .push(format!("忽略符号链接：{}", name.to_string_lossy()));
            continue;
        }
        let target_path = target.join(&name);
        if metadata.is_dir() {
            if kind == FileKind::ToolResult {
                // `tool-results/` 是扁平目录：出现子目录说明源布局不是预期结构，
                // 记成错误并跳过，不把未知结构带进目标侧。
                ctx.outcome.errors.push(format!(
                    "工具结果目录内出现子目录，未复制：{}",
                    display_name(&path)
                ));
                continue;
            }
            if let Err(error) = std::fs::create_dir_all(&target_path) {
                ctx.outcome.errors.push(format!(
                    "附属目录创建失败（{}）：{error}",
                    name.to_string_lossy()
                ));
                continue;
            }
            let child_kind = if name == OsStr::new(SUBDIR_TOOL_RESULTS) {
                FileKind::ToolResult
            } else {
                FileKind::Other
            };
            copy_dir(&path, &target_path, child_kind, ctx);
        } else {
            copy_file(&path, &target_path, kind, ctx);
        }
    }
}

/// 复制单个文件：内容做会话 id 替换，目标已存在则跳过（绝不覆盖）。
fn copy_file(source: &Path, target: &Path, kind: FileKind, ctx: &mut CopyContext) {
    let name = source
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let bytes = match std::fs::read(source) {
        Ok(bytes) => bytes,
        Err(error) => {
            ctx.outcome
                .errors
                .push(format!("附属文件读取失败（{name}）：{error}"));
            return;
        }
    };
    let content = replace_session_id(bytes, ctx.source_cid, ctx.target_cid);
    if std::fs::symlink_metadata(target).is_ok() {
        // 目标已有同名文件：绝不覆盖（目标侧自己也会产生子 agent 记录）。
        ctx.outcome.skipped += 1;
        let identical = std::fs::read(target).map(|old| old == content).unwrap_or(false);
        if !identical {
            ctx.outcome.mismatched += 1;
            ctx.outcome
                .notes
                .push(format!("目标已存在且内容不同，未覆盖：{name}"));
        }
        return;
    }
    if let Some(parent) = target.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            ctx.outcome.errors.push(format!(
                "附属文件目录创建失败（{name}）：{error}"
            ));
            return;
        }
    }
    // 附属文件同样走会话专用可靠落盘（sync_all + rename 原子替换）。
    if let Err(error) = session_backup::durable_write(target, &content) {
        ctx.outcome
            .errors
            .push(format!("附属文件保存失败（{name}）：{error}"));
        return;
    }
    match kind {
        FileKind::Agent => ctx.outcome.agents_copied += 1,
        FileKind::ToolResult => ctx.outcome.tool_results_copied += 1,
        FileKind::Other => ctx.outcome.other_copied += 1,
    }
}

/// 内容里的会话 id 替换：文本文件按字符串替换，非文本文件原样返回。
pub fn replace_session_id(bytes: Vec<u8>, source_cid: &str, target_cid: &str) -> Vec<u8> {
    match String::from_utf8(bytes) {
        Ok(text) => text.replace(source_cid, target_cid).into_bytes(),
        // 非 UTF-8（二进制）原样复制：不做任何猜测性改写。
        Err(error) => error.into_bytes(),
    }
}

/// 附属目录复制的「绝不致命」入口：结构性错误也只进 [`SidecarOutcome::errors`]。
///
/// 附属目录是渲染增强材料、不是业务完成门禁：主正文已经可用，附属目录缺失只
/// 让子 agent 面板空白（客户端把读不到文件吞成空列表，不报错）。让附属目录的
/// 失败把整次复制/同步判死，代价远大于收益，因此调用方统一走这个入口，
/// 把失败如实呈现而不是中断流程。
pub fn copy_sidecar_best_effort(
    source_body_path: &Path,
    target_body_path: &Path,
    source_cid: &str,
    target_cid: &str,
) -> SidecarOutcome {
    match copy_sidecar_dir(source_body_path, target_body_path, source_cid, target_cid) {
        Ok(outcome) => outcome,
        Err(reason) => SidecarOutcome {
            errors: vec![reason],
            ..SidecarOutcome::default()
        },
    }
}

/// 路径逃逸防护：条目名必须是单个普通名字。
///
/// 不允许空名、`.`、`..`，也不允许任何路径分隔符——从目录读取出来的名字
/// 天然满足后半条，这里显式校验是为了让「名字来自外部」时也守得住。
pub fn is_safe_component(name: &OsStr) -> bool {
    let Some(text) = name.to_str() else {
        return false;
    };
    !text.is_empty()
        && text != "."
        && text != ".."
        && !text.contains('/')
        && !text.contains('\\')
}

/// 目录/文件的可读名（用于报告）：取最后一段路径。
fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    //! 附属目录复制的行为单测：全部在临时目录里构造，绝不碰真实数据根。

    use super::*;

    const SOURCE_CID: &str = "11111111-1111-4111-8111-111111111111";
    const TARGET_CID: &str = "22222222-2222-4222-8222-222222222222";

    struct Env {
        root: PathBuf,
        workspace: PathBuf,
    }

    impl Env {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "wb_switch_sidecar_test_{}_{name}",
                uuid::Uuid::new_v4().simple()
            ));
            let workspace = root.join("ws-a");
            std::fs::create_dir_all(&workspace).unwrap();
            Env { root, workspace }
        }

        /// 源主正文路径（并落一个占位正文）。
        fn source_body(&self) -> PathBuf {
            let path = self.workspace.join(format!("{SOURCE_CID}.jsonl"));
            std::fs::write(&path, "{\"kind\":\"user\"}\n").unwrap();
            path
        }

        /// 目标主正文路径（不落盘：附属目录复制不依赖目标正文存在）。
        fn target_body(&self) -> PathBuf {
            self.workspace.join(format!("{TARGET_CID}.jsonl"))
        }

        /// 源附属目录。
        fn source_sidecar(&self) -> PathBuf {
            self.workspace.join(SOURCE_CID)
        }

        /// 目标附属目录。
        fn target_sidecar(&self) -> PathBuf {
            self.workspace.join(TARGET_CID)
        }

        /// 写源侧附属文件（相对附属目录根）。
        fn write_source(&self, relative: &str, content: &str) -> PathBuf {
            let path = self.source_sidecar().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
            path
        }

        /// 读目标侧附属文件（相对附属目录根）。
        fn read_target(&self, relative: &str) -> String {
            std::fs::read_to_string(self.target_sidecar().join(relative)).unwrap()
        }

        fn target_exists(&self, relative: &str) -> bool {
            self.target_sidecar().join(relative).exists()
        }
    }

    impl Drop for Env {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn copy_sidecar_reports_no_sidecar_as_success() {
        let env = Env::new("no_sidecar");
        let outcome =
            copy_sidecar_dir(&env.source_body(), &env.target_body(), SOURCE_CID, TARGET_CID)
                .unwrap();
        assert!(!outcome.source_present);
        assert_eq!(outcome.copied_total(), 0);
        assert_eq!(outcome.skipped, 0);
        assert!(outcome.errors.is_empty());
        assert!(outcome.is_noop());
        // 目标侧不该凭空多出目录。
        assert!(!env.target_sidecar().exists());
    }

    #[test]
    fn copy_sidecar_replaces_session_id_in_agent_files() {
        let env = Env::new("agents");
        let body = format!(
            "{{\"sessionId\":\"agent-abc\",\"rootSessionId\":\"{SOURCE_CID}\",\"path\":\"C:\\\\ws\\\\{SOURCE_CID}\\\\tool-results\\\\x.txt\"}}\n"
        );
        env.write_source(
            "subagents/agent-abc.jsonl",
            &body,
        );
        env.write_source("subagents/agent-abc/tool-results/out.txt", "hello");

        let outcome =
            copy_sidecar_dir(&env.source_body(), &env.target_body(), SOURCE_CID, TARGET_CID)
                .unwrap();
        assert!(outcome.source_present);
        assert_eq!(outcome.agents_copied, 1);
        assert_eq!(outcome.tool_results_copied, 1);
        assert_eq!(outcome.other_copied, 0);
        assert!(outcome.errors.is_empty());

        // 真实落盘内容：主会话 id 必须已被替换成新 id。
        let copied = env.read_target("subagents/agent-abc.jsonl");
        assert!(!copied.contains(SOURCE_CID), "落盘内容仍含源会话 id");
        assert!(copied.contains(TARGET_CID));
        assert!(copied.contains(&format!("rootSessionId\":\"{TARGET_CID}\"")));
        assert_eq!(env.read_target("subagents/agent-abc/tool-results/out.txt"), "hello");
    }

    #[test]
    fn copy_sidecar_skips_existing_target_file() {
        let env = Env::new("skip_existing");
        env.write_source("subagents/agent-abc.jsonl", "from-source");
        std::fs::create_dir_all(env.target_sidecar().join("subagents")).unwrap();
        std::fs::write(
            env.target_sidecar().join("subagents/agent-abc.jsonl"),
            "target-own-content",
        )
        .unwrap();

        let outcome =
            copy_sidecar_dir(&env.source_body(), &env.target_body(), SOURCE_CID, TARGET_CID)
                .unwrap();
        assert_eq!(outcome.agents_copied, 0);
        assert_eq!(outcome.skipped, 1);
        assert_eq!(outcome.mismatched, 1);
        assert!(!outcome.notes.is_empty());
        // 目标侧内容保持原样：绝不覆盖。
        assert_eq!(
            env.read_target("subagents/agent-abc.jsonl"),
            "target-own-content"
        );
    }

    #[test]
    fn copy_sidecar_existing_identical_file_is_skip_not_mismatch() {
        let env = Env::new("skip_same");
        env.write_source("tool-results/call.txt", "same");
        std::fs::create_dir_all(env.target_sidecar().join("tool-results")).unwrap();
        std::fs::write(env.target_sidecar().join("tool-results/call.txt"), "same").unwrap();

        let outcome =
            copy_sidecar_dir(&env.source_body(), &env.target_body(), SOURCE_CID, TARGET_CID)
                .unwrap();
        assert_eq!(outcome.tool_results_copied, 0);
        assert_eq!(outcome.skipped, 1);
        assert_eq!(outcome.mismatched, 0);
    }

    #[test]
    fn copy_sidecar_copies_tool_results() {
        let env = Env::new("tool_results");
        env.write_source("tool-results/call_1-render.txt", "{\"content\":\"ok\"}");
        env.write_source("tool-results/call_2-render.txt", "{\"content\":\"ok2\"}");

        let outcome =
            copy_sidecar_dir(&env.source_body(), &env.target_body(), SOURCE_CID, TARGET_CID)
                .unwrap();
        assert_eq!(outcome.tool_results_copied, 2);
        assert_eq!(outcome.agents_copied, 0);
        assert!(env.target_exists("tool-results/call_1-render.txt"));
        assert!(env.target_exists("tool-results/call_2-render.txt"));
        assert_eq!(
            env.read_target("tool-results/call_1-render.txt"),
            "{\"content\":\"ok\"}"
        );
    }

    #[test]
    fn copy_sidecar_single_failure_does_not_abort_others() {
        let env = Env::new("one_failure");
        env.write_source("subagents/agent-abc.jsonl", "content-a");
        // 工具结果目录里放一个子目录：源布局异常 → 记错误，不复制。
        std::fs::create_dir_all(env.source_sidecar().join("tool-results/odd-dir")).unwrap();
        env.write_source("tool-results/ok.txt", "content-b");

        let outcome =
            copy_sidecar_dir(&env.source_body(), &env.target_body(), SOURCE_CID, TARGET_CID)
                .unwrap();
        assert_eq!(outcome.agents_copied, 1);
        assert_eq!(outcome.tool_results_copied, 1);
        assert_eq!(outcome.errors.len(), 1);
        assert!(outcome.errors[0].contains("工具结果目录内出现子目录"));
        // 其余文件确实落盘了。
        assert_eq!(env.read_target("subagents/agent-abc.jsonl"), "content-a");
        assert_eq!(env.read_target("tool-results/ok.txt"), "content-b");
    }

    /// 单个文件**读取失败**必须记进 errors，并且不中断其余文件的复制。
    ///
    /// Windows 上用独占共享锁制造读失败（`share_mode(0)` 让任何其它打开都撞
    /// sharing violation）；Unix 上用 `chmod 000`。
    #[cfg(windows)]
    #[test]
    fn copy_sidecar_unreadable_file_is_recorded_and_others_continue() {
        use std::os::windows::fs::OpenOptionsExt;

        let env = Env::new("read-failure");
        env.write_source("subagents/agent-abc.jsonl", "content-a");
        env.write_source("subagents/agent-xyz.jsonl", "content-b");
        // 独占打开：别的句柄再读同一个文件会失败。
        let _lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(env.source_sidecar().join("subagents/agent-abc.jsonl"))
            .unwrap();

        let outcome =
            copy_sidecar_dir(&env.source_body(), &env.target_body(), SOURCE_CID, TARGET_CID)
                .unwrap();
        assert_eq!(outcome.errors.len(), 1, "{outcome:?}");
        assert!(
            outcome.errors[0].contains("附属文件读取失败"),
            "{outcome:?}"
        );
        // 其余文件照常复制。
        assert_eq!(outcome.agents_copied, 1, "{outcome:?}");
        assert_eq!(env.read_target("subagents/agent-xyz.jsonl"), "content-b");
    }

    /// 单个文件**读取失败**必须记进 errors，并且不中断其余文件的复制（Unix 版）。
    #[cfg(unix)]
    #[test]
    fn copy_sidecar_unreadable_file_is_recorded_and_others_continue() {
        use std::os::unix::fs::PermissionsExt;

        let env = Env::new("read-failure");
        env.write_source("subagents/agent-abc.jsonl", "content-a");
        env.write_source("subagents/agent-xyz.jsonl", "content-b");
        std::fs::set_permissions(
            env.source_sidecar().join("subagents/agent-abc.jsonl"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();

        let outcome =
            copy_sidecar_dir(&env.source_body(), &env.target_body(), SOURCE_CID, TARGET_CID)
                .unwrap();
        assert_eq!(outcome.errors.len(), 1, "{outcome:?}");
        assert!(
            outcome.errors[0].contains("附属文件读取失败"),
            "{outcome:?}"
        );
        // 其余文件照常复制。
        assert_eq!(outcome.agents_copied, 1, "{outcome:?}");
        assert_eq!(env.read_target("subagents/agent-xyz.jsonl"), "content-b");
    }

    #[test]
    fn copy_sidecar_ignores_non_whitelisted_entries() {
        let env = Env::new("whitelist");
        env.write_source("subagents/agent-abc.jsonl", "content-a");
        env.write_source("evil.txt", "evil");
        env.write_source("other-dir/inside.txt", "evil");
        env.write_source("..evil/inside.txt", "evil");

        let outcome =
            copy_sidecar_dir(&env.source_body(), &env.target_body(), SOURCE_CID, TARGET_CID)
                .unwrap();
        assert_eq!(outcome.agents_copied, 1);
        assert_eq!(outcome.other_copied, 0);
        assert!(!env.target_exists("evil.txt"));
        assert!(!env.target_exists("other-dir/inside.txt"));
        assert!(!env.target_exists("..evil/inside.txt"));
        // 三个表外条目（散落文件 + 两个目录）都要被计数，不能静默消失。
        assert_eq!(outcome.ignored_entries, 3, "{outcome:?}");
    }

    /// 白名单之外的**目录**（客户端将来新增的 `plans/`）必须可见：
    /// 不复制进目标，但要计入 `ignored_entries` 并在 notes 里留名。
    #[test]
    fn copy_sidecar_counts_unknown_subdir_as_ignored() {
        let env = Env::new("ignored_subdir");
        env.write_source("subagents/agent-abc.jsonl", "content-a");
        env.write_source("plans/plan-1.md", "not session material");

        let outcome =
            copy_sidecar_dir(&env.source_body(), &env.target_body(), SOURCE_CID, TARGET_CID)
                .unwrap();
        // 已知目录照常复制。
        assert_eq!(outcome.agents_copied, 1, "{outcome:?}");
        // 未知目录不复制……
        assert!(!env.target_exists("plans/plan-1.md"));
        // ……但要被如实计数并留说明：新目录不会静默吞掉。
        assert_eq!(outcome.ignored_entries, 1, "{outcome:?}");
        assert!(
            outcome.notes.iter().any(|note| note.contains("plans")),
            "{outcome:?}"
        );
        assert!(outcome.errors.is_empty(), "{outcome:?}");
    }

    /// 白名单**名字**但形态不对（符号链接）不计入复制：绝不跟随到目录之外。
    #[cfg(unix)]
    #[test]
    fn copy_sidecar_does_not_follow_symlinked_subdir() {
        let env = Env::new("symlink_subdir");
        let outside = env.root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "secret").unwrap();
        std::fs::create_dir_all(env.source_sidecar()).unwrap();
        std::os::unix::fs::symlink(&outside, env.source_sidecar().join("subagents")).unwrap();

        let outcome =
            copy_sidecar_dir(&env.source_body(), &env.target_body(), SOURCE_CID, TARGET_CID)
                .unwrap();
        assert_eq!(outcome.agents_copied, 0, "{outcome:?}");
        assert_eq!(outcome.ignored_entries, 1, "{outcome:?}");
        assert!(!env.target_exists("subagents/secret.txt"));
    }

    #[test]
    fn is_safe_component_rejects_escaping_names() {
        assert!(is_safe_component(OsStr::new("agent-abc.jsonl")));
        assert!(!is_safe_component(OsStr::new("../evil")));
        assert!(!is_safe_component(OsStr::new("..\\evil")));
        assert!(!is_safe_component(OsStr::new("a/b")));
        assert!(!is_safe_component(OsStr::new("a\\b")));
        assert!(!is_safe_component(OsStr::new("")));
        assert!(!is_safe_component(OsStr::new(".")));
        assert!(!is_safe_component(OsStr::new("..")));
    }

    #[test]
    fn copy_sidecar_is_idempotent() {        let env = Env::new("idempotent");
        env.write_source("subagents/agent-abc.jsonl", "content-a");
        env.write_source("tool-results/call.txt", "content-b");

        let first =
            copy_sidecar_dir(&env.source_body(), &env.target_body(), SOURCE_CID, TARGET_CID)
                .unwrap();
        assert_eq!(first.agents_copied, 1);
        assert_eq!(first.tool_results_copied, 1);
        assert_eq!(first.skipped, 0);

        let second =
            copy_sidecar_dir(&env.source_body(), &env.target_body(), SOURCE_CID, TARGET_CID)
                .unwrap();
        assert_eq!(second.agents_copied, 0);
        assert_eq!(second.tool_results_copied, 0);
        assert_eq!(second.skipped, 2);
        assert_eq!(second.mismatched, 0);
        assert!(second.errors.is_empty());
        // 内容没被第二次改动。
        assert_eq!(env.read_target("subagents/agent-abc.jsonl"), "content-a");
    }

    #[test]
    fn copy_sidecar_copies_binary_unchanged() {
        let env = Env::new("binary");
        let bytes = vec![0xff, 0xfe, 0x00, 0x01];
        let path = env.source_sidecar().join("tool-results");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("blob.bin"), &bytes).unwrap();

        let outcome =
            copy_sidecar_dir(&env.source_body(), &env.target_body(), SOURCE_CID, TARGET_CID)
                .unwrap();
        assert_eq!(outcome.tool_results_copied, 1);
        let copied = std::fs::read(env.target_sidecar().join("tool-results/blob.bin")).unwrap();
        assert_eq!(copied, bytes);
    }

    #[test]
    fn replace_session_id_only_touches_session_id() {
        let replaced = replace_session_id(
            format!("before {SOURCE_CID} after").into_bytes(),
            SOURCE_CID,
            TARGET_CID,
        );
        assert_eq!(String::from_utf8(replaced).unwrap(), format!("before {TARGET_CID} after"));
    }
}
