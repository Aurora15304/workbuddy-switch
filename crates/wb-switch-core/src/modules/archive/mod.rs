//! 整包导出 / 导入：把「切换器能管的所有东西」打成一个 zip，便于迁移与备份。
//!
//! 分工（按文件拆分，避免一个文件越滚越大）：
//! - [`group`]：哪些数据算一类（分组定义 + 收集 + 恢复 + 白名单）；
//! - [`manifest`]：归档清单（版本、导出时间、分组统计）；
//! - [`zip`]：纯压缩/解压 + zip slip 防护 + 体积条目限额。
//!
//! 本文件只做编排：选择分组 → 收集 → 写 zip → inspect / 备份 → 逐组恢复。
//!
//! 安全底线：
//! 1. 解压出的条目名先过 [`zip::validate_entry_name`]，再只作为「组内相对路径」
//!    到白名单里查目标，绝不直接拼路径；
//! 2. 导入前把将被覆盖的文件全量备份；备份失败立即中止；逐组恢复中途失败时，
//!    按备份把已写文件拷回、把新建文件删除，绝不留半覆盖现场；
//! 3. 账号不整文件覆盖，统一走 `account::with_accounts_write` 按 uid 合并。

mod group;
mod manifest;
mod zip;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::modules::session_link::{try_lock_file, LockError};
use crate::modules::{account, config, update};

pub use group::ArchiveGroup;

use group::{DataGroupSpec, ImportContext};
use manifest::{Manifest, ManifestGroup, FORMAT_VERSION, MANIFEST_NAME};
use zip::ZipPayload;

/// 国内版 / 国际版的数据根目录名，与 `variant::data_root()` 的取值一一对应。
pub(crate) const DATA_ROOT_CN: &str = ".workbuddy";
pub(crate) const DATA_ROOT_AI: &str = ".workbuddy-ai";

/// 两个数据根在 zip 条目名里的命名空间（不带前导点，避免与相对路径混淆）。
pub(crate) const NAMESPACE_CN: &str = "workbuddy";
pub(crate) const NAMESPACE_AI: &str = "workbuddy-ai";

/// 导入互斥：同一进程内一次只允许一个导入。
static IMPORT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 导出环境的两个根目录。
///
/// 不直接在实现里调 `config::store_dir()`：测试需要把数据放进临时目录。用注入的
/// 根目录代替「改 `WB_SWITCH_HOME` 环境变量」——环境变量是进程级全局状态，与
/// 其它并行测试互相干扰，还得额外加串行锁。
#[derive(Debug, Clone)]
pub(crate) struct ArchiveEnv {
    pub(crate) store: PathBuf,
    pub(crate) home: PathBuf,
}

impl ArchiveEnv {
    pub(crate) fn current() -> Self {
        Self {
            store: config::store_dir(),
            home: config::home_dir(),
        }
    }

    /// 临时沙箱：`{root}/.wb-switch` + `{root}/.workbuddy`。
    #[cfg(test)]
    fn at(root: &Path) -> Self {
        Self {
            store: root.join(".wb-switch"),
            home: root.to_path_buf(),
        }
    }

    fn accounts_file(&self) -> PathBuf {
        self.store.join("accounts.json")
    }

    /// 两个档位的数据根（国内版 / 国际版）。
    ///
    /// 一律返回两个：用户可能只装了其中一个档位，对应的目录不存在时由 `collect`
    /// 自行跳过——漏掉国际版会让只玩国际版的用户导出空组、换机后记忆静默丢失。
    pub(crate) fn memory_roots(&self) -> [PathBuf; 2] {
        [
            self.home.join(DATA_ROOT_CN),
            self.home.join(DATA_ROOT_AI),
        ]
    }
}

/// 导出结果：宿主直接返回给前端（camelCase）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportOutcome {
    pub path: String,
    pub bytes: u64,
    pub groups: Vec<String>,
}

/// 归档概览：只读清单，不解压。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveInspect {
    pub format_version: u32,
    pub app_version: String,
    pub exported_at: i64,
    /// `files` / `bytes` 取自清单记录（导出那一刻的值），不是当前磁盘。
    pub groups: Vec<ArchiveGroup>,
}

/// 导入结果。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportOutcome {
    pub groups: Vec<String>,
    pub backup_dir: String,
    pub accounts_added: u64,
    pub accounts_updated: u64,
}

/// 当前磁盘上各分组的文件数与体积（前端勾选面板用）。
pub fn groups() -> Vec<ArchiveGroup> {
    groups_in(&ArchiveEnv::current())
}

fn groups_in(env: &ArchiveEnv) -> Vec<ArchiveGroup> {
    group::registry()
        .iter()
        .map(|spec| {
            let (id, title, description, sensitive) = spec.meta();
            let entries = spec.collect(env);
            let sizes = || {
                entries
                    .iter()
                    .filter_map(|entry| std::fs::metadata(&entry.source).ok())
                    .map(|meta| meta.len())
            };
            ArchiveGroup {
                id: id.to_string(),
                title: title.to_string(),
                description: description.to_string(),
                sensitive,
                files: entries.len() as u64,
                bytes: sizes().sum(),
                largest_file_bytes: sizes().max().unwrap_or(0),
            }
        })
        .collect()
}

