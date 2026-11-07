//! 整份镜像切换（Wave 2 批次①）：只读预览 + 契约冻结，**零会话/记忆数据写入**。
//!
//! 语义：以「当前登录账号」为母本，切号时把目标账号的会话集合整份对齐成 100% 一致。
//! 本批次只交付只读预览端点与冻结契约；软删、复制、MEMORY.md 覆盖、还原端点属于批次②③，
//! 此文件绝不写 `sessions` 表、不置 `deleted_at`、不碰正文与 MEMORY.md。
//!
//! 两条安全不变量：
//! - 母本 uid 一律取自该档位登录态文件（`current_user_uid_at`），**绝不接受前端传入**；
//!   前端 `motherUid` 参数仅用于界面回显，宿主层必须忽略。
//! - 目标清单只从 `user_id = 目标 uid` 且 `deleted_at IS NULL` 的行生成（含未建组的私有
//!   会话，逐条不折叠）；身份按 `LinkMember.uid` 精确匹配，绝不按标题全局去重（11-06 误删教训）。

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::modules::auth_file;
use crate::modules::config::now_ms;
use crate::modules::memory;
use crate::modules::session::{
    self, current_user_uid_at, open_db, table_exists, SessionPaths, REASON_PREVIEW_STALE,
};
use crate::modules::session_backup;
use crate::modules::session_link::{
    self, full_digest_of, LinkGroup, MemberState, PreviewBinding, PreviewMemberBinding, PreviewToken,
    SyncVerdict,
};
use crate::modules::variant::WbVariant;

/// 镜像预览凭据的 group_id 哨兵：与任何真实 LinkGroup.id 都不同，仅供 `verify_preview` 承载绑定。
pub const MIRROR_GROUP_ID: &str = "__wb_mirror__";

// ---------------------------------------------------------------------------
// 只读契约类型（serde camelCase 与 src/lib/types.ts 逐字对齐）
// ---------------------------------------------------------------------------

/// 逐条列出的「目标账号将被软删」会话；清单不折叠。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorTarget {
    pub session_id: String,
    pub title: String,
    /// 目标会话归属的 user_id；清单只从目标 uid 的行生成。
    pub user_id: String,
    /// 所属关联组 id；未建立 LinkGroup 的私有会话为空串。
    pub group_id: String,
    /// 预览时目标会话行的 updated_at，落盘前现场复核。
    pub updated_at: i64,
    /// 预览时目标会话的规范化摘要，落盘前逐条重算，不符零写入。
    pub full_digest: String,
    /// 会话级软删；镜像不引入记录级删除。
    pub kind: String,
}

/// 独立只读预览结果：凭证绑定 motherUid（当前登录账号），变化一律 REASON_PREVIEW_STALE。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorPreview {
    pub mother_uid: String,
    pub mother_label: String,
    pub preview_token: String,
    /// 待删会话集合规范化摘要（sessionId 有序 + 各自 fullDigest + updatedAt + deletedAt 均为 NULL）。
    pub expected_digest: String,
    /// 逐条待软删清单，界面不得折叠。
    pub targets: Vec<MirrorTarget>,
    pub session_count: usize,
    /// 将从母本复制的会话数（母本有、目标没有，按 LinkGroup 判重）。
    pub copied_count: usize,
    /// 执行前预登记的备份目录（预览阶段恒 null）。
    pub backup_dir: Option<String>,
}

/// 冻结的执行计划：作为 switchAccount 的独立参数并入 switch_account 主干（批次②③接线）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorPlan {
    /// 前端已不再下发母本（批次②a）；`#[serde(default)]` 让字段缺省合法，仅兼容旧载荷。
    /// 执行时**恒以登录态重取母本**（`current_user_uid_at`），本字段被忽略，绝不作作用域来源。
    #[serde(default)]
    pub mother_uid: String,
    pub targets: Vec<MirrorTarget>,
    pub expected_digest: String,
    pub preview_token: String,
}

/// 镜像切换执行报告（批次②③产出）：备份路径明文、中间态可还原、绝不伪装成成功。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorSkipped {
    pub session_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorError {
    pub session_id: String,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorReport {
    pub deleted_sessions: usize,
    pub copied_sessions: usize,
    /// 还原时：把本次「复制新增」的会话按软删撤销的条数（rollback 时填充）。
    #[serde(default)]
    pub restored_copies: usize,
    pub backup: Option<String>,
    pub needs_recovery: bool,
    pub warnings: Vec<String>,
    pub skipped: Vec<MirrorSkipped>,
    pub errors: Vec<MirrorError>,
}

// ---------------------------------------------------------------------------
// 现场绑定与 STALE 六条（纯函数，零 IO，供批次②③执行前逐条复核）
// ---------------------------------------------------------------------------

/// 镜像预览现场快照。执行前用同一构造函数重算 `live`，两者逐字段比对。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorBinding {
    /// 母本 uid（当前登录账号）。
    pub mother_uid: String,
    /// 目标 uid。
    pub target_uid: String,
    /// 母本在所有关联组里的 active 成员集合指纹（group_id|uid|session_id|state 排序去重）。
    pub mother_active_fingerprint: String,
    /// 待删会话集合规范化摘要。
    pub expected_digest: String,
    /// 逐条待删会话现场（sessionId 有序）。
    pub targets: Vec<MirrorTarget>,
    /// 预览涉及的关联组 id（排序去重；私有会话不占）。
    pub group_ids: Vec<String>,
    /// 现场仍能查到的关联组 id。
    pub found_group_ids: Vec<String>,
    /// 现场仍含母本 uid active 成员的组 id。
    pub groups_with_mother: Vec<String>,
    /// 现场待删会话是否仍全部 deleted_at IS NULL。
    pub all_targets_active: bool,
}

/// 一条失配原因：代码恒为 `REASON_PREVIEW_STALE`，detail 给出可读解释。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorStale {
    pub reason_code: String,
    pub detail: String,
}

fn stale(detail: &str) -> MirrorStale {
    MirrorStale {
        reason_code: REASON_PREVIEW_STALE.to_string(),
        detail: detail.to_string(),
    }
}

/// 母本 active 成员集合指纹：任一组成员替换/新增/移除都会改变摘要。
pub fn mother_active_fingerprint(groups: &[LinkGroup], mother_uid: &str) -> String {
    let mut lines: Vec<String> = groups
        .iter()
        .flat_map(|group| {
            group
                .members
                .iter()
                .filter(|member| member.uid == mother_uid && member.state == MemberState::Active)
                .map(move |member| {
                    format!(
                        "{}|{}|{}|{}",
                        group.id,
                        member.uid,
                        member.session_id,
                        member.state.as_str()
                    )
                })
        })
        .collect();
    lines.sort();
    lines.dedup();
    full_digest_of(lines.join("\n").as_bytes())
}

/// 待删会话集合规范化摘要：sessionId 有序 + 各自 fullDigest + updatedAt + deletedAt=NULL。
/// 顺序无关——同一集合任意排列得到同一摘要。
pub fn expected_digest_of(targets: &[MirrorTarget]) -> String {
    let mut lines: Vec<String> = targets
        .iter()
        .map(|target| {
            format!(
                "{}|{}|{}|NULL",
                target.session_id, target.full_digest, target.updated_at
            )
        })
        .collect();
    lines.sort();
    full_digest_of(lines.join("\n").as_bytes())
}

/// 从预览现场构造绑定。`found_group_ids` / `groups_with_mother` / `all_targets_active`
/// 由调用方按现场实况填入（预览时前两者恒等于 group_ids、后者恒 true）。
#[allow(clippy::too_many_arguments)]
pub fn build_mirror_binding(
    mother_uid: &str,
    target_uid: &str,
    groups: &[LinkGroup],
    targets: &[MirrorTarget],
    found_group_ids: Vec<String>,
    groups_with_mother: Vec<String>,
    all_targets_active: bool,
) -> MirrorBinding {
    let mut group_ids: Vec<String> = targets
        .iter()
        .map(|target| target.group_id.clone())
        .filter(|id| !id.is_empty())
        .collect();
    group_ids.sort();
    group_ids.dedup();
    MirrorBinding {
        mother_uid: mother_uid.to_string(),
        target_uid: target_uid.to_string(),
        mother_active_fingerprint: mother_active_fingerprint(groups, mother_uid),
        expected_digest: expected_digest_of(targets),
        targets: targets.to_vec(),
        group_ids,
        found_group_ids,
        groups_with_mother,
        all_targets_active,
    }
}

fn same_targets(expected: &[MirrorTarget], live: &[MirrorTarget]) -> bool {
    let key = |targets: &[MirrorTarget]| -> Vec<(String, String, i64)> {
        let mut rows: Vec<(String, String, i64)> = targets
            .iter()
            .map(|t| (t.session_id.clone(), t.full_digest.clone(), t.updated_at))
            .collect();
        rows.sort();
        rows
    };
    key(expected) == key(live)
}

/// 核对预览绑定与现场绑定，返回六类失配（空表示一致，可继续）。
///
/// 六条与设计一一对应：
/// 1. motherUid 变化；
/// 2. 母本在任一组 active 成员集合变化；
/// 3. 待删会话 fullDigest / updated_at 变化；
/// 4. 待删会话软删状态变化；
/// 5. group_id 查不到；
/// 6. 母本 uid 不在该组成员中。
pub fn verify_mirror_binding(expected: &MirrorBinding, live: &MirrorBinding) -> Vec<MirrorStale> {
    let mut result = Vec::new();
    if expected.mother_uid != live.mother_uid {
        result.push(stale("母本账号已变化，请重新预览"));
    }
    if expected.mother_active_fingerprint != live.mother_active_fingerprint {
        result.push(stale("母本账号的关联会话集合已变化，请重新预览"));
    }
    if !same_targets(&expected.targets, &live.targets) {
        result.push(stale("待删会话的内容或更新时间已变化，请重新预览"));
    }
    if !live.all_targets_active {
        result.push(stale("待删会话已被软删或移除，请重新预览"));
    }
    if expected
        .group_ids
        .iter()
        .any(|id| !live.found_group_ids.contains(id))
    {
        result.push(stale("会话关联组已不存在，请重新预览"));
    }
    if expected
        .group_ids
        .iter()
        .any(|id| !live.groups_with_mother.contains(id))
    {
        result.push(stale("母本账号已不在该组成员中，请重新预览"));
    }
    result
}

// ---------------------------------------------------------------------------
// 只读清单生成（纯函数，红测直接覆盖隔离 / 判重 / 字段齐备）
// ---------------------------------------------------------------------------

/// 单条会话行（只读查询结果）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorRow {
    pub session_id: String,
    pub user_id: String,
    pub title: Option<String>,
    pub custom_title: Option<String>,
    pub updated_at: i64,
    /// Some = 已软删，绝不进清单；None = deleted_at IS NULL。
    pub deleted_at: Option<i64>,
}

