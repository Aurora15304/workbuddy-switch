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

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::modules::auth_file;
use crate::modules::session::{
    current_user_uid_at, open_db, table_exists, SessionPaths, REASON_PREVIEW_STALE,
};
use crate::modules::session_link::{
    self, full_digest_of, LinkGroup, MemberState, PreviewBinding, PreviewMemberBinding, SyncVerdict,
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

/// 从只读行生成逐条清单：`user_id` 隔离（母本 uid 绝不进清单）+ 仅未软删行 + 字段齐备。
pub fn mirror_targets_from_rows(
    rows: &[MirrorRow],
    target_uid: &str,
    groups: &[LinkGroup],
) -> Vec<MirrorTarget> {
    let mut targets: Vec<MirrorTarget> = rows
        .iter()
        .filter(|row| row.user_id == target_uid && row.deleted_at.is_none())
        .map(|row| MirrorTarget {
            session_id: row.session_id.clone(),
            title: display_title(row.title.clone(), row.custom_title.clone()),
            user_id: row.user_id.clone(),
            group_id: group_id_for(groups, target_uid, &row.session_id),
            updated_at: row.updated_at,
            full_digest: session_row_digest(row),
            kind: "session".to_string(),
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
    let targets = mirror_targets_from_rows(&rows, &target_uid, &groups);
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
        let targets = mirror_targets_from_rows(&rows, "u-target", &groups);
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
        let targets = mirror_targets_from_rows(&rows, "u-target", &[]);
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
}
