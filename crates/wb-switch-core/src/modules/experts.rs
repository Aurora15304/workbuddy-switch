//! 跨账号「自定义专家团」清单的合并与分发。
//!
//! WorkBuddy 把用户自己添加的专家团按账号隔离，记在
//! `{home}/.workbuddy/experts/custom/<uid>/experts.json` 里，内容是一个**字符串数组**，
//! 每个元素是一个专家团的名字。专家团的插件本体在全局 `~/.workbuddy/plugins/marketplaces/`
//! 下、跨账号共享，所以「切号后专家团就没了」只是这份名字清单跟着账号走。把各账号的清单
//! 取并集写回每个账号，已添加的专家团就出现在所有账号里。
//!
//! 三条硬约束：
//! 1. **只增不减**：合并只做并集与去重，绝不删除任何账号已添加的名字；目标侧已有文件先备份成
//!    `experts.json.bak-<毫秒时间戳>`，随时可回滚。
//! 2. **非法内容一字不动**：读取失败、不是合法 JSON、顶层不是数组、数组里混入非字符串元素，
//!    都只把该账号记成一条错误并跳过写入。静默丢弃不认识的元素，写回时就会删掉用户数据。
//! 3. **内容一致不写**：比较**解析后的去重集合**而不是字节。App 可能按插入顺序写这份清单，
//!    比字节会让每次切号都重写、都留一份备份。
//!
//! 不参与同步的东西：`storage/user-<uid>-personal/global/expert.json` 是「最近使用」历史，
//! `scoped/<hash>/expert-picker.json` 是置顶状态——两者都是使用痕迹，同步过去等于伪造记录。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::modules::config::{atomic_write, now_ms, wb_data_root};

/// 专家团清单文件名：唯一参与同步的文件。
const EXPERTS_FILE: &str = "experts.json";

/// 指定 uid 的自定义专家团目录：`{root}/experts/custom/<uid>`。
fn custom_dir_at(root: &Path, uid: &str) -> PathBuf {
    root.join("experts").join("custom").join(uid)
}

/// 把本机所有账号已添加的专家团取并集，写回每一个账号。
///
/// `accounts` 是 `[(uid, label)]`（调用方从账号库投影），缺 uid 的账号记一条错误跳过——
/// uid 是目录名的唯一依据，猜不出来。
pub fn merge_and_distribute(accounts: &[(String, String)]) -> Value {
    merge_and_distribute_at(&wb_data_root(), accounts)
}