/// 导出：`selected` 为空表示全部组；未知 id 直接报错。
///
/// 先写 `{dest}.tmp` 再 rename：中途失败不会留下半个可用文件。
pub fn export(selected: &[String], dest: &Path) -> Result<ExportOutcome, String> {
    export_in(&ArchiveEnv::current(), selected, dest)
}

fn export_in(env: &ArchiveEnv, selected: &[String], dest: &Path) -> Result<ExportOutcome, String> {
    validate_dest(dest)?;
    let ids = resolve_selection(selected)?;

    let mut entries: Vec<(String, ZipPayload)> = Vec::new();
    let mut stats: Vec<ManifestGroup> = Vec::new();
    for spec in group::registry() {
        let id = spec.id();
        if !ids.iter().any(|wanted| wanted == id) {
            continue;
        }
        let collected = spec.collect(env);
        if collected.is_empty() {
            // 本机没有该组数据：不写进清单，避免前端看到「导出了 0 个文件」的组。
            continue;
        }
        let bytes: u64 = collected
            .iter()
            .filter_map(|entry| std::fs::metadata(&entry.source).ok())
            .map(|meta| meta.len())
            .sum();
        stats.push(ManifestGroup {
            id: id.to_string(),
            files: collected.len() as u64,
            bytes,
        });
        for entry in collected {
            entries.push((entry.name, ZipPayload::File(entry.source)));
        }
    }
    if entries.is_empty() {
        return Err("没有可导出的数据：所选分组在本机都是空的".to_string());
    }

    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        app_version: update::APP_VERSION.to_string(),
        exported_at: chrono::Utc::now().timestamp(),
        groups: stats,
    };
    let mut all: Vec<(String, ZipPayload)> = Vec::with_capacity(entries.len() + 1);
    all.push((
        MANIFEST_NAME.to_string(),
        ZipPayload::Bytes(manifest.to_json()?.into_bytes()),
    ));
    all.extend(entries);

    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建导出目录失败（{}）：{e}", parent.display()))?;
        }
    }
    let file_name = dest
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "导出文件名不合法".to_string())?;
    let tmp = dest.with_file_name(format!("{file_name}.tmp"));
    if let Err(error) = zip::write_archive(&tmp, &all) {
        // 写一半失败也要把 tmp 清掉，别在用户选的目录里留垃圾。
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    // `std::fs::rename` 在 Windows 上走 MOVEFILE_REPLACE_EXISTING，本身就会覆盖旧包；
    // 先删再换会留下「旧包已删、新包还没就位」的空窗（崩溃时备份直接没了）。
    if let Err(error) = std::fs::rename(&tmp, dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("归档落盘失败（{}）：{error}", dest.display()));
    }
    let bytes = std::fs::metadata(dest)
        .map_err(|e| format!("读取归档大小失败：{e}"))?
        .len();

    Ok(ExportOutcome {
        path: dest.to_string_lossy().into_owned(),
        bytes,
        groups: manifest.groups.iter().map(|g| g.id.clone()).collect(),
    })
}

fn validate_dest(dest: &Path) -> Result<(), String> {
    if dest.as_os_str().is_empty() {
        return Err("请先选择导出位置".to_string());
    }
    if !dest.is_absolute() {
        return Err("导出路径必须是绝对路径".to_string());
    }
    if dest.is_dir() {
        return Err("导出路径不能是目录".to_string());
    }
    Ok(())
}

/// 选择解析：空 = 全部；未知 id 报错；去重并保持注册表顺序（导出/清单稳定）。
fn resolve_selection(selected: &[String]) -> Result<Vec<String>, String> {
    let registry_ids: Vec<&str> = group::registry().iter().map(|spec| spec.id()).collect();
    if selected.is_empty() {
        return Ok(registry_ids.into_iter().map(str::to_string).collect());
    }
    for raw in selected {
        let id = raw.trim();
        if id.is_empty() {
            continue;
        }
        if !registry_ids.contains(&id) {
            return Err(format!("未知的数据分组：{id}"));
        }
    }
    let ordered: Vec<String> = registry_ids
        .into_iter()
        .filter(|id| selected.iter().any(|raw| raw.trim() == *id))
        .map(str::to_string)
        .collect();
    if ordered.is_empty() {
        return Err("未选择任何数据分组".to_string());
    }
    Ok(ordered)
}

/// 读归档清单：只解压 manifest，不落盘。
pub fn inspect(archive: &Path) -> Result<ArchiveInspect, String> {
    inspect_in(archive)
}