fn nonblank(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn display_title(title: Option<String>, custom_title: Option<String>) -> String {
    nonblank(custom_title)
        .or_else(|| nonblank(title))
        .unwrap_or_else(|| "(无标题)".to_string())
}

/// 会话行规范化摘要：预览与执行前重算用同一口径，不符零写入。
pub fn session_row_digest(row: &MirrorRow) -> String {
    let deleted = row
        .deleted_at
        .map(|value| value.to_string())
        .unwrap_or_else(|| "NULL".to_string());
    let raw = format!(
        "{}\0{}\0{}\0{}\0{}\0{}",
        row.session_id,
        row.user_id,
        row.title.clone().unwrap_or_default(),
        row.custom_title.clone().unwrap_or_default(),
        row.updated_at,
        deleted
    );
    full_digest_of(raw.as_bytes())
}

/// 会话所属关联组：`LinkMember(uid, session_id, state=Active)` 精确匹配；私有会话空串。
pub fn group_id_for(groups: &[LinkGroup], uid: &str, session_id: &str) -> String {
    let mut ids: Vec<String> = groups
        .iter()
        .filter(|group| {
            group.members.iter().any(|member| {
                member.uid == uid
                    && member.session_id == session_id
                    && member.state == MemberState::Active
            })
        })
        .map(|group| group.id.clone())
        .collect();
    ids.sort();
    ids.into_iter().next().unwrap_or_default()
}

/// 从只读行生成逐条待软删清单：`user_id` 隔离（母本 uid 不进）+ 仅未软删行 + 字段齐备。
///
/// **排除母本对端/副本**：目标行所属关联组若含母本 active 成员（例如上一次镜像复制过来的
/// 会话），它不是「目标多余会话」，不得进入软删清单——否则下一次镜像会误删自身副本。
pub fn mirror_targets_from_rows(
    rows: &[MirrorRow],
    target_uid: &str,
    mother_uid: &str,
    groups: &[LinkGroup],
) -> Vec<MirrorTarget> {
    let mut targets: Vec<MirrorTarget> = rows
        .iter()
        .filter(|row| row.user_id == target_uid && row.deleted_at.is_none())
        .filter_map(|row| {
            let group_id = group_id_for(groups, target_uid, &row.session_id);
            if !group_id.is_empty()
                && groups.iter().any(|group| {
                    group.id == group_id
                        && group
                            .members
                            .iter()
                            .any(|m| m.uid == mother_uid && m.state == MemberState::Active)
                })
            {
                return None;
            }
            Some(MirrorTarget {
                session_id: row.session_id.clone(),
                title: display_title(row.title.clone(), row.custom_title.clone()),
                user_id: row.user_id.clone(),
                group_id,
                updated_at: row.updated_at,
                full_digest: session_row_digest(row),
                kind: "session".to_string(),
            })
        })
        .collect();
    targets.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    targets
}

/// 母本有、目标没有的会话数：按 `LinkGroup.id + LinkMember(uid, session_id, state)` 判重，
/// 身份以 `LinkMember.uid` 精确匹配，绝不按标题全局去重。同一组只计一次。
pub fn count_mirror_copies(groups: &[LinkGroup], mother_uid: &str, target_uid: &str) -> usize {
    groups
        .iter()
        .filter(|group| {
            let has_mother = group
                .members
                .iter()
                .any(|m| m.uid == mother_uid && m.state == MemberState::Active);
            let has_target = group
                .members
                .iter()
                .any(|m| m.uid == target_uid && m.state == MemberState::Active);
            has_mother && !has_target
        })
        .count()
}

/// 目标账号缺失的母本会话 id 清单（母本有、目标没有；已关联的不算缺失），去重排序。
/// 身份用 `LinkMember.uid` 精确匹配，绝不按标题全局去重；供复制执行。
pub fn missing_mirror_session_ids(
    groups: &[LinkGroup],
    mother_uid: &str,
    target_uid: &str,
) -> Vec<String> {
    let mut ids: Vec<String> = groups
        .iter()
        .filter(|group| {
            let has_target = group
                .members
                .iter()
                .any(|m| m.uid == target_uid && m.state == MemberState::Active);
            has_target == false
                && group
                    .members
                    .iter()
                    .any(|m| m.uid == mother_uid && m.state == MemberState::Active)
        })
        .flat_map(|group| {
            group
                .members
                .iter()
                .filter(|m| m.uid == mother_uid && m.state == MemberState::Active)
                .map(|m| m.session_id.clone())
        })
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

// ---------------------------------------------------------------------------
// 只读预览端点（唯一 IO 编排；read_only 打开数据库，零会话数据写入）
// ---------------------------------------------------------------------------

fn account_uid_of(acc: &Value) -> String {
    acc.get("uid")
        .and_then(|value| value.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// 登录态展示名；取不到时回退 uid。
pub fn account_label(auth: &Value, uid_fallback: &str) -> String {
    let account = auth.get("account").unwrap_or(auth);
    for key in ["label", "nickname", "email", "name"] {
        if let Some(value) = account.get(key).and_then(Value::as_str) {
            if !value.trim().is_empty() {
                return value.trim().to_string();
            }
        }
    }
    uid_fallback.to_string()
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    let Ok(mut stmt) = conn.prepare(&format!("PRAGMA table_info({table})")) else {
        return false;
    };
    let Ok(iter) = stmt.query_map([], |row| row.get::<_, String>(1)) else {
        return false;
    };
    // 先收进 Vec 释放 `stmt` 借用，避免尾部临时值析构晚于 stmt（E0597）。
    let names: Vec<String> = iter.flatten().collect();
    names.iter().any(|name| name == column)
}

/// 只读查询目标账号全部 `deleted_at IS NULL` 会话（含私有会话，逐条不折叠）。
fn read_target_rows(conn: &Connection, target_uid: &str) -> Result<Vec<MirrorRow>, String> {
    if !table_exists(conn, "sessions") {
        return Ok(Vec::new());
    }
    let custom_expr = if column_exists(conn, "sessions", "custom_title") {
        "custom_title"
    } else {
        "NULL"
    };
    let deleted_filter = if column_exists(conn, "sessions", "deleted_at") {
        " AND deleted_at IS NULL"
    } else {
        ""
    };
    let sql = format!(
        "SELECT id, user_id, title, {custom_expr}, updated_at FROM sessions \
         WHERE user_id = ?1{deleted_filter} ORDER BY id"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|error| format!("镜像预览：会话清单查询失败：{error}"))?;
    let mapped = stmt
        .query_map([target_uid], |row| {
            Ok(MirrorRow {
                session_id: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                user_id: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                title: row.get::<_, Option<String>>(2)?,
                custom_title: row.get::<_, Option<String>>(3)?,
                updated_at: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                deleted_at: None, // SQL 已过滤为 IS NULL
            })
        })
        .map_err(|error| format!("镜像预览：会话清单读取失败：{error}"))?;
    let mut rows = Vec::new();
    for row in mapped {
        rows.push(row.map_err(|error| format!("镜像预览：会话行读取失败：{error}"))?);
    }
    Ok(rows)
}

fn load_groups(paths: &SessionPaths) -> Vec<LinkGroup> {
    match session_link::load_store(paths) {
        session_link::StoreState::Ready(store) => store.groups,
        _ => Vec::new(),
    }
}

/// 构造镜像预览凭据绑定：复用既有 `PreviewBinding`（`mother_uid` 绑定母本，
/// `group_fingerprint` 绑定 expectedDigest），执行时 `load_preview_token` + `verify_preview` 复核。
fn build_mirror_token_binding(
    variant: WbVariant,
    mother_uid: &str,
    target_uid: &str,
    expected_digest: &str,
    groups: &[LinkGroup],
    targets_len: usize,
    copied_count: usize,
) -> PreviewBinding {
    let member = |uid: &str, role: &str, digest: String, count: usize| PreviewMemberBinding {
        member_id: format!("mirror-{role}"),
        account_id: None,
        uid: uid.to_string(),
        session_id: String::new(),
        raw_digest: String::new(),
        normalized_digest: digest,
        record_count: count,
    };
    PreviewBinding {
        variant,
        group_id: MIRROR_GROUP_ID.to_string(),
        group_fingerprint: expected_digest.to_string(),
        source: member(
            mother_uid,
            "mother",
            mother_active_fingerprint(groups, mother_uid),
            copied_count,
        ),
        target: member(
            target_uid,
            "target",
            expected_digest.to_string(),
            targets_len,
        ),
        baseline_ref: None,
        baseline_total_digest: None,
        baseline_record_count: None,
        verdict: SyncVerdict::Identical,
        source_status: None,
        target_status: None,
        archive_action: None,
        mother_uid: Some(mother_uid.to_string()),
    }
}

/// 生产入口：按档位路径只读预览「当前登录账号 → 目标账号」的整份镜像清单。
pub fn mirror_preview(variant: WbVariant, target_acc: &Value) -> Result<Value, String> {
    let preview = mirror_preview_at(&SessionPaths::for_variant(variant), variant, target_acc)?;
    serde_json::to_value(preview).map_err(|error| format!("镜像预览序列化失败：{error}"))
}

/// 路径注入的可测实现：只读数据库（`read_only = true`），唯一落盘是预览凭据 token
/// （既有只读预览机制，非会话/记忆数据），绝不写 sessions / MEMORY.md / 正文。
pub fn mirror_preview_at(
    paths: &SessionPaths,
    variant: WbVariant,
    target_acc: &Value,
) -> Result<MirrorPreview, String> {
    // 母本恒取登录态，杜绝前端伪造 motherUid 扩大作用域。
    let mother_uid = current_user_uid_at(&paths.auth_file)
        .ok_or_else(|| "未读取到本机登录态，无法确定母本账号".to_string())?;
    let target_uid = account_uid_of(target_acc);
    if target_uid.is_empty() {
        return Err("目标账号缺少 uid，无法预览镜像".to_string());
    }
    if mother_uid == target_uid {
        return Err("当前账号与目标账号相同，无需镜像".to_string());
    }

    let conn = open_db(&paths.workbuddy_db(), true)
        .ok_or_else(|| "会话库不可读，无法预览镜像".to_string())?;
    let rows = read_target_rows(&conn, &target_uid)?;
    let groups = load_groups(paths);
    let targets = mirror_targets_from_rows(&rows, &target_uid, &mother_uid, &groups);
    let expected_digest = expected_digest_of(&targets);
    let copied_count = count_mirror_copies(&groups, &mother_uid, &target_uid);

    let binding = build_mirror_token_binding(
        variant,
        &mother_uid,
        &target_uid,
        &expected_digest,
        &groups,
        targets.len(),
        copied_count,
    );
    let preview_token = session_link::save_preview_token(paths, binding)?;
    let auth = auth_file::read_auth_file_at(&paths.auth_file).unwrap_or(Value::Null);
    let mother_label = account_label(&auth, &mother_uid);

    Ok(MirrorPreview {
        mother_uid,
        mother_label,
        preview_token,
        expected_digest,
        session_count: targets.len(),
        copied_count,
        targets,
        backup_dir: None,
    })
}

// ---------------------------------------------------------------------------
// 执行链路（批次②）：软删 + 精确白名单回滚（本批不接线到 switch.rs）
// ---------------------------------------------------------------------------

/// 镜像备份清单版本；与操作目录约定一一对应，读到其它版本一律拒绝。
const MIRROR_BACKUP_VERSION: u32 = 1;
/// 镜像备份清单文件名（固定放在操作专属目录内）。
const MIRROR_MANIFEST_FILE: &str = "mirror-manifest.json";
/// 清单状态：备份已落、软删事务尚未可靠完成（或崩溃窗口），库可能已改但未标记完成。
const MIRROR_STATE_PENDING: &str = "pending";
/// 清单状态：软删事务已 commit 且读回校验通过，可放心一键还原。
const MIRROR_STATE_DONE: &str = "done";
/// 记忆步骤在报告与清单里的标识：它不是会话，不该冒用 `session_id` 语义。
const MEMORY_STEP_ID: &str = "MEMORY.md";

/// 单条待软删会话的备份登记：恢复只按 `(sessionId, userId, deletedAtWritten)` 三元组精确置空。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MirrorBackupRow {
    session_id: String,
    user_id: String,
    group_id: String,
    group_fingerprint: String,
    title: String,
    updated_at_before: i64,
    /// 只登记未软删行，恒为空；保留字段以便未来扩展。
    deleted_at_before: Option<i64>,
    /// 预览时的规范化摘要，供人工核对。
    full_digest: String,
    /// 本次写入的 `deleted_at` 值，恢复时严格按其匹配。
    deleted_at_written: i64,
}

/// 镜像切换备份清单：先落备份再置位；清单读不回来就不算备份成功。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MirrorBackupManifest {
    version: u32,
    operation_id: String,
    variant: WbVariant,
    mother_uid: String,
    target_uid: String,
    preview_token: String,
    expected_digest: String,
    created_at: i64,
    /// `pending`（备份已落、软删/复制未确认完成/崩溃窗口）或 `done`（软删+复制已 commit 且读回校验通过）。
    /// 供 `scan_mirror_backups` 发现「库已改但 UI/启动恢复无感知」的窄窗口。
    #[serde(default = "default_mirror_state")]
    state: String,
    rows: Vec<MirrorBackupRow>,
    /// 本次「复制新增」的会话：回滚时按 ids 软删，禁止静默留下孤儿。
    #[serde(default)]
    copied: Vec<MirrorCopiedEntry>,
    /// 本次 `MEMORY.md` 整份覆盖的备份绝对路径（若有）。
    /// 记忆覆盖与 sessions 软删是两个独立可还原步骤，互不替代。
    #[serde(default)]
    memory_backup: Option<String>,
    restore_steps: Vec<String>,
}

/// 一条「复制新增」登记：源会话 id、目标新 id、归属与创建时刻。回滚据此精确软删新副本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorCopiedEntry {
    pub source_session_id: String,
    pub new_session_id: String,
    pub user_id: String,
    pub group_id: String,
    pub created_at: i64,
}

/// 旧清单缺 `state` 时按 pending 处理（保守：宁可提示可还原，也不当作已完成）。
fn default_mirror_state() -> String {
    MIRROR_STATE_PENDING.to_string()
}

/// 构造「预览时冻结」的绑定：母本、指纹与待删清单都来自预览凭据与计划，绝不信任前端母本。
fn expected_mirror_binding(
    token: &PreviewToken,
    plan: &MirrorPlan,
    target_uid: &str,
) -> MirrorBinding {
    let mother_uid = token.binding.mother_uid.clone().unwrap_or_default();
    let mut group_ids: Vec<String> = plan
        .targets
        .iter()
        .map(|target| target.group_id.clone())
        .filter(|id| !id.is_empty())
        .collect();
    group_ids.sort();
    group_ids.dedup();
    MirrorBinding {
        mother_uid,
        target_uid: target_uid.to_string(),
        mother_active_fingerprint: token.binding.source.normalized_digest.clone(),
        expected_digest: plan.expected_digest.clone(),
        targets: plan.targets.clone(),
        group_ids: group_ids.clone(),
        found_group_ids: group_ids.clone(),
        groups_with_mother: group_ids,
        all_targets_active: true,
    }
}

/// 构造「现场重算」的绑定：所有字段都从当前库与关联表重取，供 `verify_mirror_binding` 逐条比对。
fn live_mirror_binding(
    mother_uid: &str,
    target_uid: &str,
    groups: &[LinkGroup],
    live_targets: &[MirrorTarget],
    expected: &MirrorBinding,
) -> MirrorBinding {
    let found_group_ids: Vec<String> = expected
        .group_ids
        .iter()
        .filter(|id| groups.iter().any(|group| &group.id == *id))
        .cloned()
        .collect();
    let groups_with_mother: Vec<String> = expected
        .group_ids
        .iter()
        .filter(|id| {
            groups.iter().any(|group| {
                &group.id == *id
                    && group.members.iter().any(|member| {
                        member.uid == mother_uid && member.state == MemberState::Active
                    })
            })
        })
        .cloned()
        .collect();
    let all_targets_active = expected
        .targets
        .iter()
        .all(|target| live_targets.iter().any(|live| live.session_id == target.session_id));
    build_mirror_binding(
        mother_uid,
        target_uid,
        groups,
        live_targets,
        found_group_ids,
        groups_with_mother,
        all_targets_active,
    )
}

/// 先落备份清单再置位：在没有备份正文的前提下，复用操作专属目录约定 + 持久化写。
fn write_mirror_manifest(
    paths: &SessionPaths,
    variant: WbVariant,
    operation_id: &str,
    mother_uid: &str,
    target_uid: &str,
    plan: &MirrorPlan,
    groups: &[LinkGroup],
    live_targets: &[MirrorTarget],
    deleted_at_written: i64,
) -> Result<PathBuf, String> {
    let dir = session_backup::transaction_dir(paths, variant, operation_id)?;
    std::fs::create_dir_all(&dir).map_err(|error| format!("镜像备份目录创建失败：{error}"))?;
    let rows: Vec<MirrorBackupRow> = live_targets
        .iter()
        .map(|target| MirrorBackupRow {
            session_id: target.session_id.clone(),
            user_id: target.user_id.clone(),
            group_id: target.group_id.clone(),
            group_fingerprint: groups
                .iter()
                .find(|group| group.id == target.group_id)
                .map(session_link::group_fingerprint)
                .unwrap_or_default(),
            title: target.title.clone(),
            updated_at_before: target.updated_at,
            deleted_at_before: None,
            full_digest: target.full_digest.clone(),
            deleted_at_written,
        })
        .collect();
    let manifest = MirrorBackupManifest {
        version: MIRROR_BACKUP_VERSION,
        operation_id: operation_id.to_string(),
        variant,
        mother_uid: mother_uid.to_string(),
        target_uid: target_uid.to_string(),
        preview_token: plan.preview_token.clone(),
        expected_digest: plan.expected_digest.clone(),
        created_at: now_ms(),
        state: MIRROR_STATE_PENDING.to_string(),
        rows,
        copied: Vec::new(),
        memory_backup: None,
        restore_steps: vec![
            "按清单逐条把 sessions.deleted_at 置空：WHERE id=? AND user_id=? AND deleted_at=<本清单 deletedAtWritten>".to_string(),
            "只还原本次写入值；严禁按 user_id 批量置空（会误恢复用户此前自删的会话）".to_string(),
            "本备份不涉及会话正文：镜像只软删，不写 jsonl，恢复亦不碰正文".to_string(),
        ],
    };
    let content = serde_json::to_string_pretty(&manifest).map_err(|error| error.to_string())?;
    let file = dir.join(MIRROR_MANIFEST_FILE);
    session_backup::durable_write_str(&file, &content)
        .map_err(|error| format!("镜像备份清单保存失败：{error}"))?;
    let read_back = std::fs::read_to_string(&file)
        .map_err(|error| format!("镜像备份清单回读失败：{error}"))?;
    let parsed: MirrorBackupManifest = serde_json::from_str(&read_back)
        .map_err(|error| format!("镜像备份清单回读解析失败：{error}"))?;
    if parsed != manifest {
        return Err("镜像备份清单保存后核验不一致，未按备份成功处理".to_string());
    }
    session_backup::sync_tree(&dir).map_err(|error| format!("镜像备份目录持久化失败：{error}"))?;
    Ok(dir)
}

/// 短事务软删：条件 UPDATE 幂等且防跨账号（`user_id` + `deleted_at IS NULL` 双重前置）。
fn soft_delete_targets(
    conn: &mut Connection,
    target_uid: &str,
    targets: &[MirrorTarget],
    deleted_at: i64,
) -> Result<(usize, Vec<MirrorSkipped>), String> {
    session_backup::ensure_full_synchronous(conn)?;
    let tx = conn
        .transaction()
        .map_err(|error| format!("会话数据事务开启失败：{error}"))?;
    let mut deleted = 0usize;
    let mut skipped = Vec::new();
    for target in targets {
        let affected = tx
            .execute(
                "UPDATE sessions SET deleted_at = ?1 \
                 WHERE id = ?2 AND user_id = ?3 AND deleted_at IS NULL",
                params![deleted_at, target.session_id, target_uid],
            )
            .map_err(|error| format!("软删会话失败（{}）：{error}", target.session_id))?;
        if affected == 1 {
            deleted += 1;
        } else {
            skipped.push(MirrorSkipped {
                session_id: target.session_id.clone(),
                reason: "会话不存在、不属于目标账号或已被软删，已跳过".to_string(),
            });
        }
    }
    tx.commit()
        .map_err(|error| format!("会话数据提交失败：{error}"))?;
    Ok((deleted, skipped))
}

/// 落盘后读回校验：`deleted_at` 必须等于本次写入值，否则报「持久化证明缺失」（禁止反向写回）。
fn verify_soft_deleted(
    conn: &Connection,
    target_uid: &str,
    targets: &[MirrorTarget],
    expected_deleted_at: i64,
) -> Result<(), String> {
    let mut stmt = conn
        .prepare("SELECT deleted_at FROM sessions WHERE id = ?1 AND user_id = ?2")
        .map_err(|error| format!("软删读回校验失败：{error}"))?;
    for target in targets {
        let value = stmt
            .query_row(params![target.session_id, target_uid], |row| {
                row.get::<_, Option<i64>>(0)
            })
            .optional()
            .map_err(|error| format!("软删读回校验失败（{}）：{error}", target.session_id))?;
        match value {
            Some(Some(v)) if v == expected_deleted_at => {}
            other => {
                return Err(format!(
                    "会话 {} 的 deleted_at 读回为 {other:?}，与本次写入值不一致",
                    target.session_id
                ))
            }
        }
    }
    Ok(())
}

/// 执行整份镜像：现场双 verify → 摘要重算 → 先备份再置位 → 软删 → 读回校验 → 复制母本独有会话
/// → `MEMORY.md` 整份覆盖。任一 verify 非空即零写入（连复制与记忆覆盖也不做）。
///
/// `memory_root` 是人格目录根（生产为 `wb_data_root()`）：显式传入而非模块内取全局，
/// 既保证测试不触碰真实用户目录，也让两个档位共用同一份人格目录。
pub fn mirror_apply_at(
    paths: &SessionPaths,
    variant: WbVariant,
    target_uid: &str,
    acc: &Value,
    memory_root: &Path,
    plan: &MirrorPlan,
) -> Result<MirrorReport, String> {
    let target_uid = target_uid.trim();
    if target_uid.is_empty() {
        return Err("目标账号缺少 uid，无法执行镜像".to_string());
    }
    // 母本恒取登录态，忽略 plan.mother_uid（前端已不下发；后端同源 current_user_uid_at）。
    let mother_uid = current_user_uid_at(&paths.auth_file)
        .ok_or_else(|| "未读取到本机登录态，无法执行镜像".to_string())?;
    if mother_uid == target_uid {
        return Err("当前账号与目标账号相同，无需镜像".to_string());
    }
    // 预览凭据是执行授权的唯一依据：缺失即零写入。
    let token = session_link::load_preview_token(paths, &plan.preview_token)
        .ok_or_else(|| "镜像预览凭据已失效，请重新预览".to_string())?;

    let mut conn = open_db(&paths.workbuddy_db(), false)
        .ok_or_else(|| "会话库不可读，无法执行镜像".to_string())?;
    if !table_exists(&conn, "sessions") {
        return Err("会话库缺少 sessions 表，无法执行镜像".to_string());
    }
    let rows = read_target_rows(&conn, target_uid)?;
    let groups = load_groups(paths);
    let live_targets = mirror_targets_from_rows(&rows, target_uid, &mother_uid, &groups);
    let live_expected_digest = expected_digest_of(&live_targets);

    // 硬门禁 2：expectedDigest 逐条重算，与计划不符 → 零写入。
    if live_expected_digest != plan.expected_digest {
        return Err("待删会话集合与预览不一致（内容已变化），已停止镜像；请重新预览".to_string());
    }

    // 硬门禁 3：造镜像专用 live PreviewBinding（哨兵 group_id + mother_uid=Some(重取登录态)、
    // 逐字段重算），同跑 verify_preview；绝不复用逐组同步的 live_preview_binding。
    let live_copied_count = count_mirror_copies(&groups, &mother_uid, target_uid);
    let live_preview_binding = build_mirror_token_binding(
        variant,
        &mother_uid,
        target_uid,
        &live_expected_digest,
        &groups,
        live_targets.len(),
        live_copied_count,
    );
    let preview_stale = session_link::verify_preview(&token, &live_preview_binding);

    // 硬门禁 5：母本唯一权威 = 登录态重取。expected 取凭据绑定母本、live 取当前登录态，
    // 两个 verify 的 live 侧都由同一 mother_uid 构造，口径同源不漂移。
    let expected_binding = expected_mirror_binding(&token, plan, target_uid);
    let live_binding = live_mirror_binding(&mother_uid, target_uid, &groups, &live_targets, &expected_binding);
    let mirror_stale = verify_mirror_binding(&expected_binding, &live_binding);

    if !preview_stale.is_empty() || !mirror_stale.is_empty() {
        let mut details = preview_stale;
        details.extend(mirror_stale.into_iter().map(|item| item.detail));
        return Err(format!(
            "镜像预览已过期，已停止且零写入：{}",
            details.join("；")
        ));
    }

    // 目标账号缺失的母本会话（已关联的不算缺失）：100% 镜像的另一半。
    let missing = missing_mirror_session_ids(&groups, &mother_uid, target_uid);

    // 记忆是否已一致：必须并入「零操作」判定——否则「仅 MEMORY.md 不同」的切号会被整体跳过，
    // 镜像就不算 100%。
    let memory_pending = !memory::memory_already_identical(memory_root, &mother_uid, target_uid);

    // 既无待删、也无待复制、记忆也已一致：无操作、不建备份目录（避免空还原项）。
    if live_targets.is_empty() && missing.is_empty() && !memory_pending {
        return Ok(MirrorReport {
            deleted_sessions: 0,
            copied_sessions: 0,
            restored_copies: 0,
            backup: None,
            needs_recovery: false,
            warnings: Vec::new(),
            skipped: Vec::new(),
            errors: Vec::new(),
        });
    }

    // 先落备份记录再置位（清单同时锚定软删与复制的回滚依据）。
    let deleted_at_written = now_ms();
    let operation_id = uuid::Uuid::new_v4().to_string();
    let backup_dir = write_mirror_manifest(
        paths,
        variant,
        &operation_id,
        &mother_uid,
        target_uid,
        plan,
        &groups,
        &live_targets,
        deleted_at_written,
    )?;

    // 软删（可能 0 行）：单事务 commit，随后读回校验。
    let (deleted, skipped) =
        soft_delete_targets(&mut conn, target_uid, &live_targets, deleted_at_written)?;
    verify_soft_deleted(&conn, target_uid, &live_targets, deleted_at_written).map_err(|detail| {
        format!(
            "已落盘但持久化证明缺失：{detail}；备份保留在 {}，请勿反向写回",
            backup_dir.display()
        )
    })?;

    // 复制母本独有会话：失败不阻断软删，错误进 errors；成功的新副本登记进清单供回滚。
    let (copied_entries, copy_errors, copy_needs_recovery) = if missing.is_empty() {
        (Vec::new(), Vec::new(), false)
    } else {
        copy_missing_sessions(paths, variant, acc, target_uid, &missing)
    };

    // MEMORY.md 整份覆盖（只写目标账号）：失败不阻断已完成的软删/复制；
    // 记忆备份路径登记进同一清单，作为与 sessions 解耦的独立可还原步骤。
    let (memory_backup, memory_errors) = if memory_pending {
        overwrite_target_memory(memory_root, &mother_uid, target_uid)
    } else {
        (None, Vec::new())
    };
    let mut errors = copy_errors;
    errors.extend(memory_errors);

    // 软删+复制+记忆覆盖完成后把清单原子改写为 done；失败仅降级为 warning。
    let mut warnings = Vec::new();
    if let Err(error) =
        finalize_mirror_manifest(&backup_dir, &copied_entries, memory_backup.as_deref())
    {
        warnings.push(format!(
            "镜像已完成但清单状态标记失败（{error}）；备份仍在 {}，扫描会显示为 pending",
            backup_dir.display()
        ));
    }

    // `needs_recovery` 只回填复制侧的真实值（`session.rs` 在「有未完成写入 / 恢复不干净」
    // 时才置 true）——它的语义是「下次切号会先恢复」，不能拿来表达「这次没全成」。
    // 复制在写入前就失败（如源正文不存在）、或多账号 MEMORY.md 覆盖失败，都不产生
    // pending operation，标 true 等于承诺一件不会发生的事；那类情况由前端用
    // `errors` 单独表达为「部分未完成，备份已保留」。
    let needs_recovery = copy_needs_recovery;

    Ok(MirrorReport {
        deleted_sessions: deleted,
        copied_sessions: copied_entries.len(),
        restored_copies: 0,
        backup: Some(backup_dir.to_string_lossy().to_string()),
        needs_recovery,
        warnings,
        skipped,
        errors,
    })
}

/// 调 `copy_sessions_for_switch_at` 复制，解析 `copied`/`errors`（复制失败不阻断软删）。
fn copy_missing_sessions(
    paths: &SessionPaths,
    variant: WbVariant,
    acc: &Value,
    target_uid: &str,
    missing: &[String],
) -> (Vec<MirrorCopiedEntry>, Vec<MirrorError>, bool) {
    // App 已在 switch 主干 close_workbuddy 之后关闭，探针恒 false。
    let report = match session::copy_sessions_for_switch_at(paths, variant, acc, missing, |_| false) {
        Ok(report) => report,
        Err(error) => {
            return (
                Vec::new(),
                vec![MirrorError {
                    session_id: String::new(),
                    error,
                }],
                false,
            )
        }
    };
    let copied_entries: Vec<MirrorCopiedEntry> = report
        .get("copied")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let source = item.get("id").and_then(Value::as_str)?;
                    let new_id = item.get("newId").and_then(Value::as_str)?;
                    Some(MirrorCopiedEntry {
                        source_session_id: source.to_string(),
                        new_session_id: new_id.to_string(),
                        user_id: target_uid.to_string(),
                        group_id: item
                            .get("groupId")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        created_at: now_ms(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let copy_errors: Vec<MirrorError> = report
        .get("errors")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| MirrorError {
                    session_id: item
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    error: item
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    // 复制侧自己的恢复标记：会话持久化不完整时它会是 true，必须原样上报，
    // 不能被镜像层压成「整份镜像完成」。
    let copy_needs_recovery = report
        .get("needsRecovery")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    (copied_entries, copy_errors, copy_needs_recovery)
}

/// 整份覆盖目标账号 `MEMORY.md`：成功回填备份路径，失败折算成 `errors`（不阻断软删/复制）。
///
/// 错误项以 `MEMORY.md` 作为标识（它不是会话，不该冒用 session_id 语义）。
fn overwrite_target_memory(
    memory_root: &Path,
    mother_uid: &str,
    target_uid: &str,
) -> (Option<String>, Vec<MirrorError>) {
    let report = memory::copy_and_overwrite_to_at(memory_root, mother_uid, target_uid);
    let backup = report
        .get("written")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|item| item.get("backup"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let errors: Vec<MirrorError> = report
        .get("errors")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| MirrorError {
                    session_id: MEMORY_STEP_ID.to_string(),
                    error: item
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    (backup, errors)
}

/// 软删+复制可靠完成后，把清单原子改写为 done 并登记 copied（durable_write 不产出半写文件）。
fn finalize_mirror_manifest(
    backup_dir: &Path,
    copied: &[MirrorCopiedEntry],
    memory_backup: Option<&str>,
) -> Result<(), String> {
    let file = backup_dir.join(MIRROR_MANIFEST_FILE);
    let text =
        std::fs::read_to_string(&file).map_err(|error| format!("镜像清单读取失败：{error}"))?;
    let mut manifest: MirrorBackupManifest =
        serde_json::from_str(&text).map_err(|error| format!("镜像清单解析失败：{error}"))?;
    manifest.copied = copied.to_vec();
    manifest.memory_backup = memory_backup.map(str::to_string);
    manifest.state = MIRROR_STATE_DONE.to_string();
    let content = serde_json::to_string_pretty(&manifest).map_err(|error| error.to_string())?;
    session_backup::durable_write_str(&file, &content)
        .map_err(|error| format!("镜像清单状态更新失败：{error}"))?;
    Ok(())
}

fn load_mirror_manifest(backup_path: &Path) -> Result<MirrorBackupManifest, String> {
    let file = backup_path.join(MIRROR_MANIFEST_FILE);
    let text = std::fs::read_to_string(&file)
        .map_err(|error| format!("镜像备份清单不可读：{error}"))?;
    let manifest: MirrorBackupManifest =
        serde_json::from_str(&text).map_err(|error| format!("镜像备份清单已损坏：{error}"))?;
    if manifest.version != MIRROR_BACKUP_VERSION {
        return Err(format!("镜像备份清单版本不受支持（{}）", manifest.version));
    }
    Ok(manifest)
}

/// 精确白名单回滚：只按清单登记的 `(sessionId, userId, deletedAtWritten)` 三元组置空。
///
/// **严禁** `WHERE user_id=?` 批量置空（会误恢复用户此前自己删的会话）；**不碰正文文件**。
/// 返回的 `deleted_sessions` 表示本次还原行数，`skipped` 为未命中清单。
pub fn mirror_restore_at(
    paths: &SessionPaths,
    variant: WbVariant,
    backup_path: &Path,
) -> Result<MirrorReport, String> {
    let manifest = load_mirror_manifest(backup_path)?;
    if manifest.variant != variant {
        return Err("镜像备份所属应用与当前不一致，拒绝还原".to_string());
    }
    let mut conn = open_db(&paths.workbuddy_db(), false)
        .ok_or_else(|| "会话库不可写，无法还原镜像".to_string())?;
    if !table_exists(&conn, "sessions") {
        return Err("会话库缺少 sessions 表，无法还原镜像".to_string());
    }
    session_backup::ensure_full_synchronous(&conn)?;
    let tx = conn
        .transaction()
        .map_err(|error| format!("会话数据事务开启失败：{error}"))?;
    let mut restored = 0usize;
    let mut skipped = Vec::new();
    for row in &manifest.rows {
        let affected = tx
            .execute(
                "UPDATE sessions SET deleted_at = NULL \
                 WHERE id = ?1 AND user_id = ?2 AND deleted_at = ?3",
                params![row.session_id, row.user_id, row.deleted_at_written],
            )
            .map_err(|error| format!("还原会话失败（{}）：{error}", row.session_id))?;
        if affected == 1 {
            restored += 1;
        } else {
            skipped.push(MirrorSkipped {
                session_id: row.session_id.clone(),
                reason: "会话不存在、归属不符或 deleted_at 已被改动，未还原".to_string(),
            });
        }
    }
    // 回滚本次「复制新增」的会话：同样按软删撤销（可再次还原），绝不静默留下孤儿。
    // 已被改动/不存在（如用户继续对话后 deleted_at 已非 NULL）则列入未撤销清单。
    let copy_deleted_at = now_ms();
    let mut restored_copies = 0usize;
    for entry in &manifest.copied {
        let affected = tx
            .execute(
                "UPDATE sessions SET deleted_at = ?1 \
                 WHERE id = ?2 AND user_id = ?3 AND deleted_at IS NULL",
                params![copy_deleted_at, entry.new_session_id, entry.user_id],
            )
            .map_err(|error| format!("撤销复制会话失败（{}）：{error}", entry.new_session_id))?;
        if affected == 1 {
            restored_copies += 1;
        } else {
            skipped.push(MirrorSkipped {
                session_id: entry.new_session_id.clone(),
                reason: "复制新增未撤销：会话不存在或已被改动".to_string(),
            });
        }
    }
    tx.commit()
        .map_err(|error| format!("会话数据提交失败：{error}"))?;
    Ok(MirrorReport {
        deleted_sessions: restored,
        copied_sessions: 0,
        restored_copies,
        backup: Some(backup_path.to_string_lossy().to_string()),
        needs_recovery: false,
        warnings: Vec::new(),
        skipped,
        errors: Vec::new(),
    })
}

/// 限根还原：canonicalize 后校验备份路径落在 `transactions_root` 之下，越界直接拒绝。
///
/// 只在 `mirror_restore_at` 之前加一道路径围栏，**不改其软删/回滚算法**。
pub fn mirror_restore_within_root(
    paths: &SessionPaths,
    variant: WbVariant,
    backup_path: &Path,
) -> Result<MirrorReport, String> {
    let canonical_backup = std::fs::canonicalize(backup_path)
        .map_err(|error| format!("镜像备份路径不存在或不可访问：{error}"))?;
    // 校验落在「任一合法档位根」之下：两个档位的 transactions_root 都 canonicalize 后比对；
    // 某档位根不存在（尚无该档备份）则跳过该根。越界即拒绝，绝不触碰数据库。
    let in_range = [WbVariant::Cn, WbVariant::Ai].iter().any(|candidate| {
        let root = session_backup::transactions_root(paths, *candidate);
        std::fs::canonicalize(&root)
            .map(|canonical_root| canonical_backup.starts_with(&canonical_root))
            .unwrap_or(false)
    });
    if !in_range {
        return Err("备份路径不在允许的镜像备份目录内，已拒绝还原".to_string());
    }
    // manifest 内部会再校验 `manifest.variant == variant`，故落到此处的路径与档位一致。
    mirror_restore_at(paths, variant, &canonical_backup)
}

/// 一处镜像备份的可观测摘要（扫描用）：状态、绝对路径、目标账号与软删行数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorBackupEntry {
    pub operation_id: String,
    pub variant: WbVariant,
    /// `pending`（崩溃窗口/未确认完成）或 `done`（软删已 commit 且读回校验通过）。
    pub state: String,
    /// 备份目录绝对路径（一键还原用）。
    pub backup: String,
    pub created_at: i64,
    pub target_uid: String,
    /// 本次写入的 `deleted_at`（各行一致；空清单为 None）。
    pub deleted_at: Option<i64>,
    /// 清单登记的行数（本次软删量）。
    pub deleted_sessions: usize,
}

/// 扫描所有镜像备份清单：暴露 `state`，让 UI 与启动恢复能发现「库已改但无感知」的窄窗口。
/// 按创建时间倒序；损坏/非镜像清单跳过（不 panic、不误报）。
pub fn scan_mirror_backups(paths: &SessionPaths) -> Vec<MirrorBackupEntry> {
    let root = paths
        .backup_root()
        .join(session_backup::TRANSACTIONS_DIR_NAME);
    let Ok(variant_dirs) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for variant_dir in variant_dirs.flatten() {
        let Ok(op_dirs) = std::fs::read_dir(variant_dir.path()) else {
            continue;
        };
        for op_dir in op_dirs.flatten() {
            let dir = op_dir.path();
            let Ok(text) = std::fs::read_to_string(dir.join(MIRROR_MANIFEST_FILE)) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_str::<MirrorBackupManifest>(&text) else {
                continue;
            };
            entries.push(MirrorBackupEntry {
                operation_id: manifest.operation_id,
                variant: manifest.variant,
                state: manifest.state,
                backup: dir.to_string_lossy().to_string(),
                created_at: manifest.created_at,
                target_uid: manifest.target_uid,
                deleted_at: manifest.rows.first().map(|row| row.deleted_at_written),
                deleted_sessions: manifest.rows.len(),
            });
        }
    }
    entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    entries
}

// ---------------------------------------------------------------------------
// 红测（批次①：零 IO 副作用、契约字段、STALE 六条、隔离与判重）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::config::now_ms;
    use crate::modules::session::LinkNamespace;
    use serde_json::json;
    use std::path::{Path, PathBuf};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "wb-mirror-{tag}-{}-{}",
                std::process::id(),
                now_ms()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn paths_for(dir: &TempDir) -> SessionPaths {
        SessionPaths {
            store_root: dir.path().join("store"),
            data_root: dir.path().join("data"),
            auth_file: dir.path().join("auth.json"),
            link_namespace: LinkNamespace::WorkBuddy,
        }
    }

    fn group(id: &str, members: Vec<(&str, &str, MemberState)>) -> LinkGroup {
        LinkGroup {
            id: id.to_string(),
            variant: WbVariant::Cn,
            created_at: 0,
            members: members
                .into_iter()
                .map(|(uid, session_id, state)| session_link::LinkMember {
                    member_id: format!("{id}-{uid}-{session_id}"),
                    account_id: None,
                    uid: uid.to_string(),
                    session_id: session_id.to_string(),
                    variant: Some(WbVariant::Cn),
                    state,
                    linked_at: 0,
                    last_synced_at: None,
                })
                .collect(),
            pair_bases: Vec::new(),
        }
    }

    fn target(session_id: &str, group_id: &str, digest: &str, updated_at: i64) -> MirrorTarget {
        MirrorTarget {
            session_id: session_id.to_string(),
            title: format!("标题-{session_id}"),
            user_id: "u-target".to_string(),
            group_id: group_id.to_string(),
            updated_at,
            full_digest: digest.to_string(),
            kind: "session".to_string(),
        }
    }

    fn baseline_binding() -> (Vec<LinkGroup>, Vec<MirrorTarget>, MirrorBinding) {
        let groups = vec![group(
            "g-1",
            vec![
                ("u-mother", "s-mother", MemberState::Active),
                ("u-target", "s-1", MemberState::Active),
            ],
        )];
        let targets = vec![target("s-1", "g-1", "digest-1", 1000)];
        let binding = build_mirror_binding(
            "u-mother",
            "u-target",
            &groups,
            &targets,
            vec!["g-1".to_string()],
            vec!["g-1".to_string()],
            true,
        );
        (groups, targets, binding)
    }

    // --- 红测 1：STALE 六条逐条触发 + 一致时为空 ---------------------------

    #[test]
    fn verify_mirror_binding_consistent_binding_has_no_stale() {
        let (_, _, binding) = baseline_binding();
        assert!(
            verify_mirror_binding(&binding, &binding.clone()).is_empty(),
            "现场一致时不得报 STALE"
        );
    }

    #[test]
    fn verify_mirror_binding_reports_six_stale_rules() {
        let (_, _, expected) = baseline_binding();
        let cases: Vec<(&str, MirrorBinding, &str)> = vec![
            (
                "motherUid 变化",
                MirrorBinding {
                    mother_uid: "u-other".to_string(),
                    ..expected.clone()
                },
                "母本账号已变化",
            ),
            (
                "母本 active 成员集合变化",
                MirrorBinding {
                    mother_active_fingerprint: "changed".to_string(),
                    ..expected.clone()
                },
                "关联会话集合已变化",
            ),
            (
                "待删会话 fullDigest/updated_at 变化",
                MirrorBinding {
                    targets: vec![target("s-1", "g-1", "digest-changed", 1000)],
                    ..expected.clone()
                },
                "内容或更新时间已变化",
            ),
            (
                "待删会话软删状态变化",
                MirrorBinding {
                    all_targets_active: false,
                    ..expected.clone()
                },
                "已被软删或移除",
            ),
            (
                "group_id 查不到",
                MirrorBinding {
                    found_group_ids: Vec::new(),
                    ..expected.clone()
                },
                "关联组已不存在",
            ),
            (
                "母本 uid 不在该组成员中",
                MirrorBinding {
                    groups_with_mother: Vec::new(),
                    ..expected.clone()
                },
                "已不在该组成员中",
            ),
        ];
        assert_eq!(cases.len(), 6, "施工单要求恰好六条 STALE");
        for (name, live, keyword) in cases {
            let result = verify_mirror_binding(&expected, &live);
            assert_eq!(result.len(), 1, "{name} 应只触发一条：{result:?}");
            assert_eq!(result[0].reason_code, REASON_PREVIEW_STALE, "{name}");
            assert!(
                result[0].detail.contains(keyword),
                "{name} 文案缺失：{:?}",
                result[0].detail
            );
        }
    }

    // --- 红测 2：清单 user_id 隔离 + 母本 uid 不进清单 + 字段齐备 ----------

    #[test]
    fn mirror_targets_isolates_user_id_and_lists_every_field() {
        let groups = vec![group(
            "g-1",
            vec![("u-target", "s-1", MemberState::Active)],
        )];
        let rows = vec![
            MirrorRow {
                session_id: "s-1".to_string(),
                user_id: "u-target".to_string(),
                title: Some("原名".to_string()),
                custom_title: Some("改名".to_string()),
                updated_at: 1000,
                deleted_at: None,
            },
            MirrorRow {
                session_id: "s-mother".to_string(),
                user_id: "u-mother".to_string(),
                title: Some("母本会话".to_string()),
                custom_title: None,
                updated_at: 1000,
                deleted_at: None,
            },
            MirrorRow {
                session_id: "s-third".to_string(),
                user_id: "u-third".to_string(),
                title: Some("第三方".to_string()),
                custom_title: None,
                updated_at: 1000,
                deleted_at: None,
            },
            MirrorRow {
                session_id: "s-deleted".to_string(),
                user_id: "u-target".to_string(),
                title: Some("已软删".to_string()),
                custom_title: None,
                updated_at: 1000,
                deleted_at: Some(999),
            },
        ];
        let targets = mirror_targets_from_rows(&rows, "u-target", "u-mother", &groups);
        assert_eq!(targets.len(), 1, "只列目标 uid 且未软删的行");
        let only = &targets[0];
        assert_eq!(only.session_id, "s-1");
        assert_eq!(only.user_id, "u-target");
        assert_eq!(only.title, "改名", "custom_title 优先展示");
        assert_eq!(only.group_id, "g-1");
        assert_eq!(only.updated_at, 1000);
        assert_eq!(only.kind, "session");
        assert!(!only.full_digest.is_empty(), "fullDigest 必须齐备");
        assert!(
            targets.iter().all(|t| t.session_id != "s-mother"),
            "母本 uid 绝不进清单"
        );
    }

    #[test]
    fn private_sessions_get_empty_group_id() {
        let rows = vec![MirrorRow {
            session_id: "s-private".to_string(),
            user_id: "u-target".to_string(),
            title: None,
            custom_title: None,
            updated_at: 1,
            deleted_at: None,
        }];
        let targets = mirror_targets_from_rows(&rows, "u-target", "u-mother", &[]);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].group_id, "", "未建组会话 groupId 为空");
        assert_eq!(targets[0].title, "(无标题)");
    }

    // --- 红测 3：跨账号关联键判重（uid 精确匹配，绝不按标题） --------------

    #[test]
    fn count_mirror_copies_uses_link_member_uid_not_title() {
        let groups = vec![
            group(
                "g-both",
                vec![
                    ("u-mother", "same-title", MemberState::Active),
                    ("u-target", "same-title", MemberState::Active),
                ],
            ),
            group(
                "g-mother-only",
                vec![("u-mother", "same-title", MemberState::Active)],
            ),
            group(
                "g-other-uid",
                vec![("u-third", "same-title", MemberState::Active)],
            ),
        ];
        assert_eq!(
            count_mirror_copies(&groups, "u-mother", "u-target"),
            1,
            "仅母本有、目标没有的组需复制；标题相同不合并；第三方 uid 不算母本"
        );
    }

    #[test]
    fn group_id_for_matches_uid_and_session_id_exactly() {
        let groups = vec![group(
            "g-1",
            vec![("u-third", "s-1", MemberState::Active)],
        )];
        assert_eq!(
            group_id_for(&groups, "u-target", "s-1"),
            "",
            "别的 uid 的同名 session_id 不得误判归属"
        );
    }

    #[test]
    fn mother_active_fingerprint_changes_when_members_change() {
        let base = vec![group(
            "g-1",
            vec![("u-mother", "s-1", MemberState::Active)],
        )];
        let changed = vec![group(
            "g-1",
            vec![
                ("u-mother", "s-1", MemberState::Active),
                ("u-mother", "s-2", MemberState::Active),
            ],
        )];
        assert_ne!(
            mother_active_fingerprint(&base, "u-mother"),
            mother_active_fingerprint(&changed, "u-mother")
        );
    }

    // --- 红测 4：expectedDigest 顺序无关、内容变化即变 ---------------------

    #[test]
    fn expected_digest_is_order_independent_and_content_sensitive() {
        let a = vec![
            target("s-1", "g-1", "d1", 10),
            target("s-2", "g-1", "d2", 20),
        ];
        let b = vec![
            target("s-2", "g-1", "d2", 20),
            target("s-1", "g-1", "d1", 10),
        ];
        assert_eq!(expected_digest_of(&a), expected_digest_of(&b));
        let c = vec![
            target("s-1", "g-1", "d1-changed", 10),
            target("s-2", "g-1", "d2", 20),
        ];
        assert_ne!(expected_digest_of(&a), expected_digest_of(&c));
    }

    // --- 红测 5：previewToken 绑定 motherUid（复用 save/load_preview_token）+ 旧 token 兼容 ---

    #[test]
    fn mirror_token_binds_mother_uid_and_verify_preview_detects_change() {
        let dir = TempDir::new("token");
        let paths = paths_for(&dir);
        let groups = vec![group(
            "g-1",
            vec![("u-mother", "s-m", MemberState::Active)],
        )];
        let binding = build_mirror_token_binding(
            WbVariant::Cn,
            "u-mother",
            "u-target",
            "digest-x",
            &groups,
            1,
            1,
        );
        assert_eq!(binding.mother_uid.as_deref(), Some("u-mother"));
        let token_id = session_link::save_preview_token(&paths, binding.clone()).unwrap();
        let token = session_link::load_preview_token(&paths, &token_id).unwrap();
        assert_eq!(token.binding.mother_uid.as_deref(), Some("u-mother"));
        assert_eq!(token.binding.group_fingerprint, "digest-x");

        let mut drifted = token.binding.clone();
        drifted.mother_uid = Some("u-other".to_string());
        let reasons = session_link::verify_preview(&token, &drifted);
        assert!(
            reasons.iter().any(|r| r.contains("母本账号已变化")),
            "verify_preview 必须纳入 motherUid 比较：{reasons:?}"
        );
        assert!(
            session_link::verify_preview(&token, &token.binding.clone()).is_empty(),
            "现场一致零 STALE"
        );
    }

    #[test]
    fn legacy_token_without_mother_uid_deserializes_to_none() {
        // 旧逐组同步凭据没有 mother_uid 字段：serde(default) 必须得 None，零回归。
        let legacy = json!({
            "version": session_link::PREVIEW_TOKEN_VERSION,
            "previewId": "00000000-0000-0000-0000-000000000000",
            "createdAt": 1,
            "binding": {
                "variant": "cn",
                "groupId": "g-legacy",
                "groupFingerprint": "fp",
                "source": { "memberId": "m", "uid": "u-a", "sessionId": "s", "rawDigest": "", "normalizedDigest": "", "recordCount": 0 },
                "target": { "memberId": "n", "uid": "u-b", "sessionId": "t", "rawDigest": "", "normalizedDigest": "", "recordCount": 0 },
                "verdict": "identical"
            }
        });
        let token: session_link::PreviewToken = serde_json::from_value(legacy).unwrap();
        assert_eq!(token.binding.mother_uid, None);
    }

    // --- 红测 6：只读预览、零会话数据写入、幂等 ----------------------------

    #[test]
    fn mirror_preview_at_is_read_only_idempotent_and_binds_mother() {
        let dir = TempDir::new("readonly");
        let paths = paths_for(&dir);
        std::fs::create_dir_all(&paths.data_root).unwrap();
        std::fs::write(
            &paths.auth_file,
            r#"{"account":{"uid":"u-mother","label":"母本账号"}}"#,
        )
        .unwrap();
        {
            let conn = Connection::open(paths.workbuddy_db()).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (id TEXT, user_id TEXT, title TEXT, custom_title TEXT, updated_at INTEGER, deleted_at INTEGER);\
                 INSERT INTO sessions VALUES ('s-1','u-target','会话一',NULL,1000,NULL);\
                 INSERT INTO sessions VALUES ('s-2','u-target','会话二','别名',2000,NULL);\
                 INSERT INTO sessions VALUES ('s-3','u-mother','母本会话',NULL,1000,NULL);\
                 INSERT INTO sessions VALUES ('s-4','u-target','已软删',NULL,1000,999);",
            )
            .unwrap();
        }
        let target_acc = json!({ "uid": "u-target" });
        // 前端伪造 motherUid 也不能改变母本：母本恒取登录态 u-mother。
        let forged_acc = json!({ "uid": "u-target", "motherUid": "u-attacker" });

        let snapshot = |conn: &Connection| -> Vec<(String, Option<i64>)> {
            let mut stmt = conn
                .prepare("SELECT id, deleted_at FROM sessions ORDER BY id")
                .unwrap();
            stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
            })
            .unwrap()
            .flatten()
            .collect()
        };
        let before = {
            let conn = Connection::open(paths.workbuddy_db()).unwrap();
            snapshot(&conn)
        };

        let first = mirror_preview_at(&paths, WbVariant::Cn, &target_acc).unwrap();
        let second = mirror_preview_at(&paths, WbVariant::Cn, &forged_acc).unwrap();

        assert_eq!(first.mother_uid, "u-mother", "母本恒取登录态");
        assert_eq!(second.mother_uid, "u-mother", "前端伪造 motherUid 无效");
        assert_eq!(first.mother_label, "母本账号");
        assert_eq!(first.session_count, 2, "仅目标 uid 且未软删的两条");
        assert_eq!(first.targets.len(), 2, "清单逐条不折叠");
        assert!(first.backup_dir.is_none(), "预览阶段无备份目录");
        assert!(!first.preview_token.is_empty(), "必须签发预览凭据");
        assert_eq!(
            first.expected_digest, second.expected_digest,
            "同一现场两次预览摘要一致（幂等）"
        );
        assert_eq!(first.targets, second.targets, "同一现场清单一致（幂等）");
        assert_eq!(first.session_count, second.session_count);

        let after = {
            let conn = Connection::open(paths.workbuddy_db()).unwrap();
            snapshot(&conn)
        };
        assert_eq!(before, after, "预览零会话数据写入：id 与 deleted_at 逐行不变");
        assert!(
            !paths.data_root.join("MEMORY.md").exists(),
            "预览绝不写 MEMORY.md"
        );

        let token = session_link::load_preview_token(&paths, &second.preview_token).unwrap();
        assert_eq!(token.binding.mother_uid.as_deref(), Some("u-mother"));
    }

    #[test]
    fn mirror_preview_at_rejects_same_mother_and_target() {
        let dir = TempDir::new("same");
        let paths = paths_for(&dir);
        std::fs::write(&paths.auth_file, r#"{"account":{"uid":"u-same"}}"#).unwrap();
        let error = mirror_preview_at(&paths, WbVariant::Cn, &json!({ "uid": "u-same" }))
            .unwrap_err();
        assert!(error.contains("相同"), "{error}");
    }

    // --- 批次②：软删执行 + 精确白名单回滚 ---------------------------------

    fn seed_auth(paths: &SessionPaths, uid: &str) {
        std::fs::write(
            &paths.auth_file,
            format!(r#"{{"account":{{"uid":"{uid}","label":"母本账号"}}}}"#),
        )
        .unwrap();
    }

    /// 建 `sessions` 表并按 `(id, user_id, updated_at, deleted_at)` 播种；只读列齐备。
    fn seed_session_db(paths: &SessionPaths, rows: &[(&str, &str, i64, Option<i64>)]) {
        std::fs::create_dir_all(&paths.data_root).unwrap();
        let conn = Connection::open(paths.workbuddy_db()).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (\
             id TEXT, user_id TEXT, title TEXT, custom_title TEXT, \
             updated_at INTEGER, deleted_at INTEGER);",
        )
        .unwrap();
        for (id, uid, updated, deleted) in rows {
            conn.execute(
                "INSERT INTO sessions (id, user_id, title, custom_title, updated_at, deleted_at) \
                 VALUES (?1, ?2, ?3, NULL, ?4, ?5)",
                rusqlite::params![id, uid, format!("标题-{id}"), updated, deleted],
            )
            .unwrap();
        }
    }

    fn deleted_at_of(paths: &SessionPaths, session_id: &str) -> Option<i64> {
        let conn = Connection::open(paths.workbuddy_db()).unwrap();
        conn.query_row(
            "SELECT deleted_at FROM sessions WHERE id = ?1",
            [session_id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .unwrap()
    }

    /// 走真实只读预览产出计划与凭据，保证执行侧拿到的是「真凭据」而非手搓。
    fn preview_plan(paths: &SessionPaths, target_uid: &str) -> MirrorPlan {
        let preview = mirror_preview_at(paths, WbVariant::Cn, &json!({ "uid": target_uid })).unwrap();
        MirrorPlan {
            mother_uid: preview.mother_uid.clone(),
            targets: preview.targets,
            expected_digest: preview.expected_digest,
            preview_token: preview.preview_token,
        }
    }

    #[test]
    fn mirror_apply_soft_deletes_only_target_active_rows() {
        let dir = TempDir::new("apply-scope");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(
            &paths,
            &[
                ("s-1", "u-target", 1000, None),
                ("s-2", "u-target", 2000, None),
                ("s-other", "u-other", 1000, None),
                ("s-mother", "u-mother", 1000, None),
                ("s-already", "u-target", 1000, Some(999)),
            ],
        );
        let plan = preview_plan(&paths, "u-target");
        assert_eq!(plan.targets.len(), 2, "只列 u-target 未软删两行");

        let report = mirror_apply_at(&paths, WbVariant::Cn, "u-target", &json!({"uid": "u-target"}), &paths.data_root, &plan).unwrap();
        assert_eq!(report.deleted_sessions, 2);
        assert!(report.backup.is_some(), "软删必须留备份");
        assert!(deleted_at_of(&paths, "s-1").is_some());
        assert!(deleted_at_of(&paths, "s-2").is_some());
        assert_eq!(deleted_at_of(&paths, "s-other"), None, "跨账号零影响");
        assert_eq!(deleted_at_of(&paths, "s-mother"), None, "母本会话零影响");
        assert_eq!(deleted_at_of(&paths, "s-already"), Some(999), "已软删行不被改写");
    }

    #[test]
    fn mirror_apply_is_idempotent_after_repreview() {
        let dir = TempDir::new("apply-idem");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(&paths, &[("s-1", "u-target", 1000, None)]);
        let plan = preview_plan(&paths, "u-target");
        let first = mirror_apply_at(&paths, WbVariant::Cn, "u-target", &json!({"uid": "u-target"}), &paths.data_root, &plan).unwrap();
        assert_eq!(first.deleted_sessions, 1);
        let written = deleted_at_of(&paths, "s-1").unwrap();

        // 同一份计划重放必须 fail-closed（集合已变），且不重复写入、不改写已置位行。
        assert!(
            mirror_apply_at(&paths, WbVariant::Cn, "u-target", &json!({"uid": "u-target"}), &paths.data_root, &plan).is_err(),
            "同一计划重放应被拒绝而非再次软删"
        );
        assert_eq!(deleted_at_of(&paths, "s-1"), Some(written), "重放不动已置位行");

        // 重新预览：目标已空 → 第二次 deleted=0，且不建空备份。
        let empty_plan = preview_plan(&paths, "u-target");
        assert!(empty_plan.targets.is_empty());
        let second = mirror_apply_at(&paths, WbVariant::Cn, "u-target", &json!({"uid": "u-target"}), &paths.data_root, &empty_plan).unwrap();
        assert_eq!(second.deleted_sessions, 0);
        assert!(second.backup.is_none(), "空清单不建备份目录");
    }

    #[test]
    fn mirror_apply_rejects_digest_mismatch_with_zero_write() {
        let dir = TempDir::new("apply-digest");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(&paths, &[("s-1", "u-target", 1000, None)]);
        let mut plan = preview_plan(&paths, "u-target");
        plan.expected_digest = "tampered-digest".to_string();

        let error = mirror_apply_at(&paths, WbVariant::Cn, "u-target", &json!({"uid": "u-target"}), &paths.data_root, &plan).unwrap_err();
        assert!(error.contains("不一致"), "{error}");
        assert_eq!(deleted_at_of(&paths, "s-1"), None, "摘要不符零写入");
    }

    #[test]
    fn mirror_apply_rejects_when_mother_changed_with_zero_write() {
        let dir = TempDir::new("apply-mother");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(&paths, &[("s-1", "u-target", 1000, None)]);
        let plan = preview_plan(&paths, "u-target");
        // 预览后登录态被切到别的母本：现场母本与凭据不符，双 verify 必报 STALE。
        seed_auth(&paths, "u-attacker");

        let error = mirror_apply_at(&paths, WbVariant::Cn, "u-target", &json!({"uid": "u-target"}), &paths.data_root, &plan).unwrap_err();
        assert!(error.contains("过期"), "{error}");
        assert_eq!(deleted_at_of(&paths, "s-1"), None, "母本变化零写入");
    }

    #[test]
    fn mirror_restore_only_reverts_this_write_not_prior_deletes() {
        let dir = TempDir::new("restore-scope");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(
            &paths,
            &[
                ("s-1", "u-target", 1000, None),
                // 用户此前自己删的会话：deleted_at=999，绝不因本次镜像回滚被误恢复。
                ("s-prior", "u-target", 1000, Some(999)),
            ],
        );
        let plan = preview_plan(&paths, "u-target");
        assert_eq!(plan.targets.len(), 1, "只列未软删的 s-1");

        let report = mirror_apply_at(&paths, WbVariant::Cn, "u-target", &json!({"uid": "u-target"}), &paths.data_root, &plan).unwrap();
        assert_eq!(report.deleted_sessions, 1);
        let backup = PathBuf::from(report.backup.clone().unwrap());
        assert!(deleted_at_of(&paths, "s-1").is_some());

        let restored = mirror_restore_at(&paths, WbVariant::Cn, &backup).unwrap();
        assert_eq!(restored.deleted_sessions, 1, "只还原本次写入一行");
        assert_eq!(deleted_at_of(&paths, "s-1"), None, "本次软删被还原");
        assert_eq!(
            deleted_at_of(&paths, "s-prior"),
            Some(999),
            "禁止批量置空：用户此前自删必须保留"
        );
    }

    #[test]
    fn mirror_apply_does_not_touch_session_body_files() {
        let dir = TempDir::new("apply-body");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(&paths, &[("s-1", "u-target", 1000, None)]);
        let project = paths.projects_dir().join("proj-a");
        std::fs::create_dir_all(&project).unwrap();
        let body = project.join("sessions.jsonl");
        std::fs::write(&body, b"{\"id\":\"s-1\",\"content\":\"keep me\"}\n").unwrap();
        let before = std::fs::read(&body).unwrap();

        let plan = preview_plan(&paths, "u-target");
        mirror_apply_at(&paths, WbVariant::Cn, "u-target", &json!({"uid": "u-target"}), &paths.data_root, &plan).unwrap();

        let after = std::fs::read(&body).unwrap();
        assert_eq!(before, after, "镜像只软删数据库行，绝不改会话正文");
    }

    #[test]
    fn mirror_restore_within_root_accepts_in_root_backup() {
        let dir = TempDir::new("restore-inroot");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(&paths, &[("s-1", "u-target", 1000, None)]);
        let plan = preview_plan(&paths, "u-target");
        let report = mirror_apply_at(&paths, WbVariant::Cn, "u-target", &json!({"uid": "u-target"}), &paths.data_root, &plan).unwrap();
        let backup = PathBuf::from(report.backup.clone().unwrap());

        let restored = mirror_restore_within_root(&paths, WbVariant::Cn, &backup).unwrap();
        assert_eq!(restored.deleted_sessions, 1, "根内路径可还原");
        assert_eq!(deleted_at_of(&paths, "s-1"), None);
    }

    #[test]
    fn mirror_restore_rejects_path_outside_root() {
        let dir = TempDir::new("restore-outside");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(&paths, &[("s-1", "u-target", 1000, None)]);
        let plan = preview_plan(&paths, "u-target");
        mirror_apply_at(&paths, WbVariant::Cn, "u-target", &json!({"uid": "u-target"}), &paths.data_root, &plan).unwrap(); // 确保根目录存在

        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let error = mirror_restore_within_root(&paths, WbVariant::Cn, &outside).unwrap_err();
        assert!(error.contains("不在允许"), "{error}");
    }

    #[test]
    fn mirror_scan_lists_backups_with_done_state() {
        let dir = TempDir::new("scan");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(&paths, &[("s-1", "u-target", 1000, None)]);
        let plan = preview_plan(&paths, "u-target");
        let report = mirror_apply_at(&paths, WbVariant::Cn, "u-target", &json!({"uid": "u-target"}), &paths.data_root, &plan).unwrap();

        let entries = scan_mirror_backups(&paths);
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.state, "done", "软删成功后清单应为 done");
        assert_eq!(entry.deleted_sessions, 1);
        assert_eq!(entry.target_uid, "u-target");
        assert_eq!(Some(entry.backup.clone()), report.backup);
        assert_eq!(
            entry.deleted_at,
            deleted_at_of(&paths, "s-1"),
            "扫描暴露本次写入的 deletedAt"
        );
    }

    #[test]
    fn mirror_manifest_state_pending_then_done() {
        // 缺省（无 state 字段）的清单按 pending 处理（保守：提示可还原，不当作已完成）。
        let legacy = json!({
            "version": 1, "operationId": "op", "variant": "cn", "motherUid": "m",
            "targetUid": "t", "previewToken": "p", "expectedDigest": "d",
            "createdAt": 1, "rows": [], "restoreSteps": []
        });
        let manifest: MirrorBackupManifest = serde_json::from_value(legacy).unwrap();
        assert_eq!(manifest.state, "pending", "缺省清单按 pending");

        // apply 成功后落盘清单必须为 done。
        let dir = TempDir::new("state");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(&paths, &[("s-1", "u-target", 1000, None)]);
        let plan = preview_plan(&paths, "u-target");
        let report = mirror_apply_at(&paths, WbVariant::Cn, "u-target", &json!({"uid": "u-target"}), &paths.data_root, &plan).unwrap();
        let file = PathBuf::from(report.backup.unwrap()).join(MIRROR_MANIFEST_FILE);
        let on_disk: MirrorBackupManifest =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(on_disk.state, "done", "软删成功后清单应改写为 done");
    }

    #[test]
    fn scan_ignores_non_mirror_transactions() {
        let dir = TempDir::new("scan-nonmirror");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(&paths, &[("s-1", "u-target", 1000, None)]);
        let plan = preview_plan(&paths, "u-target");
        mirror_apply_at(&paths, WbVariant::Cn, "u-target", &json!({"uid": "u-target"}), &paths.data_root, &plan).unwrap();

        // 另造一个非镜像交易目录（只有不属于 mirror 的清单），扫描必须忽略。
        let other = session_backup::transactions_root(&paths, WbVariant::Cn).join("other-op");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("manifest.json"), "{}").unwrap();

        let entries = scan_mirror_backups(&paths);
        assert_eq!(entries.len(), 1, "只返回 mirror manifest，不含其它交易目录");
    }

    // --- 批次③a：复制母本独有会话 + 复制可回滚 -----------------------------

    fn write_link_store(paths: &SessionPaths, groups: Vec<LinkGroup>) {
        let store = session_link::LinkStore {
            version: session_link::LINK_STORE_VERSION,
            revision: 1,
            groups,
        };
        let file = paths.session_links_file();
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(file, serde_json::to_string(&store).unwrap()).unwrap();
    }

    /// 可复制环境：源账号 u-mother 有带正文的 s-mother，目标 u-target 空；关联组 g-1 只有母本。
    fn copy_ready_env(dir: &TempDir) -> SessionPaths {
        let paths = paths_for(dir);
        seed_auth(&paths, "u-mother");
        std::fs::create_dir_all(&paths.data_root).unwrap();
        let conn = Connection::open(paths.workbuddy_db()).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT, user_id TEXT, title TEXT, custom_title TEXT, \
             updated_at INTEGER, deleted_at INTEGER);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions VALUES ('s-mother','u-mother','母本会话',NULL,2000,NULL)",
            [],
        )
        .unwrap();
        drop(conn);
        let econn = Connection::open(paths.edge_sync_db(WbVariant::Cn)).unwrap();
        econn
            .execute_batch(
                "CREATE TABLE edge_sync_mapping (session_id TEXT, conversation_id TEXT, \
                 msg_channel TEXT, created_at INTEGER);",
            )
            .unwrap();
        drop(econn);
        let proj = paths.projects_dir().join("ws-a");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("s-mother.jsonl"),
            format!(
                "{}\n{}\n",
                json!({"type":"user","sessionId":"s-mother","text":"你好"}),
                json!({"type":"assistant","sessionId":"s-mother","text":"hi"})
            ),
        )
        .unwrap();
        write_link_store(
            &paths,
            vec![group("g-1", vec![("u-mother", "s-mother", MemberState::Active)])],
        );
        paths
    }

    fn target_active_count(paths: &SessionPaths, uid: &str) -> i64 {
        Connection::open(paths.workbuddy_db())
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE user_id=?1 AND deleted_at IS NULL",
                [uid],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn missing_mirror_session_ids_excludes_linked() {
        let groups = vec![
            group("g-mother-only", vec![("u-mother", "s-m", MemberState::Active)]),
            group(
                "g-both",
                vec![
                    ("u-mother", "s-m2", MemberState::Active),
                    ("u-target", "s-t2", MemberState::Active),
                ],
            ),
        ];
        assert_eq!(
            missing_mirror_session_ids(&groups, "u-mother", "u-target"),
            vec!["s-m".to_string()],
            "只算母本独有组；已关联（g-both）不算缺失"
        );
    }

    #[test]
    fn mirror_copy_adds_mother_only_sessions() {
        let dir = TempDir::new("copy-add");
        let paths = copy_ready_env(&dir);
        let target_acc = json!({"id":"acc-u-target","uid":"u-target","variant":"cn"});
        let plan = preview_plan(&paths, "u-target");
        assert_eq!(plan.targets.len(), 0, "目标无待删行");

        let report = mirror_apply_at(&paths, WbVariant::Cn, "u-target", &target_acc, &paths.data_root, &plan).unwrap();
        assert_eq!(report.copied_sessions, 1, "应复制 1 个母本独有会话");
        assert_eq!(report.deleted_sessions, 0);
        assert_eq!(target_active_count(&paths, "u-target"), 1, "目标库出现副本");

        let backup = PathBuf::from(report.backup.clone().unwrap());
        let manifest: MirrorBackupManifest =
            serde_json::from_str(&std::fs::read_to_string(backup.join(MIRROR_MANIFEST_FILE)).unwrap())
                .unwrap();
        assert_eq!(manifest.copied.len(), 1, "复制登记进清单");
        assert_eq!(manifest.state, "done");
    }

    #[test]
    fn mirror_copy_is_idempotent_after_repreview() {
        let dir = TempDir::new("copy-idem");
        let paths = copy_ready_env(&dir);
        let target_acc = json!({"id":"acc-u-target","uid":"u-target","variant":"cn"});

        let plan1 = preview_plan(&paths, "u-target");
        let r1 = mirror_apply_at(&paths, WbVariant::Cn, "u-target", &target_acc, &paths.data_root, &plan1).unwrap();
        assert_eq!(r1.copied_sessions, 1);

        // 重预览：副本在含母本的组里，不算多余、不再复制、不被误软删。
        let plan2 = preview_plan(&paths, "u-target");
        assert_eq!(plan2.targets.len(), 0, "母本对端副本不算待删");
        let r2 = mirror_apply_at(&paths, WbVariant::Cn, "u-target", &target_acc, &paths.data_root, &plan2).unwrap();
        assert_eq!(r2.copied_sessions, 0, "已关联不重复复制");
        assert_eq!(r2.deleted_sessions, 0, "副本不被误软删");
        assert_eq!(target_active_count(&paths, "u-target"), 1, "副本仍在");
    }

    #[test]
    fn mirror_copy_failure_does_not_block_soft_delete() {
        let dir = TempDir::new("copy-fail");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        // 目标有一个多余行（待软删）；母本组里的 s-mother 无正文 → 复制必失败。
        seed_session_db(&paths, &[("s-extra", "u-target", 1000, None)]);
        {
            let conn = Connection::open(paths.workbuddy_db()).unwrap();
            conn.execute(
                "INSERT INTO sessions VALUES ('s-mother','u-mother','母本',NULL,2000,NULL)",
                [],
            )
            .unwrap();
        }
        write_link_store(
            &paths,
            vec![group("g-1", vec![("u-mother", "s-mother", MemberState::Active)])],
        );
        let target_acc = json!({"id":"acc-u-target","uid":"u-target","variant":"cn"});
        let plan = preview_plan(&paths, "u-target");
        assert_eq!(plan.targets.len(), 1, "s-extra 待软删");

        let report = mirror_apply_at(&paths, WbVariant::Cn, "u-target", &target_acc, &paths.data_root, &plan).unwrap();
        assert_eq!(report.deleted_sessions, 1, "软删成功");
        assert_eq!(report.copied_sessions, 0, "复制失败");
        assert!(!report.errors.is_empty(), "复制错误进 errors");
        assert!(deleted_at_of(&paths, "s-extra").is_some(), "软删未被复制失败阻断");
    }

    #[test]
    fn mirror_restore_soft_deletes_copied_sessions() {
        let dir = TempDir::new("copy-rollback");
        let paths = copy_ready_env(&dir);
        let target_acc = json!({"id":"acc-u-target","uid":"u-target","variant":"cn"});
        let plan = preview_plan(&paths, "u-target");
        let report = mirror_apply_at(&paths, WbVariant::Cn, "u-target", &target_acc, &paths.data_root, &plan).unwrap();
        assert_eq!(report.copied_sessions, 1);
        assert_eq!(target_active_count(&paths, "u-target"), 1, "副本存在");

        let backup = PathBuf::from(report.backup.clone().unwrap());
        let restored = mirror_restore_within_root(&paths, WbVariant::Cn, &backup).unwrap();
        assert_eq!(restored.restored_copies, 1, "复制新增被撤销");
        assert_eq!(target_active_count(&paths, "u-target"), 0, "副本已软删");
    }

    #[test]
    fn mirror_memory_failure_does_not_block_soft_delete() {
        let dir = TempDir::new("memory-fail");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(&paths, &[("s-1", "u-target", 1000, None)]);

        // 母本有记忆；目标的人格目录位置被一个同名**文件**占住 → 创建目录必失败。
        let memory_root = dir.path().join("memory-root");
        let mother_dir = memory::personal_dir_at(&memory_root, "u-mother");
        std::fs::create_dir_all(&mother_dir).unwrap();
        std::fs::write(mother_dir.join("MEMORY.md"), "## 母本记忆\n").unwrap();
        std::fs::write(memory::personal_dir_at(&memory_root, "u-target"), "占位文件").unwrap();

        let plan = preview_plan(&paths, "u-target");
        let report = mirror_apply_at(
            &paths,
            WbVariant::Cn,
            "u-target",
            &json!({"uid": "u-target"}),
            &memory_root,
            &plan,
        )
        .unwrap();

        assert_eq!(report.deleted_sessions, 1, "记忆覆盖失败不得回滚已完成的软删");
        assert!(
            report
                .errors
                .iter()
                .any(|error| error.session_id == MEMORY_STEP_ID),
            "记忆失败必须进 errors：{:?}",
            report.errors
        );
        // 记忆步骤失败不该伪造备份路径。
        let manifest_file = PathBuf::from(report.backup.clone().unwrap()).join(MIRROR_MANIFEST_FILE);
        let manifest: MirrorBackupManifest =
            serde_json::from_str(&std::fs::read_to_string(&manifest_file).unwrap()).unwrap();
        assert_eq!(manifest.memory_backup, None, "未写成功就不得登记记忆备份");
        assert_eq!(manifest.state, "done", "会话步骤已完成，清单仍应 done");
    }

    #[test]
    fn mirror_memory_overwrite_runs_even_when_sessions_match() {
        let dir = TempDir::new("memory-only");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        // 目标没有任何会话差异 → 会话步骤零操作，但 MEMORY.md 不同，仍必须覆盖。
        seed_session_db(&paths, &[]);
        let memory_root = dir.path().join("memory-root");
        let mother_dir = memory::personal_dir_at(&memory_root, "u-mother");
        std::fs::create_dir_all(&mother_dir).unwrap();
        std::fs::write(mother_dir.join("MEMORY.md"), "## 母本记忆\n").unwrap();
        let target_dir = memory::personal_dir_at(&memory_root, "u-target");
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::write(target_dir.join("MEMORY.md"), "## 目标旧记忆\n").unwrap();

        let plan = preview_plan(&paths, "u-target");
        let report = mirror_apply_at(
            &paths,
            WbVariant::Cn,
            "u-target",
            &json!({"uid": "u-target"}),
            &memory_root,
            &plan,
        )
        .unwrap();

        assert_eq!(report.deleted_sessions, 0);
        assert!(
            std::fs::read_to_string(target_dir.join("MEMORY.md"))
                .unwrap()
                .contains("母本记忆"),
            "仅记忆不同也必须整份覆盖"
        );
        let manifest_file = PathBuf::from(report.backup.clone().unwrap()).join(MIRROR_MANIFEST_FILE);
        let manifest: MirrorBackupManifest =
            serde_json::from_str(&std::fs::read_to_string(&manifest_file).unwrap()).unwrap();
        assert!(
            manifest.memory_backup.is_some(),
            "记忆备份路径必须登记进同一清单"
        );
    }
}