fn merge_and_distribute_at(root: &Path, accounts: &[(String, String)]) -> Value {
    let mut errors: Vec<Value> = Vec::new();
    // 读得到的账号才进 `known`：不可信的账号读不到，也就不会被写回覆盖。
    let mut known: Vec<AccountExperts> = Vec::new();
    let mut merged: BTreeSet<String> = BTreeSet::new();

    for (uid, label) in accounts {
        if uid.trim().is_empty() {
            errors.push(json!({ "label": label, "error": "账号缺少 uid，无法定位专家目录" }));
            continue;
        }
        match read_experts_at(root, uid) {
            Ok(existing) => {
                if let Some(names) = &existing {
                    merged.extend(names.iter().cloned());
                }
                known.push(AccountExperts {
                    uid: uid.clone(),
                    label: label.clone(),
                    existing,
                });
            }
            Err(error) => errors.push(json!({ "label": label, "error": error })),
        }
    }

    // 排序 + 去重：排序是为了落盘稳定、与 App 的字节形态一致；幂等由 `same_set` 做集合
    // 比较保证，与顺序无关。
    let merged_value = Value::Array(merged.iter().map(|name| json!(name)).collect());
    if merged.is_empty() {
        return json!({
            "accounts": accounts.len(),
            "mergedExperts": merged_value,
            "mergedCount": 0,
            "written": [],
            "unchanged": [],
            "errors": errors,
            "note": "本机账号都没有可同步的专家团，未做任何写入",
        });
    }

    // 落盘格式与 App 一致：pretty、无尾换行。
    let Ok(content) = serde_json::to_string_pretty(&merged_value) else {
        // 字符串数组的序列化理论上不会失败；真失败也不能拿空串去覆盖用户文件，直接放弃写入。
        errors.push(json!({
            "label": "全部账号",
            "error": "专家团清单序列化失败，未写入任何账号",
        }));
        return json!({
            "accounts": accounts.len(),
            "mergedExperts": merged_value,
            "mergedCount": merged.len(),
            "written": [],
            "unchanged": [],
            "errors": errors,
        });
    };
    let mut written: Vec<Value> = Vec::new();
    let mut unchanged: Vec<Value> = Vec::new();

    for AccountExperts {
        uid,
        label,
        existing,
    } in &known
    {
        let dir = custom_dir_at(root, uid);
        let path = dir.join(EXPERTS_FILE);
        if existing
            .as_deref()
            .is_some_and(|names| same_set(names, &merged))
        {
            unchanged.push(json!({ "label": label }));
            continue;
        }
        if let Err(error) = std::fs::create_dir_all(&dir) {
            errors.push(json!({ "label": label, "error": format!("创建专家目录失败：{error}") }));
            continue;
        }
        let backup = if path.is_file() {
            let backup = backup_path(&path);
            match std::fs::copy(&path, &backup) {
                Ok(_) => Some(backup.to_string_lossy().to_string()),
                Err(error) => {
                    // 备份失败就不覆盖：宁可这个账号这次没同步，也不能让原清单不可恢复。
                    errors.push(json!({
                        "label": label,
                        "error": format!("备份 {EXPERTS_FILE} 失败，已跳过：{error}"),
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
                "backup": backup,
                "hadContent": existing.is_some(),
            })),
            Err(error) => errors.push(json!({
                "label": label,
                "error": format!("写入 {EXPERTS_FILE} 失败：{error}"),
            })),
        }
    }

    json!({
        "accounts": accounts.len(),
        "mergedExperts": merged_value,
        "mergedCount": merged.len(),
        "written": written,
        "unchanged": unchanged,
        "errors": errors,
    })
}

/// 一个账号的读取结果：`existing = None` 表示这份清单文件还不存在（全新账号）。
struct AccountExperts {
    uid: String,
    label: String,
    existing: Option<Vec<String>>,
}

/// 某个账号当前可用的专家团清单。
///
/// `Ok(None)` = 文件不存在（全新账号，不是错误）；`Err` = 内容不可信，调用方既不能采纳
/// 它的名字，也不能覆盖它。
fn read_experts_at(root: &Path, uid: &str) -> Result<Option<Vec<String>>, String> {
    let path = custom_dir_at(root, uid).join(EXPERTS_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("读取 {EXPERTS_FILE} 失败：{error}")),
    };
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| format!("{EXPERTS_FILE} 不是合法 JSON，已跳过该账号：{error}"))?;
    let Value::Array(items) = value else {
        return Err(format!(
            "{EXPERTS_FILE} 顶层不是数组，已跳过该账号（原文件未改动）"
        ));
    };
    let mut names: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        match item {
            Value::String(name) => names.push(name),
            _ => {
                return Err(format!(
                    "{EXPERTS_FILE} 含非字符串元素，整份清单视为不可信，已跳过该账号（原文件未改动）"
                ))
            }
        }
    }
    Ok(Some(names))
}

/// 幂等判定：比的是**去重后的集合**，不看顺序、缩进与重复项。
///
/// 只比长度会假阳性：`["x","x"]` 与并集 `{x,y}` 长度都是 2，两个 `"x"` 也都在并集里，
/// 于是「无变化」——那个账号就永远拿不到 `y`。
fn same_set(existing: &[String], merged: &BTreeSet<String>) -> bool {
    existing.iter().collect::<BTreeSet<&String>>() == merged.iter().collect::<BTreeSet<&String>>()
}