fn inspect_in(archive: &Path) -> Result<ArchiveInspect, String> {
    let raw = zip::read_entry(archive, MANIFEST_NAME)?;
    let manifest = Manifest::from_json(&raw)?;
    let registry = group::registry();
    let groups = manifest
        .groups
        .iter()
        .map(|record| match registry.iter().find(|spec| spec.id() == record.id) {
            Some(spec) => {
                let (id, title, description, sensitive) = spec.meta();
                ArchiveGroup {
                    id: if record.id.is_empty() {
                        id.to_string()
                    } else {
                        record.id.clone()
                    },
                    title: title.to_string(),
                    description: description.to_string(),
                    sensitive,
                    files: record.files,
                    bytes: record.bytes,
                    // 清单里不记单文件大小：导入侧只按「组」勾选，用不到这个值。
                    largest_file_bytes: 0,
                }
            }
            // 未来版本新增的组：如实展示 id 与统计，让用户知道包里还有东西，
            // 但不假装能识别它（导入时这类组会被跳过）。
            None => ArchiveGroup {
                id: record.id.clone(),
                title: record.id.clone(),
                description: "当前版本无法识别的分组，导入时会跳过".to_string(),
                sensitive: false,
                files: record.files,
                bytes: record.bytes,
                largest_file_bytes: 0,
            },
        })
        .collect();
    Ok(ArchiveInspect {
        format_version: manifest.format_version,
        app_version: manifest.app_version,
        exported_at: manifest.exported_at,
        groups,
    })
}

/// 导入：`selected` 为空表示 zip 里全部组；只导入「归档里确实存在且被选中」的组。
pub fn import(archive: &Path, selected: &[String]) -> Result<ImportOutcome, String> {
    import_in(&ArchiveEnv::current(), archive, selected)
}

fn import_in(
    env: &ArchiveEnv,
    archive: &Path,
    selected: &[String],
) -> Result<ImportOutcome, String> {
    import_with_registry(env, archive, selected, group::registry())
}

/// 导入主流程；registry 可注入（测试用「必定失败的组」验证回滚）。
fn import_with_registry(
    env: &ArchiveEnv,
    archive: &Path,
    selected: &[String],
    registry: Vec<Box<dyn DataGroupSpec>>,
) -> Result<ImportOutcome, String> {
    // 两级互斥：进程内 Mutex 拦同进程并发（Windows 上并发 CreateFile 会偶发 ACCESS_DENIED），
    // 文件锁拦桌面端与 webui/CLI 同时导入。锁覆盖「备份 + 写入 + 回滚」全过程。
    let _process_guard = IMPORT_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let _file_guard = match try_lock_file(&env.store.join("imports.lock")) {
        Ok(lock) => lock,
        Err(LockError::Busy) => {
            return Err("另一个导入正在进行中，请稍后再试".to_string());
        }
        Err(LockError::Unavailable(error)) => {
            return Err(format!("无法建立导入互斥（{}）：{error}", env.store.display()));
        }
    };

    let files = zip::read_archive(archive)?;
    let raw_manifest = files
        .get(MANIFEST_NAME)
        .ok_or_else(|| "归档缺少 archive-manifest.json，不是本工具导出的备份".to_string())?;
    let manifest = Manifest::from_json(raw_manifest)?;

    let known: Vec<&str> = registry.iter().map(|spec| spec.id()).collect();
    let wanted: Vec<String> = if selected.is_empty() {
        manifest.groups.iter().map(|g| g.id.clone()).collect()
    } else {
        // 未知 id 静默丢掉：老版本读到新版本备份时，前端会把清单里的组全部勾上，
        // 其中「当前版本不认识的组」本来就不该阻断其余组的导入。只有全都不认识才报错。
        let mut ids: Vec<String> = Vec::new();
        for raw in selected {
            let id = raw.trim();
            if id.is_empty() || !known.contains(&id) {
                continue;
            }
            if !ids.iter().any(|kept| kept == id) {
                ids.push(id.to_string());
            }
        }
        if ids.is_empty() {
            return Err("所选分组当前版本都无法识别，请升级应用后再导入".to_string());
        }
        ids
    };
    if wanted.is_empty() {
        return Err("未选择任何数据分组".to_string());
    }

    // 只处理：已知组 + 被选中 + zip 里确实有条目。清单里有名字但没有条目
    // （旧版本导出的空组）不当作错误。
    let plan: Vec<Box<dyn DataGroupSpec>> = registry
        .into_iter()
        .filter(|spec| {
            wanted.iter().any(|id| id == spec.id()) && has_group_entries(&files, spec.id())
        })
        .collect();
    if plan.is_empty() {
        return Err("归档中没有可导入的数据：所选分组在归档里都是空的".to_string());
    }

    // 先算「会写哪些文件」：白名单/重复目标都在这里判掉，此时磁盘上什么都没动。
    // 这份清单同时用于备份与回滚，两边不会漂移。
    let mut group_plans: Vec<GroupPlan> = Vec::new();
    for spec in &plan {
        group_plans.push(GroupPlan {
            id: spec.id().to_string(),
            targets: plan_targets(env, spec.id(), &files)?,
        });
    }

    // 全量备份先于任何写盘：备份失败就中止，用户数据保持原样。
    let backup_dir = create_backup_dir(env)?;
    for group_plan in &group_plans {
        backup_targets(&backup_dir, group_plan)?;
    }

    let mut ctx = ImportContext {
        env,
        files,
        accounts_added: 0,
        accounts_updated: 0,
    };
    for spec in &plan {
        if let Err(error) = spec.apply(&mut ctx) {
            // 已经写过的组（含当前组写了一半的）按备份还原；导入新建的文件删除。
            return Err(rollback_error(env, error, &backup_dir, &group_plans));
        }
    }

    Ok(ImportOutcome {
        groups: plan.iter().map(|spec| spec.id().to_string()).collect(),
        backup_dir: backup_dir.to_string_lossy().into_owned(),
        accounts_added: ctx.accounts_added,
        accounts_updated: ctx.accounts_updated,
    })
}

