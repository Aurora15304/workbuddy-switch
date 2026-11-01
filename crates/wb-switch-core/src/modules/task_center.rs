//! 成长中心任务（beta）。
//!
//! 只做三件事：列出当前账号的成长任务、一键接受可接受的任务、一键领取已达标的奖励。
//! 全部走 WorkBuddy 官方成长域接口，不伪造任何上报数据——需要真实行为才能达成的任务
//! （对话、邀请等）只会显示进度，不会被本模块代劳。
//!
//! 两个口径：默认口径与小程序口径。小程序口径需附加 `X-Client-Platform: miniprogram`，
//! 且实测是默认口径的**超集**（多出 Sequential_Tasks_* 等专属任务），所以先各自拉取、
//! 再按任务编号合并去重。
//!
//! 领奖先走 chat 域端点，返回 400 时降级到 web 域端点重试——部分任务/租户形态只在
//! web 域可领。

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::modules::account::{build_auth_headers, envelope_token_error};
use crate::modules::config::http_request;
use crate::modules::credits::api_base_for;

const TASKS_PATH: &str = "/v2/activity/growth/tasks";
const TASKS_ACCEPT_PATH: &str = "/v2/activity/growth/tasks/accept";
const MP_PLATFORM: &str = "miniprogram";

/// 一条成长任务（只保留界面要用的字段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskItem {
    pub code: String,
    pub title: String,
    /// 达成条件或操作指引，二者取其一。
    pub hint: String,
    pub current: i64,
    pub target: i64,
    /// 上游是否真的下发了进度：global 口径完全不报进度，不能当成 0/0。
    pub progress_known: bool,
    pub credit: i64,
    pub energy: i64,
    pub accept_status: String,
    pub claimed: bool,
}

impl TaskItem {
    /// 进度已达标且尚未领取。
    pub fn claimable(&self) -> bool {
        !self.claimed && self.progress_known && self.target > 0 && self.current >= self.target
    }

    /// 尚可接受：上游没有「已领取」也没有「已接受」记录。
    pub fn acceptable(&self) -> bool {
        !self.claimed && self.accept_status.is_empty()
    }

    fn to_json(&self) -> Value {
        json!({
            "code": self.code,
            "title": self.title,
            "hint": self.hint,
            "current": self.current,
            "target": self.target,
            "progressKnown": self.progress_known,
            "credit": self.credit,
            "energy": self.energy,
            "acceptStatus": self.accept_status,
            "claimed": self.claimed,
            "claimable": self.claimable(),
        })
    }
}

/// 解析任务列表。宽松：字段缺失不报错，进度兼容 `progress: {current,target}` 与平铺两种形状。
fn parse_tasks(data: &Value) -> Vec<TaskItem> {
    let tasks = data
        .get("tasks")
        .and_then(Value::as_array)
        .or_else(|| {
            data.get("data")
                .and_then(|inner| inner.get("tasks"))
                .and_then(Value::as_array)
        })
        .cloned()
        .unwrap_or_default();

    tasks.iter().filter_map(parse_task).collect()
}

fn parse_task(task: &Value) -> Option<TaskItem> {
    // 默认口径用 task_code，global 口径只有 code。
    let code = task
        .get("task_code")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| task.get("code").and_then(Value::as_str))
        .filter(|s| !s.is_empty())?
        .to_string();

    let title = text_of(task, "title").unwrap_or_else(|| code.clone());
    let hint = text_of(task, "task_desc")
        .or_else(|| text_of(task, "description"))
        .unwrap_or_default();

    let (current, target, progress_known) = read_progress(task);
    let accept_status = text_of(task, "accept_status").unwrap_or_default();

    Some(TaskItem {
        code,
        title,
        hint,
        current,
        target,
        progress_known,
        credit: int_of(task, "reward_credit"),
        energy: int_of(task, "reward_energy"),
        claimed: accept_status == "claimed",
        accept_status,
    })
}