fn backup_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| EXPERTS_FILE.to_string());
    path.with_file_name(format!("{name}.bak-{}", now_ms()))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "wb_switch_experts_{}_{name}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write_raw(&self, uid: &str, text: &str) {
            let dir = custom_dir_at(self.path(), uid);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(EXPERTS_FILE), text).unwrap();
        }

        /// 用 App 的落盘格式写一份清单（pretty、无尾换行）。
        fn write_list(&self, uid: &str, names: &[&str]) {
            let value = json!(names);
            self.write_raw(uid, &serde_json::to_string_pretty(&value).unwrap());
        }

        fn read(&self, uid: &str) -> Option<String> {
            std::fs::read_to_string(custom_dir_at(self.path(), uid).join(EXPERTS_FILE)).ok()
        }

        /// 该账号目录下已有的备份文件名（只关心数量与内容）。
        fn backups(&self, uid: &str) -> Vec<PathBuf> {
            let dir = custom_dir_at(self.path(), uid);
            let Ok(entries) = std::fs::read_dir(&dir) else {
                return Vec::new();
            };
            let prefix = format!("{EXPERTS_FILE}.bak-");
            let mut found: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .map(|name| name.to_string_lossy().starts_with(&prefix))
                        .unwrap_or(false)
                })
                .collect();
            found.sort();
            found
        }

        fn exists(&self, uid: &str) -> bool {
            custom_dir_at(self.path(), uid).join(EXPERTS_FILE).is_file()
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

    fn parsed(content: &str) -> Value {
        serde_json::from_str(content).unwrap()
    }

    /// 并集写回每个账号：源账号的独有条目都要保住，缺文件的账号也要拿到一份。
    #[test]
    fn union_is_distributed_to_every_account() {
        let dir = TempDir::new("union");
        dir.write_list("a", &["lean-adaptive-engineering"]);
        dir.write_list("b", &["sales-coach"]);
        let report = merge_and_distribute_at(dir.path(), &[pair("a"), pair("b"), pair("c")]);

        assert_eq!(
            report["mergedExperts"],
            json!(["lean-adaptive-engineering", "sales-coach"])
        );
        assert_eq!(report["mergedCount"], 2);
        for uid in ["a", "b", "c"] {
            let content = dir.read(uid).unwrap_or_else(|| panic!("{uid} 未写入"));
            assert_eq!(
                parsed(&content),
                json!(["lean-adaptive-engineering", "sales-coach"]),
                "{uid}"
            );
        }
        // 三个账号内容都变了（a/b 被合并，c 从无到有）。
        assert_eq!(report["written"].as_array().unwrap().len(), 3);
        assert!(report["unchanged"].as_array().unwrap().is_empty());
        assert!(report["errors"].as_array().unwrap().is_empty(), "{report}");

        // 全新账号：目录被创建、没有备份、hadContent=false。
        assert!(custom_dir_at(dir.path(), "c").is_dir());
        let fresh = report["written"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["label"] == "acc-c")
            .expect("c 应在写入列表里");
        assert!(fresh["backup"].is_null(), "{fresh}");
        assert_eq!(fresh["hadContent"], false);
    }

    /// 合并结果按名字排序：这是对外声称的落盘契约，与 App 的字节形态一致。
    #[test]
    fn merged_experts_are_sorted() {
        let dir = TempDir::new("sorted");
        dir.write_list("a", &["zeta-expert"]);
        dir.write_list("b", &["alpha-expert", "mid-expert"]);
        let report = merge_and_distribute_at(dir.path(), &[pair("a"), pair("b")]);
        assert_eq!(
            report["mergedExperts"],
            json!(["alpha-expert", "mid-expert", "zeta-expert"])
        );
    }

    /// 只有一份清单时也分发全员：这是正常用法，不是「单来源不做合成」的特例。
    #[test]
    fn single_source_is_distributed_to_all() {
        let dir = TempDir::new("single");
        dir.write_list("a", &["lean-adaptive-engineering"]);
        let report = merge_and_distribute_at(dir.path(), &[pair("a"), pair("b"), pair("c")]);

        assert_eq!(
            parsed(&dir.read("b").unwrap()),
            json!(["lean-adaptive-engineering"])
        );
        assert_eq!(
            parsed(&dir.read("c").unwrap()),
            json!(["lean-adaptive-engineering"])
        );
        // 源账号内容已经是结果，不该被重写。
        assert_eq!(report["written"].as_array().unwrap().len(), 2);
        assert_eq!(report["unchanged"].as_array().unwrap().len(), 1);
        assert_eq!(report["unchanged"][0]["label"], "acc-a");
    }

    /// 写回格式与 App 一致：pretty、无尾换行（真实样本 33 字节）。
    #[test]
    fn written_content_matches_app_on_disk_format() {
        let dir = TempDir::new("format");
        dir.write_list("a", &["lean-adaptive-engineering"]);
        merge_and_distribute_at(dir.path(), &[pair("a"), pair("b")]);
        let content = dir.read("b").unwrap();
        assert_eq!(content, "[\n  \"lean-adaptive-engineering\"\n]");
        assert_eq!(content.len(), 33, "与 App 实际落盘大小一致");
        assert!(!content.ends_with('\n'), "不写尾换行");
    }

    /// 集合相同就不写：顺序、缩进、重复项不同都不算变化，也不产生备份。
    #[test]
    fn identical_set_is_not_rewritten() {
        let dir = TempDir::new("idempotent");
        dir.write_list("a", &["x", "y"]);
        // 同一集合，但顺序相反、缩进不同：字节不同，集合相同。
        let other = "[ \"y\",\r\n    \"x\" ]";
        dir.write_raw("b", other);
        let report = merge_and_distribute_at(dir.path(), &[pair("a"), pair("b")]);

        assert!(report["written"].as_array().unwrap().is_empty(), "{report}");
        assert_eq!(report["unchanged"].as_array().unwrap().len(), 2);
        assert_eq!(dir.read("b").unwrap(), other, "无变化时不得改写");
        assert!(dir.backups("b").is_empty(), "无变化时不该产生备份");
    }

    /// 覆盖前先备份，且备份内容就是被覆盖前的原文。
    #[test]
    fn existing_file_is_backed_up_before_overwrite() {
        let dir = TempDir::new("backup");
        dir.write_list("a", &["x"]);
        dir.write_list("b", &["y"]);
        let original = dir.read("b").unwrap();
        let report = merge_and_distribute_at(dir.path(), &[pair("a"), pair("b")]);

        let backups = dir.backups("b");
        assert_eq!(backups.len(), 1, "{backups:?}");
        assert_eq!(std::fs::read_to_string(&backups[0]).unwrap(), original);
        let written = report["written"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["label"] == "acc-b")
            .expect("b 应在写入列表里");
        assert_eq!(
            written["backup"].as_str().unwrap(),
            backups[0].to_string_lossy()
        );
        assert_eq!(written["hadContent"], true);
    }

    /// 只有重复项差异（集合相同）不算变化：不写、不备份，原文件保持原样。
    ///
    /// 「内容一致不写」比的是 dedup 后的集合，所以 `["x","x","y"]` 与 `{x,y}` 视为同一份；
    /// 真需要重排时（集合不同）写回的内容一律是去重后的并集，见上面两条用例。
    #[test]
    fn duplicates_alone_do_not_trigger_a_rewrite() {
        let dir = TempDir::new("dedupe");
        let original = "[\"x\",\"x\",\"y\"]";
        dir.write_raw("a", original);
        let report = merge_and_distribute_at(dir.path(), &[pair("a")]);

        assert_eq!(report["mergedExperts"], json!(["x", "y"]));
        assert!(report["written"].as_array().unwrap().is_empty(), "{report}");
        assert_eq!(report["unchanged"][0]["label"], "acc-a");
        assert_eq!(dir.read("a").unwrap(), original);
        assert!(dir.backups("a").is_empty());
    }

    /// 重复项不能让同集判定假阳性：B 的清单有重复项时，长度恰好等于并集长度。
    ///
    /// `["x","x"]` 与并集 `{x,y}` 长度都是 2，且两个 `"x"` 都在并集里——只比长度会漏判，
    /// 于是 B 永远拿不到 `y`，还会被报成「无需更新」。
    #[test]
    fn duplicate_entries_do_not_look_like_the_merged_set() {
        let dir = TempDir::new("dedupe-false-positive");
        dir.write_list("a", &["y"]);
        dir.write_raw("b", "[\"x\",\"x\"]");
        let report = merge_and_distribute_at(dir.path(), &[pair("a"), pair("b")]);

        assert_eq!(report["mergedExperts"], json!(["x", "y"]));
        assert_eq!(parsed(&dir.read("b").unwrap()), json!(["x", "y"]));
        assert_eq!(parsed(&dir.read("a").unwrap()), json!(["x", "y"]));
        assert_eq!(report["written"].as_array().unwrap().len(), 2, "{report}");
        assert!(report["unchanged"].as_array().unwrap().is_empty(), "{report}");
    }

    /// 缺 uid 的账号记错误、不猜目录，其余账号照常分发。
    #[test]
    fn missing_uid_is_reported_not_guessed() {
        let dir = TempDir::new("no-uid");
        dir.write_list("a", &["x"]);
        let report = merge_and_distribute_at(
            dir.path(),
            &[pair("a"), (String::new(), "无 uid 账号".to_string())],
        );
        assert_eq!(report["mergedExperts"], json!(["x"]));
        assert_eq!(report["errors"].as_array().unwrap().len(), 1);
        assert_eq!(report["errors"][0]["label"], "无 uid 账号");
        assert_eq!(
            report["errors"][0]["error"],
            "账号缺少 uid，无法定位专家目录"
        );
        assert!(dir.exists("a"));
    }

    /// 内容不是合法 JSON：该账号记错误，原文件一字不动，其他账号照常。
    #[test]
    fn invalid_json_is_reported_and_left_untouched() {
        let dir = TempDir::new("bad-json");
        dir.write_raw("a", "not-json");
        dir.write_list("b", &["y"]);
        let report = merge_and_distribute_at(dir.path(), &[pair("a"), pair("b")]);

        assert_eq!(report["mergedExperts"], json!(["y"]));
        assert_eq!(report["errors"].as_array().unwrap().len(), 1);
        assert_eq!(report["errors"][0]["label"], "acc-a");
        assert!(report["errors"][0]["error"]
            .as_str()
            .unwrap()
            .contains("不是合法 JSON"));
        assert_eq!(dir.read("a").unwrap(), "not-json", "不可信的文件不得被改写");
        assert!(dir.backups("a").is_empty());
        assert_eq!(report["unchanged"][0]["label"], "acc-b");
    }

    /// 顶层不是数组：同样记错误并保持原文件。
    #[test]
    fn non_array_json_is_reported_and_left_untouched() {
        let dir = TempDir::new("object-json");
        let original = "{\"experts\":[\"x\"]}";
        dir.write_raw("a", original);
        dir.write_list("b", &["y"]);
        let report = merge_and_distribute_at(dir.path(), &[pair("a"), pair("b")]);

        assert_eq!(report["mergedExperts"], json!(["y"]), "对象里的内容不得被采纳");
        assert_eq!(report["errors"][0]["label"], "acc-a");
        assert!(report["errors"][0]["error"]
            .as_str()
            .unwrap()
            .contains("顶层不是数组"));
        assert_eq!(dir.read("a").unwrap(), original);
    }

    /// 数组里混入非字符串元素：整份清单不可信（既不采纳也不写回）。
    ///
    /// 静默丢弃 `42` 再写回，等于替用户删掉了它——所以整份跳过。
    #[test]
    fn non_string_element_marks_whole_file_untrusted() {
        let dir = TempDir::new("mixed-array");
        let original = "[\"x\", 42]";
        dir.write_raw("a", original);
        dir.write_list("b", &["y"]);
        let report = merge_and_distribute_at(dir.path(), &[pair("a"), pair("b")]);

        assert_eq!(
            report["mergedExperts"],
            json!(["y"]),
            "不可信清单里的名字不得被采纳"
        );
        assert_eq!(report["errors"][0]["label"], "acc-a");
        assert!(report["errors"][0]["error"]
            .as_str()
            .unwrap()
            .contains("非字符串元素"));
        assert_eq!(dir.read("a").unwrap(), original, "原文件一字不动");
        assert!(dir.backups("a").is_empty());
    }

    /// 空数组是合法内容：不贡献任何名字，但仍要参与分发。
    #[test]
    fn empty_array_contributes_nothing_but_still_receives_copy() {
        let dir = TempDir::new("empty-array");
        dir.write_raw("a", "[]");
        dir.write_list("b", &["y"]);
        let report = merge_and_distribute_at(dir.path(), &[pair("a"), pair("b")]);

        assert_eq!(report["mergedExperts"], json!(["y"]));
        assert!(report["errors"].as_array().unwrap().is_empty(), "{report}");
        assert_eq!(parsed(&dir.read("a").unwrap()), json!(["y"]));
        let written = report["written"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["label"] == "acc-a")
            .expect("a 应从空清单变成合并结果");
        assert_eq!(written["hadContent"], true);
        assert_eq!(report["unchanged"][0]["label"], "acc-b");
    }

    /// 全员都没有内容：不写任何文件，返回说明。
    #[test]
    fn no_experts_anywhere_writes_nothing() {
        let dir = TempDir::new("blank");
        let report = merge_and_distribute_at(dir.path(), &[pair("a"), pair("b")]);

        assert_eq!(report["mergedExperts"], json!([]));
        assert_eq!(report["mergedCount"], 0);
        assert!(report["written"].as_array().unwrap().is_empty());
        assert!(report["unchanged"].as_array().unwrap().is_empty());
        assert!(report["note"].as_str().unwrap().contains("未做任何写入"));
        assert!(!dir.exists("a") && !dir.exists("b"));
    }

    /// 全员都只有空数组：同上（连空文件都不重排）。
    #[test]
    fn all_empty_arrays_write_nothing() {
        let dir = TempDir::new("all-empty");
        dir.write_raw("a", "[]");
        dir.write_raw("b", "[ ]");
        let report = merge_and_distribute_at(dir.path(), &[pair("a"), pair("b")]);

        assert!(report["note"].is_string(), "{report}");
        assert_eq!(dir.read("a").unwrap(), "[]");
        assert_eq!(dir.read("b").unwrap(), "[ ]");
    }

    /// 真实 `~/.workbuddy` 只读校验：并集必须与本机现状一致，且不写任何文件。
    ///
    /// 会读真实账号库与专家目录，因此默认 ignore；换机器后此处断言的具体专家团名字可能不同，
    /// 只在发布前手动跑一次核对。
    #[test]
    #[ignore = "读取真实 ~/.workbuddy 与账号库，只读不写；发布前手动运行"]
    fn real_home_union_matches_current_machine() {
        use crate::modules::account;

        let root = wb_data_root();
        let accounts = account::load_accounts();
        let pairs: Vec<(String, String)> = accounts
            .iter()
            .map(|acc| {
                (
                    acc.get("uid")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    account::account_display_name(acc),
                )
            })
            .collect();
        assert!(!pairs.is_empty(), "真实账号库读不到账号");

        let mut merged: BTreeSet<String> = BTreeSet::new();
        let mut with_file: Vec<String> = Vec::new();
        for (uid, label) in &pairs {
            assert!(!uid.trim().is_empty(), "{label} 缺 uid");
            let existing = read_experts_at(&root, uid)
                .unwrap_or_else(|error| panic!("{label} 的 {EXPERTS_FILE} 不可信：{error}"));
            match existing {
                Some(names) => {
                    with_file.push(label.clone());
                    merged.extend(names);
                }
                None => assert!(
                    !custom_dir_at(&root, uid).join(EXPERTS_FILE).exists(),
                    "{label} 报告无文件但文件存在"
                ),
            }
        }
        println!(
            "[experts] 账号 {} 个；有清单的账号 {:?}；并集 {:?}",
            pairs.len(),
            with_file,
            merged
        );
        assert!(
            !merged.is_empty(),
            "本机并集为空，说明这份只读校验没有真的读到清单"
        );
    }
}