/// 一个分组在本次导入里会碰到的全部文件。
struct GroupPlan {
    id: String,
    targets: Vec<PlannedTarget>,
}

/// 单个目标文件：写盘位置 + 归档内条目名（备份路径直接复用它，保留分组与层级）。
struct PlannedTarget {
    archive_name: String,
    target: PathBuf,
}

/// 按白名单把某组的条目映射成目标文件清单。
fn plan_targets(
    env: &ArchiveEnv,
    group_id: &str,
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<Vec<PlannedTarget>, String> {
    let prefix = format!("{group_id}/");
    let root = group::group_root(env, group_id);
    let mut targets: Vec<PlannedTarget> = Vec::new();
    // Windows 文件系统大小写不敏感：两个只差大小写的条目会写同一路径，显式拒绝。
    let mut seen: Vec<String> = Vec::new();
    for name in files.keys() {
        let Some(rel) = name.strip_prefix(&prefix) else {
            continue;
        };
        let Some(rel_path) = group::relative_target(group_id, rel) else {
            return Err(format!("归档中的条目不属于可恢复范围：{name}"));
        };
        let target = root.join(rel_path);
        let dedupe_key = target.to_string_lossy().to_lowercase();
        if seen.contains(&dedupe_key) {
            return Err(format!("归档中存在重复的目标文件：{name}"));
        }
        seen.push(dedupe_key);
        targets.push(PlannedTarget {
            archive_name: name.clone(),
            target,
        });
    }
    Ok(targets)
}

fn has_group_entries(files: &BTreeMap<String, Vec<u8>>, group_id: &str) -> bool {
    let prefix = format!("{group_id}/");
    files.keys().any(|name| name.starts_with(&prefix))
}

/// 备份目录：`{store}/backups/archive-YYYYMMDD-HHMMSS`，同秒多次导入加序号避免互相覆盖。
fn create_backup_dir(env: &ArchiveEnv) -> Result<PathBuf, String> {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let base = env.store.join("backups");
    std::fs::create_dir_all(&base)
        .map_err(|e| format!("创建备份目录失败（{}）：{e}", base.display()))?;
    let mut candidate = base.join(format!("archive-{stamp}"));
    let mut attempt = 1;
    while candidate.exists() {
        candidate = base.join(format!("archive-{stamp}-{attempt}"));
        attempt += 1;
        if attempt > 100 {
            return Err("备份目录名冲突过多，请稍后重试".to_string());
        }
    }
    std::fs::create_dir_all(&candidate)
        .map_err(|e| format!("创建备份目录失败（{}）：{e}", candidate.display()))?;
    Ok(candidate)
}

/// 备份本组将被覆盖的文件（只备份已存在的；新文件无需备份）。
///
/// 备份路径沿用 zip 条目名（已通过安全校验，含组前缀），保留分组与层级，
/// 回滚与人工恢复都可以直接按目录结构拷回。
fn backup_targets(backup_dir: &Path, group_plan: &GroupPlan) -> Result<(), String> {
    for planned in &group_plan.targets {
        if !planned.target.exists() {
            continue;
        }
        let backup = backup_dir.join(&planned.archive_name);
        if let Some(parent) = backup.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建备份子目录失败（{}）：{e}", parent.display()))?;
        }
        std::fs::copy(&planned.target, &backup).map_err(|e| {
            format!(
                "备份失败，已中止导入（{} → {}）：{e}",
                planned.target.display(),
                backup.display()
            )
        })?;
    }
    Ok(())
}

/// 把 `apply` 失败后的现场还原到导入前：有备份的拷回，导入新建的删除。
///
/// 逐个文件尽力而为：单个文件恢复失败不阻止其它文件回滚，最后一次性报告，
/// 让用户能按提示手动补齐。
fn rollback(env: &ArchiveEnv, backup_dir: &Path, group_plans: &[GroupPlan]) -> Vec<String> {
    let mut failures: Vec<String> = Vec::new();
    for group_plan in group_plans {
        for planned in &group_plan.targets {
            let backup = backup_dir.join(&planned.archive_name);
            if backup.is_file() {
                if let Some(parent) = planned.target.parent() {
                    if let Err(error) = std::fs::create_dir_all(parent) {
                        failures.push(format!(
                            "{}：无法创建目录 {}（{error}）",
                            group_plan.id,
                            parent.display()
                        ));
                        continue;
                    }
                }
                if let Err(error) = restore_file(env, &group_plan.id, &backup, &planned.target) {
                    failures.push(format!(
                        "{}：恢复 {} 失败（{error}）",
                        group_plan.id,
                        planned.target.display()
                    ));
                }
            } else if planned.target.exists() {
                // 备份里没有 = 导入时新建的；删除它才叫「回到导入前」。
                if let Err(error) = std::fs::remove_file(&planned.target) {
                    failures.push(format!(
                        "{}：删除新建文件 {} 失败（{error}）",
                        group_plan.id,
                        planned.target.display()
                    ));
                }
            }
        }
    }
    failures
}