/// 进度读取：`progress` 对象优先，其次平铺的 current / target。
///
/// 判据是「两个键都在」而非「大于 0」——0 是合法进度值，用 >0 判断会把显式的 (0,0)
/// 误当成没给进度。
fn read_progress(task: &Value) -> (i64, i64, bool) {
    if let Some(progress) = task.get("progress") {
        let has_current = progress.get("current").is_some();
        let has_target = progress.get("target").is_some();
        if has_current && has_target {
            return (
                int_of(progress, "current"),
                int_of(progress, "target"),
                true,
            );
        }
    }
    if task.get("current").is_some() && task.get("target").is_some() {
        return (int_of(task, "current"), int_of(task, "target"), true);
    }
    (0, 0, false)
}

fn text_of(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 整数读取：兼容数字与数字字符串，缺失或非法一律 0。
fn int_of(value: &Value, key: &str) -> i64 {
    match value.get(key) {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// 合并两个口径的任务：同一编号只留一条，先出现的（默认口径）优先。
fn merge_tasks(primary: Vec<TaskItem>, extra: Vec<TaskItem>) -> Vec<TaskItem> {
    let mut seen: HashMap<String, ()> = HashMap::new();
    let mut merged = Vec::new();
    for item in primary.into_iter().chain(extra) {
        if seen.insert(item.code.clone(), ()).is_none() {
            merged.push(item);
        }
    }
    merged
}

/// 发一次成长域请求，返回信封解开后的 data。
async fn growth_json(
    account: &Value,
    method: &str,
    path: &str,
    body: Option<Value>,
    mp: bool,
) -> Result<Value, String> {
    let mut headers = build_auth_headers(account);
    headers.insert("X-Client-Platform".to_string(), "web".to_string());
    if mp {
        // 小程序口径：列表、accept、claim 全链路都必须带这个头，缺头时 accept 返回
        // task not found。
        headers.insert("X-Client-Platform".to_string(), MP_PLATFORM.to_string());
    }
    let url = format!("{}{}", api_base_for(account), path);
    let resp = http_request(&url, method, body, Some(&headers)).await;
    envelope_error(&resp)?;
    Ok(resp
        .get("data")
        .cloned()
        .unwrap_or_else(|| resp.clone()))
}

/// 上游把业务错误放在 `code`/`msg` 里，HTTP 200 也可能是失败。
fn envelope_error(resp: &Value) -> Result<(), String> {
    if let Some(code) = resp.get("code").and_then(Value::as_i64) {
        if code != 0 {
            let msg = resp
                .get("msg")
                .or_else(|| resp.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("上游返回错误");
            return Err(format!("{msg}（code {code}）"));
        }
    }
    if let Some(error) = resp.get("error").and_then(Value::as_str) {
        return Err(error.to_string());
    }
    Ok(())
}

/// 列出全部成长任务（两个口径合并去重）。
pub async fn list_tasks(account: &Value) -> Value {
    if let Some(error) = envelope_token_error(account) {
        return json!({ "tasks": [], "error": error });
    }
    let mut errors: Vec<String> = Vec::new();
    let primary = match growth_json(account, "GET", TASKS_PATH, None, false).await {
        Ok(data) => parse_tasks(&data),
        Err(error) => {
            // 默认口径都拿不到时没有可展示的内容，直接报错。
            return json!({ "tasks": [], "error": error });
        }
    };
    let extra = match growth_json(account, "GET", TASKS_PATH, None, true).await {
        Ok(data) => parse_tasks(&data),
        Err(error) => {
            // 小程序口径失败不影响主列表，只记录。
            errors.push(format!("小程序口径任务未取到：{error}"));
            Vec::new()
        }
    };
    let merged = merge_tasks(primary, extra);
    json!({
        "tasks": merged.iter().map(TaskItem::to_json).collect::<Vec<_>>(),
        "errors": errors,
        "claimable": merged.iter().filter(|t| t.claimable()).count(),
        "acceptable": merged.iter().filter(|t| t.acceptable()).count(),
    })
}

/// 未接受的任务才需要提交 accept。
///
/// 上游真实取值是 `not_accepted`；早期实现按「状态为空」判断，导致批量接受永远
/// 挑不到任务、直接返回「没有需要接受的任务」。空串保留为兼容。
fn needs_accept(status: &str) -> bool {
    status.is_empty() || status == "not_accepted"
}

/// 解析 accept 回包，返回（成功码，失败项）。
///
/// 上游对不存在的任务返回 `status: "error"` 但外层 `code: 0`、HTTP 200，
/// 只看信封会把失败当成功。回包里没有 `results` 时才按整批成功处理。
fn parse_accept_results(data: &Value, submitted: &[String]) -> (Vec<String>, Vec<Value>) {
    let mut accepted = Vec::new();
    let mut failed = Vec::new();
    match data.get("results").and_then(Value::as_array) {
        Some(items) => {
            for item in items {
                let code = item
                    .get("task_code")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let status = item
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if status == "accepted" || status == "already_accepted" {
                    accepted.push(code.to_string());
                } else {
                    let reason = item
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or(status);
                    failed.push(json!({ "code": code, "reason": reason }));
                }
            }
        }
        None => accepted.extend(submitted.iter().cloned()),
    }
    (accepted, failed)
}

/// 一键接受：两个口径分别提交，逐条核对结果。
///
/// 不能把合并后的任务码一次性发给小程序端点：web 独有的任务（实测 `playbook_prompt`）
/// 会被判 `task not found`，而外层信封仍是 `code: 0`、HTTP 200——只看信封就会误报
/// 「全部成功」。这里按口径各拉一次列表、各自接受，再按 `results[].status` 记账。
pub async fn accept_tasks(account: &Value) -> Value {
    if let Some(error) = envelope_token_error(account) {
        return json!({ "accepted": [], "failed": [], "error": error });
    }
    let mut accepted: Vec<String> = Vec::new();
    let mut failed: Vec<Value> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    for mp in [false, true] {
        let label = if mp { "小程序" } else { "默认" };
        let listing = match growth_json(account, "GET", TASKS_PATH, None, mp).await {
            Ok(data) => parse_tasks(&data),
            Err(error) => {
                notes.push(format!("{label}口径列表未取到：{error}"));
                continue;
            }
        };
        let codes: Vec<String> = listing
            .iter()
            .filter(|t| needs_accept(&t.accept_status) && !t.claimed)
            .map(|t| t.code.clone())
            .collect();
        if codes.is_empty() {
            continue;
        }
        match growth_json(
            account,
            "POST",
            TASKS_ACCEPT_PATH,
            Some(json!({ "task_codes": codes })),
            mp,
        )
        .await
        {
            Ok(data) => {
                let (ok, bad) = parse_accept_results(&data, &codes);
                accepted.extend(ok);
                failed.extend(bad);
            }
            Err(error) => notes.push(format!("{label}口径接受失败：{error}")),
        }
    }

    let mut out = json!({ "accepted": accepted, "failed": failed });
    if !notes.is_empty() {
        out["note"] = json!(notes.join("；"));
    } else if out["accepted"].as_array().map_or(true, |v| v.is_empty())
        && out["failed"].as_array().map_or(true, |v| v.is_empty())
    {
        out["note"] = json!("没有需要接受的任务");
    }
    out
}

/// 一键领取：逐个领可达标的奖励，单条失败不影响其余。
pub async fn claim_rewards(account: &Value) -> Value {
    if let Some(error) = envelope_token_error(account) {
        return json!({ "claimed": [], "skipped": [], "errors": [], "error": error });
    }
    let listing = list_tasks(account).await;
    if let Some(error) = listing.get("error").and_then(Value::as_str) {
        return json!({ "claimed": [], "skipped": [], "errors": [], "error": error });
    }
    let tasks = listing["tasks"].as_array().cloned().unwrap_or_default();
    let mut claimed: Vec<Value> = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();
    let mut errors: Vec<Value> = Vec::new();
    let mut credit_total: i64 = 0;

    for task in &tasks {
        let code = task["code"].as_str().unwrap_or_default().to_string();
        if !task["claimable"].as_bool().unwrap_or(false) {
            // 未达标或已领取：给出原因，不当成失败。
            let reason = if task["claimed"].as_bool().unwrap_or(false) {
                "已领取"
            } else if needs_accept(task["acceptStatus"].as_str().unwrap_or("")) {
                "未接受，先点「一键接受任务」"
            } else if !task["progressKnown"].as_bool().unwrap_or(false) {
                "上游暂无进度数据"
            } else {
                "需真实行为达标"
            };
            skipped.push(json!({
                "code": code,
                "title": task["title"],
                "reason": reason,
            }));
            continue;
        }
        match claim_one(account, &code).await {
            Ok(credit) => {
                credit_total += credit;
                claimed.push(json!({ "code": code, "title": task["title"], "credit": credit }));
            }
            Err(error) => errors.push(json!({ "code": code, "title": task["title"], "error": error })),
        }
    }

    json!({
        "claimed": claimed,
        "skipped": skipped,
        "errors": errors,
        "creditTotal": credit_total,
    })
}

/// 领一条奖励：chat 域失败且是 400 时降级到 web 域。
async fn claim_one(account: &Value, code: &str) -> Result<i64, String> {
    let path = format!(
        "{}/{}/claim",
        TASKS_PATH,
        percent_encode(code)
    );
    match growth_json(account, "POST", &path, None, true).await {
        Ok(data) => Ok(reward_credit(&data)),
        Err(error) => {
            // chat 域对该路径返回 400（部分任务/租户形态）→ web 域再试一次。
            if error.contains("400") {
                let retry = growth_json(account, "POST", &path, None, false).await?;
                return Ok(reward_credit(&retry));
            }
            Err(error)
        }
    }
}

/// 领奖回包的积分字段名不统一，逐个尝试。
fn reward_credit(data: &Value) -> i64 {
    for key in ["credit", "reward_credit", "gain_credit", "total_credit"] {
        if data.get(key).is_some() {
            return int_of(data, key);
        }
    }
    0
}

/// 路径片段转义：任务编号里可能出现 `_` 之外的空格或斜杠。
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(extra: Value) -> Value {
        json!(extra)
    }

    #[test]
    fn accept_results_report_per_task_errors() {
        // 实测：web 独有的任务用小程序口径提交会返回 status=error，但外层 code=0。
        let data = json!({ "results": [
            { "task_code": "chat_5", "status": "accepted" },
            { "task_code": "playbook_prompt", "status": "error", "message": "task not found" }
        ]});
        let (ok, bad) = parse_accept_results(&data, &[]);
        assert_eq!(ok, vec!["chat_5".to_string()]);
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0]["code"], "playbook_prompt");
        assert_eq!(bad[0]["reason"], "task not found");
    }

    #[test]
    fn accept_results_without_results_trust_submitted() {
        let submitted = vec!["a".to_string(), "b".to_string()];
        let (ok, bad) = parse_accept_results(&json!({}), &submitted);
        assert_eq!(ok, submitted);
        assert!(bad.is_empty());
    }

    #[test]
    fn accepts_not_accepted_status() {
        // 上游真实取值是 `not_accepted`。按「状态为空」判断会让批量接受永远空转。
        assert!(needs_accept("not_accepted"));
        assert!(needs_accept(""));
        assert!(!needs_accept("accepted"));
        assert!(!needs_accept("claimed"));
        assert!(!needs_accept("completed"));
    }

    #[test]
    fn parses_flat_progress_and_reward() {
        let data = json!({ "tasks": [{
            "task_code": "chat_5", "title": "和 AI 聊天 5 次",
            "task_desc": "完成 5 次对话", "current": 2, "target": 5,
            "reward_credit": 100, "reward_energy": 5, "accept_status": "accepted"
        }]});
        let tasks = parse_tasks(&data);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].code, "chat_5");
        assert_eq!(tasks[0].current, 2);
        assert_eq!(tasks[0].target, 5);
        assert!(tasks[0].progress_known);
        assert_eq!(tasks[0].credit, 100);
        assert!(!tasks[0].claimable());
    }

    #[test]
    fn progress_object_overrides_flat_fields() {
        let data = json!({ "tasks": [{
            "task_code": "daily", "title": "每日活跃",
            "current": 0, "target": 0,
            "progress": { "current": 3, "target": 3 }
        }]});
        let tasks = parse_tasks(&data);
        assert_eq!((tasks[0].current, tasks[0].target), (3, 3));
        assert!(tasks[0].claimable());
    }

    #[test]
    fn explicit_zero_progress_stays_known() {
        let data = json!({ "tasks": [{ "task_code": "t", "title": "x", "current": 0, "target": 0 }]});
        let tasks = parse_tasks(&data);
        // (0,0) 是合法进度，不能当成「上游没给进度」。
        assert!(tasks[0].progress_known);
        assert!(!tasks[0].claimable(), "目标为 0 时不可领取");
    }

    #[test]
    fn missing_progress_is_unknown_not_zero() {
        let data = json!({ "tasks": [{ "code": "global_task", "title": "国际版任务" }]});
        let tasks = parse_tasks(&data);
        assert_eq!(tasks[0].code, "global_task", "global 口径只有 code");
        assert!(!tasks[0].progress_known);
        assert!(!tasks[0].claimable());
    }

    #[test]
    fn merges_two_sources_by_code() {
        let primary = parse_tasks(&json!({ "tasks": [
            { "task_code": "a", "title": "A", "current": 1, "target": 1 },
            { "task_code": "b", "title": "B" }
        ]}));
        let extra = parse_tasks(&json!({ "tasks": [
            { "task_code": "b", "title": "B(小程序)" },
            { "task_code": "c", "title": "C 小程序专属" }
        ]}));
        let merged = merge_tasks(primary, extra);
        let codes: Vec<&str> = merged.iter().map(|t| t.code.as_str()).collect();
        assert_eq!(codes, vec!["a", "b", "c"]);
        assert_eq!(merged[1].title, "B", "同编号保留先出现的那条");
    }

    #[test]
    fn claimed_task_is_never_claimable_or_acceptable() {
        let data = json!({ "tasks": [{
            "task_code": "t", "title": "x", "current": 1, "target": 1,
            "accept_status": "claimed", "reward_credit": 50
        }]});
        let tasks = parse_tasks(&data);
        assert!(tasks[0].claimed);
        assert!(!tasks[0].claimable());
        assert!(!tasks[0].acceptable());
    }

    #[test]
    fn envelope_error_surfaces_upstream_message() {
        let resp = json!({ "code": 40001, "msg": "task not found" });
        let error = envelope_error(&resp).unwrap_err();
        assert!(error.contains("task not found"), "{error}");
    }

    #[test]
    fn ok_envelope_has_no_error() {
        assert!(envelope_error(&json!({ "code": 0, "data": {} })).is_ok());
    }

    #[test]
    fn claim_path_escapes_special_characters() {
        assert_eq!(percent_encode("RichMeow_Chat"), "RichMeow_Chat");
        assert_eq!(percent_encode("a/b c"), "a%2Fb%20c");
    }

    #[test]
    fn reward_credit_reads_first_known_field() {
        assert_eq!(reward_credit(&json!({ "credit": 88 })), 88);
        assert_eq!(reward_credit(&json!({ "reward_credit": "120" })), 120);
        assert_eq!(reward_credit(&json!({})), 0);
    }

    #[test]
    fn unused_task_helper_compiles() {
        // 保持 task() 辅助函数被使用，避免 dead_code 警告。
        assert_eq!(task(json!({"a":1}))["a"], 1);
    }
}