/// 单个文件回滚。
///
/// 不用 `fs::copy`：它先截断目标再写，回滚途中崩溃会把 `accounts.json` 变成空文件；
/// 账号文件还必须持 `with_accounts_write` 的锁，避免覆盖同时发生的额度刷新。
fn restore_file(
    env: &ArchiveEnv,
    group_id: &str,
    backup: &Path,
    target: &Path,
) -> Result<(), String> {
    let text = std::fs::read_to_string(backup)
        .map_err(|error| format!("读取备份失败（{}）：{error}", backup.display()))?;
    if group_id == group::ACCOUNTS_ID {
        let records: Vec<serde_json::Value> = serde_json::from_str(&text)
            .map_err(|error| format!("备份里的账号文件无法解析：{error}"))?;
        return account::with_accounts_write(&env.accounts_file(), move |accounts| {
            *accounts = records;
            Ok(())
        });
    }
    config::atomic_write(target, &text)
        .map_err(|error| format!("写入 {} 失败：{error}", target.display()))
}

/// 把「导入失败 + 回滚结果」拼成给用户看的错误文案。
fn rollback_error(
    env: &ArchiveEnv,
    error: String,
    backup_dir: &Path,
    group_plans: &[GroupPlan],
) -> String {
    let failures = rollback(env, backup_dir, group_plans);
    if failures.is_empty() {
        format!(
            "导入失败：{error}；已回滚到导入前状态（备份保留在 {}）",
            backup_dir.display()
        )
    } else {
        format!(
            "导入失败：{error}；自动回滚未完成（{}）。请手动用备份目录恢复：{}",
            failures.join("；"),
            backup_dir.display()
        )
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::path::{Path, PathBuf};

    use super::ArchiveEnv;

    /// 每个用例独立临时根目录，Drop 时清理；不碰 `WB_SWITCH_HOME`，可安全并行。
    pub(crate) struct TempRoot {
        path: PathBuf,
    }

    impl TempRoot {
        pub(crate) fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "wb-switch-archive-{tag}-{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        pub(crate) fn path(&self) -> &Path {
            &self.path
        }

        pub(crate) fn env(&self) -> ArchiveEnv {
            ArchiveEnv::at(&self.path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::testutil::TempRoot;
    use super::group::{ArchiveEntry, DataGroupSpec};
    use super::*;

    /// 在沙箱里铺一份「有代表性」的数据：账号 + 设置 + 日志 + 统计 + 记忆 + 关联。
    fn seed(env: &ArchiveEnv) {
        std::fs::create_dir_all(&env.store).unwrap();
        std::fs::write(
            env.accounts_file(),
            r#"[{"id":"local-1","uid":"u1","nickname":"旧名","access_token":"tok-1"}]"#,
        )
        .unwrap();
        std::fs::write(env.store.join("auto_checkin_config.json"), r#"{"enabled":true}"#).unwrap();
        std::fs::write(env.store.join("auto_checkin_logs.json"), r#"[{"at":1}]"#).unwrap();
        std::fs::write(env.store.join("credit_usage_snapshots.json"), r#"{"u1":1}"#).unwrap();
        std::fs::write(env.store.join("session_links.json"), r#"{"version":1}"#).unwrap();
        std::fs::create_dir_all(env.store.join("session-links").join("baselines")).unwrap();
        std::fs::write(
            env.store.join("session-links").join("baselines").join("a.json"),
            r#"{"b":1}"#,
        )
        .unwrap();
        let personal = env.home.join(".workbuddy").join("user-u1-personal");
        std::fs::create_dir_all(&personal).unwrap();
        std::fs::write(personal.join("MEMORY.md"), "# 记忆").unwrap();
        // 国际版数据根：只玩国际版的用户记忆全在这里，必须一起收。
        let ai_personal = env
            .home
            .join(".workbuddy-ai")
            .join("user-ai1-personal");
        std::fs::create_dir_all(&ai_personal).unwrap();
        std::fs::write(ai_personal.join("MEMORY.md"), "# ai 记忆").unwrap();
        // 专家团：只带名字清单，不带插件本体；两个档位各一份。
        let experts = env
            .home
            .join(".workbuddy")
            .join("experts")
            .join("custom")
            .join("u1");
        std::fs::create_dir_all(&experts).unwrap();
        std::fs::write(experts.join("experts.json"), r#"["团队A"]"#).unwrap();
        let ai_experts = env
            .home
            .join(".workbuddy-ai")
            .join("experts")
            .join("custom")
            .join("ai1");
        std::fs::create_dir_all(&ai_experts).unwrap();
        std::fs::write(ai_experts.join("experts.json"), r#"["团队B"]"#).unwrap();
    }

    fn manifest_json(group_ids: &[&str]) -> String {
        let groups: Vec<String> = group_ids
            .iter()
            .map(|id| format!(r#"{{"id":"{id}","files":1,"bytes":1}}"#))
            .collect();
        format!(
            r#"{{"formatVersion":1,"appVersion":"1.0.0","exportedAt":1700000000,"groups":[{}]}}"#,
            groups.join(",")
        )
    }

    /// 手工构造归档：只用于「恶意 / 畸形输入」用例。
    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        use std::io::Write;
        // `super::*` 里有本地模块 `zip`，这里显式用 `::zip` 指外部 crate。
        use ::zip::write::SimpleFileOptions;
        use ::zip::{CompressionMethod, ZipWriter};

        let file = std::fs::File::create(path).unwrap();
        let mut writer = ZipWriter::new(std::io::BufWriter::new(file));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, bytes) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn export_inspect_import_round_trip() {
        let source = TempRoot::new("roundtrip-src");
        let src_env = source.env();
        seed(&src_env);
        let dest = source.path().join("backup.zip");

        let outcome = export_in(&src_env, &[], &dest).unwrap();
        assert_eq!(
            outcome.groups,
            vec![
                "accounts", "settings", "logs", "stats", "memory", "experts", "links"
            ]
        );
        assert!(outcome.bytes > 0);
        assert_eq!(outcome.path, dest.to_string_lossy());

        let inspected = inspect_in(&dest).unwrap();
        assert_eq!(inspected.format_version, 1);
        assert!(!inspected.app_version.is_empty());
        assert!(inspected.exported_at > 0);
        assert_eq!(inspected.groups.len(), 7);
        let accounts = inspected
            .groups
            .iter()
            .find(|g| g.id == "accounts")
            .unwrap();
        assert!(accounts.sensitive, "账号组必须标记为敏感");
        assert_eq!(accounts.files, 1);

        let target = TempRoot::new("roundtrip-dst");
        let dst_env = target.env();
        let imported = import_in(&dst_env, &dest, &[]).unwrap();
        assert!(
            imported.groups.iter().any(|group| group == "memory"),
            "导入应包含 memory：{:?}",
            imported.groups
        );
        assert_eq!(imported.accounts_added, 1);
        assert_eq!(imported.accounts_updated, 0);
        assert!(Path::new(&imported.backup_dir).is_dir(), "应生成备份目录");

        assert_eq!(
            std::fs::read_to_string(dst_env.store.join("auto_checkin_config.json")).unwrap(),
            r#"{"enabled":true}"#
        );
        assert!(dst_env
            .store
            .join("session-links")
            .join("baselines")
            .join("a.json")
            .is_file());
        assert_eq!(
            std::fs::read_to_string(
                dst_env
                    .home
                    .join(".workbuddy")
                    .join("user-u1-personal")
                    .join("MEMORY.md")
            )
            .unwrap(),
            "# 记忆"
        );
        // 国际版根同样要还原，否则只玩国际版的用户换机后记忆全丢。
        assert_eq!(
            std::fs::read_to_string(
                dst_env
                    .home
                    .join(".workbuddy-ai")
                    .join("user-ai1-personal")
                    .join("MEMORY.md")
            )
            .unwrap(),
            "# ai 记忆"
        );
        assert_eq!(
            std::fs::read_to_string(
                dst_env
                    .home
                    .join(".workbuddy")
                    .join("experts")
                    .join("custom")
                    .join("u1")
                    .join("experts.json")
            )
            .unwrap(),
            r#"["团队A"]"#
        );
        assert_eq!(
            std::fs::read_to_string(
                dst_env
                    .home
                    .join(".workbuddy-ai")
                    .join("experts")
                    .join("custom")
                    .join("ai1")
                    .join("experts.json")
            )
            .unwrap(),
            r#"["团队B"]"#
        );

        // 再导入一次：同 uid 更新而不是新增。
        let again = import_in(&dst_env, &dest, &[]).unwrap();
        assert_eq!(again.accounts_added, 0);
        assert_eq!(again.accounts_updated, 1);
        let accounts: Vec<serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(dst_env.accounts_file()).unwrap())
                .unwrap();
        assert_eq!(accounts.len(), 1);
    }

    #[test]
    fn export_only_selected_groups() {
        let source = TempRoot::new("selected-src");
        let env = source.env();
        seed(&env);
        let dest = source.path().join("settings.zip");

        let outcome = export_in(&env, &["settings".to_string()], &dest).unwrap();
        assert_eq!(outcome.groups, vec!["settings"]);

        let inspected = inspect_in(&dest).unwrap();
        assert_eq!(inspected.groups.len(), 1);
        assert_eq!(inspected.groups[0].id, "settings");

        let entries = zip::read_archive(&dest).unwrap();
        assert!(entries.contains_key(MANIFEST_NAME));
        let data: Vec<&String> = entries.keys().filter(|k| *k != MANIFEST_NAME).collect();
        assert_eq!(data, vec!["settings/auto_checkin_config.json"]);

        // 只导入 settings：新环境下不应出现账号 / 日志 / 记忆。
        let target = TempRoot::new("selected-dst");
        let dst_env = target.env();
        let imported = import_in(&dst_env, &dest, &[]).unwrap();
        assert_eq!(imported.groups, vec!["settings"]);
        assert!(dst_env.store.join("auto_checkin_config.json").is_file());
        assert!(!dst_env.accounts_file().exists());
        assert!(!dst_env.home.join(".workbuddy").exists());
    }

    #[test]
    fn import_rejects_traversal_and_whitelist_violations() {
        let root = TempRoot::new("malicious");
        let env = root.env();

        // 1) zip slip：条目名本身越界，read_archive 阶段就拒（连 manifest 都不看）。
        let evil = root.path().join("evil.zip");
        write_zip(
            &evil,
            &[
                (MANIFEST_NAME, manifest_json(&["settings"]).as_bytes()),
                ("../evil.json", b"{}"),
            ],
        );
        let err = import_in(&env, &evil, &[]).unwrap_err();
        assert!(err.contains("相对跳转"), "{err}");

        // 2) 前缀合法但落点不在白名单：备份阶段（写盘之前）拒绝。
        let sneaky = root.path().join("sneaky.zip");
        write_zip(
            &sneaky,
            &[
                (MANIFEST_NAME, manifest_json(&["settings"]).as_bytes()),
                ("settings/evil.exe", b"boom"),
            ],
        );
        let err = import_in(&env, &sneaky, &[]).unwrap_err();
        assert!(err.contains("不属于可恢复范围"), "{err}");
        assert!(!env.store.join("evil.exe").exists());
    }

    #[test]
    fn unknown_group_is_rejected_on_export_but_skipped_on_import() {
        let root = TempRoot::new("unknown");
        let env = root.env();
        seed(&env);
        let dest = root.path().join("a.zip");
        // 导出：未知 id 说明调用方拼错了，直接报错。
        assert!(export_in(&env, &["nope".to_string()], &dest).is_err());

        let export = export_in(&env, &[], &dest).unwrap();
        assert!(!export.groups.is_empty());

        // 导入：老版本读到新版本备份时，前端会把清单里的组全勾上；不认识的组应跳过，
        // 而不是把整次导入拖死。只有「一个都不认识」才报错。
        let outcome = import_in(
            &env,
            &dest,
            &["nope".to_string(), "settings".to_string()],
        )
        .unwrap();
        assert_eq!(outcome.groups, vec!["settings".to_string()]);

        let err = import_in(&env, &dest, &["nope".to_string()]).unwrap_err();
        assert!(err.contains("无法识别"), "{err}");
    }

    #[test]
    fn backup_failure_aborts_before_any_write() {
        let source = TempRoot::new("backup-src");
        let src_env = source.env();
        seed(&src_env);
        let dest = source.path().join("settings.zip");
        export_in(&src_env, &["settings".to_string()], &dest).unwrap();

        // 目标侧：把将被备份的文件做成目录 → fs::copy 必然失败。
        let target = TempRoot::new("backup-dst");
        let dst_env = target.env();
        std::fs::create_dir_all(&dst_env.store).unwrap();
        std::fs::create_dir_all(dst_env.store.join("auto_checkin_config.json")).unwrap();
        std::fs::write(
            dst_env.store.join("rate_limit_config.json"),
            r#"{"keep":"me"}"#,
        )
        .unwrap();

        let err = import_in(&dst_env, &dest, &[]).unwrap_err();
        assert!(err.contains("备份失败"), "{err}");
        assert_eq!(
            std::fs::read_to_string(dst_env.store.join("rate_limit_config.json")).unwrap(),
            r#"{"keep":"me"}"#,
            "备份失败必须中止，不能有任何文件被覆盖"
        );
    }

    #[test]
    fn accounts_merge_counts_added_and_updated_by_uid() {
        let source = TempRoot::new("merge-src");
        let src_env = source.env();
        seed(&src_env);
        let dest = source.path().join("accounts.zip");
        export_in(&src_env, &["accounts".to_string()], &dest).unwrap();

        let target = TempRoot::new("merge-dst");
        let dst_env = target.env();
        std::fs::create_dir_all(&dst_env.store).unwrap();
        std::fs::write(
            dst_env.accounts_file(),
            r#"[{"id":"local-1","uid":"local","nickname":"本地","access_token":"tok-local"}]"#,
        )
        .unwrap();

        let outcome = import_in(&dst_env, &dest, &[]).unwrap();
        assert_eq!(outcome.accounts_added, 1, "归档里的 u1 是新 uid，应追加");
        assert_eq!(outcome.accounts_updated, 0);

        // 归档里的 u1 再改一次名字 → 同 uid 覆盖。
        std::fs::write(
            dst_env.accounts_file(),
            r#"[{"id":"local-1","uid":"u1","nickname":"旧名","access_token":"tok-old"}]"#,
        )
        .unwrap();
        let outcome = import_in(&dst_env, &dest, &[]).unwrap();
        assert_eq!(outcome.accounts_added, 0);
        assert_eq!(outcome.accounts_updated, 1);
        let accounts: Vec<serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(dst_env.accounts_file()).unwrap())
                .unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0]["access_token"], "tok-1");
    }

    #[test]
    fn groups_report_disk_state_and_sensitive_flag() {
        let source = TempRoot::new("groups");
        let env = source.env();
        seed(&env);
        let groups = groups_in(&env);
        let accounts = groups.iter().find(|g| g.id == "accounts").unwrap();
        assert!(accounts.sensitive);
        assert_eq!(accounts.files, 1);
        assert!(accounts.bytes > 0);
        let empty = groups.iter().find(|g| g.id == "stats").unwrap();
        assert_eq!(empty.files, 1);
        let experts = groups.iter().find(|g| g.id == "experts").unwrap();
        assert_eq!(experts.files, 2, "两个档位各一份清单");
        assert!(!experts.sensitive);
    }

    #[test]
    fn inspect_without_manifest_is_rejected() {
        let root = TempRoot::new("no-manifest");
        let archive = root.path().join("plain.zip");
        write_zip(&archive, &[("accounts/accounts.json", b"[]")]);
        let err = inspect_in(&archive).unwrap_err();
        assert!(err.contains("archive-manifest.json"), "{err}");
    }

    #[test]
    fn import_skips_groups_missing_from_selection() {
        let source = TempRoot::new("skip-src");
        let env = source.env();
        seed(&env);
        let dest = source.path().join("all.zip");
        export_in(&env, &[], &dest).unwrap();

        let target = TempRoot::new("skip-dst");
        let dst_env = target.env();
        let outcome = import_in(&dst_env, &dest, &["logs".to_string()]).unwrap();
        assert_eq!(outcome.groups, vec!["logs"]);
        assert!(dst_env.store.join("auto_checkin_logs.json").is_file());
        assert!(!dst_env.store.join("accounts.json").exists());
    }

    #[test]
    fn import_without_any_entry_is_rejected() {
        let root = TempRoot::new("empty-plan");
        let env = root.env();
        let archive = root.path().join("empty.zip");
        let mut entries: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        entries.insert(MANIFEST_NAME.to_string(), manifest_json(&["settings"]).into_bytes());
        assert!(!has_group_entries(&entries, "settings"));
        write_zip(
            &archive,
            &[(MANIFEST_NAME, manifest_json(&["settings"]).as_bytes())],
        );
        let err = import_in(&env, &archive, &[]).unwrap_err();
        assert!(err.contains("没有可导入的数据"), "{err}");
    }

    /// 测试用「必定失败」的组：复用真实 logs 的 id，让导入计划里排在 settings 之后，
    /// 从而在 settings 已经写盘后失败，验证回滚。
    struct FailingLogsGroup;

    impl DataGroupSpec for FailingLogsGroup {
        fn meta(&self) -> (&'static str, &'static str, &'static str, bool) {
            ("logs", "日志（测试）", "测试注入的失败分组", false)
        }

        fn collect(&self, _env: &ArchiveEnv) -> Vec<ArchiveEntry> {
            Vec::new()
        }

        fn apply(&self, _ctx: &mut group::ImportContext<'_>) -> Result<(), String> {
            Err("注入的失败".to_string())
        }
    }

    #[test]
    fn apply_failure_rolls_back_written_files_and_removes_new_ones() {
        let source = TempRoot::new("rollback-src");
        let src_env = source.env();
        seed(&src_env);
        // 归档里多一个本机（目标侧）没有的设置文件：回滚时必须删除。
        std::fs::write(
            src_env.store.join("rate_limit_config.json"),
            r#"{"from":"archive"}"#,
        )
        .unwrap();
        let dest = source.path().join("all.zip");
        export_in(&src_env, &[], &dest).unwrap();

        let target = TempRoot::new("rollback-dst");
        let dst_env = target.env();
        std::fs::create_dir_all(&dst_env.store).unwrap();
        // 导入前已存在的文件：内容与归档不同，回滚后必须恢复成 BEFORE。
        std::fs::write(
            dst_env.store.join("auto_checkin_config.json"),
            "BEFORE",
        )
        .unwrap();

        let registry: Vec<Box<dyn DataGroupSpec>> =
            vec![Box::new(group::SettingsGroup), Box::new(FailingLogsGroup)];
        let err = import_with_registry(&dst_env, &dest, &[], registry).unwrap_err();
        assert!(err.contains("注入的失败"), "应保留原始失败原因：{err}");
        assert!(err.contains("已回滚到导入前状态"), "应说明已回滚：{err}");
        assert_eq!(
            std::fs::read_to_string(dst_env.store.join("auto_checkin_config.json")).unwrap(),
            "BEFORE",
            "已写文件必须恢复为导入前内容"
        );
        assert!(
            !dst_env.store.join("rate_limit_config.json").exists(),
            "导入新建的文件必须删除"
        );
    }
}
