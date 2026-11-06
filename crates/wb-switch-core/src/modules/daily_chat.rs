//! 签到后的「日常对话」：给每个账号每天真发一条消息，点亮活跃地图。
//!
//! 为什么需要它：服务端的月活跃地图要求「当天完成一次对话」才记录，光签到不算。
//! 多账号用户不可能逐个去聊天，所以签到跑完后由这里代劳 —— 每个账号一条短消息，
//! 内容从按时段分组的词库里随机取，不用同一句。
//!
//! 三条硬约束：
//! 1. **每号每天最多一条**：记录落 `daily_chat_logs.json`，重启后不会重复发、不重复烧额度；
//! 2. **并发发**：账号之间同时发，不让十个号排成一队；
//! 3. **失败不影响签到**：对话失败只记一行，签到结果照原样返回。

use std::path::PathBuf;

use chrono::{Local, Timelike};
use serde_json::{json, Value};

use crate::modules::config::{atomic_write, now_ms, store_dir, RunFlagGuard};

/// 并发发送的互斥：签到与定时轮次可能在同一进程里前后脚触发。
static CHAT_RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 日志保留天数。
const KEEP_DAYS: i64 = 30;

/// 早上（5:00–10:59）。
const MORNING: &[&str] = &[
    "早",
    "早上好",
    "早安",
    "滴，打卡",
    "滴～上班卡",
    "新的一天",
    "起床啦",
    "今天也早起",
];

/// 中午（11:00–13:59）。
const NOON: &[&str] = &["中午好", "午安", "吃了没", "午休时间", "打卡", "歇会儿", "午饭吃啥"];

/// 下午（14:00–17:59）。
const AFTERNOON: &[&str] = &[
    "下午好",
    "下午茶时间",
    "在吗",
    "hi",
    "hello",
    "来杯咖啡",
    "还有半天",
];

/// 晚上（18:00–22:59）。
const EVENING: &[&str] = &[
    "晚上好",
    "晚安",
    "今天也辛苦啦",
    "滴，下班卡",
    "收工",
    "晚饭吃啥",
];

/// 深夜（23:00–4:59）。
const LATE_NIGHT: &[&str] = &["夜深了", "还没睡呀", "晚安", "早点休息"];

/// 全天通用（不含时段暗示，任何时间点都不违和）。
const COMMON: &[&str] = &[
    "你好", "嗨", "报个到", "来都来了", "今天也在", "打卡", "在忙吗", "冒个泡", "随便聊聊",
];

/// 账号标识：优先稳定 id，缺失时退回展示名。
fn account_key(account: &Value) -> String {
    account
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(String::from)
        .or_else(|| {
            account
                .get("email")
                .and_then(Value::as_str)
                .map(String::from)
        })
        .unwrap_or_default()
}

fn local_hour(now: i64) -> u32 {
    chrono::DateTime::from_timestamp_millis(now)
        .map(|dt| dt.with_timezone(&Local).hour())
        .unwrap_or(12)
}

/// 某个小时对应的候选词库（时段词 + 通用词）。
fn pool_for(hour: u32) -> Vec<&'static str> {
    let bucket: &[&str] = match hour {
        5..=10 => MORNING,
        11..=13 => NOON,
        14..=17 => AFTERNOON,
        18..=22 => EVENING,
        _ => LATE_NIGHT,
    };
    bucket.iter().chain(COMMON.iter()).copied().collect()
}

/// FNV-1a：把账号 id 摊成一个稳定的散列，避免每个号都抽到同一句。
fn fnv1a(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 纳秒时间戳参与随机，保证同一账号每天换句子。
fn nanos_salt() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::from(elapsed.subsec_nanos()))
        .unwrap_or(0)
}

/// 取一句话：同一时段内按 `key` + 当天日期 + `salt` 决定，可复现（测试用固定 salt）。
pub fn phrase_for_key(key: &str, now: i64, salt: u64) -> &'static str {
    let pool = pool_for(local_hour(now));
    let day = now / 86_400_000;
    let seed = fnv1a(key) ^ (day as u64).rotate_left(24) ^ salt.rotate_left(7);
    pool[(seed % pool.len() as u64) as usize]
}

/// 账号维度的取词入口：salt 取当前纳秒，多账号同一时刻不会撞同一句。
pub fn phrase_for(account: &Value, now: i64) -> &'static str {
    phrase_for_key(&account_key(account), now, nanos_salt())
}

pub fn logs_file() -> PathBuf {
    store_dir().join("daily_chat_logs.json")
}

pub fn load_logs() -> Vec<Value> {
    std::fs::read_to_string(logs_file())
        .ok()
        .and_then(|text| serde_json::from_str::<Vec<Value>>(&text).ok())
        .unwrap_or_default()
}

/// 今天这个账号是否已经发过（只看成功记录）。
pub fn sent_today(rows: &[Value], key: &str, day: &str) -> bool {
    rows.iter().any(|row| {
        row.get("day").and_then(Value::as_str) == Some(day)
            && row.get("accountKey").and_then(Value::as_str) == Some(key)
            && row.get("ok").and_then(Value::as_bool) == Some(true)
    })
}

/// 只保留最近 [`KEEP_DAYS`] 天的记录（`YYYY-MM-DD` 字典序即时间序）。
fn prune_rows(rows: Vec<Value>, day: &str) -> Vec<Value> {
    let cutoff = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.checked_sub_signed(chrono::Duration::days(KEEP_DAYS)))
        .map(|date| date.format("%Y-%m-%d").to_string());
    let Some(cutoff) = cutoff else {
        return rows;
    };
    rows.into_iter()
        .filter(|row| {
            row.get("day")
                .and_then(Value::as_str)
                .is_some_and(|value| value >= cutoff.as_str())
        })
        .collect()
}

fn today_str() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

/// 给这批账号各发一条日常对话（每号每天最多一条）。
///
/// `accounts` 由调用方先过滤好（国内版、未被排除）。返回汇总：
/// `{"sent": n, "skipped": m, "failed": k, "results": [...]}`。
pub async fn ensure_sent_today(accounts: Vec<Value>) -> Value {
    let empty = json!({"sent": 0, "skipped": 0, "failed": 0, "results": []});
    if accounts.is_empty() {
        return empty;
    }
    let Some(_guard) = RunFlagGuard::try_acquire(&CHAT_RUNNING) else {
        return json!({"sent": 0, "skipped": 0, "failed": 0, "results": [], "reason": "already_running"});
    };

    let now = now_ms();
    let day = today_str();
    let rows = load_logs();

    let mut pending: Vec<(String, Value, &'static str)> = Vec::new();
    let mut skipped = 0usize;
    for account in accounts {
        let key = account_key(&account);
        if key.is_empty() || sent_today(&rows, &key, &day) {
            skipped += 1;
            continue;
        }
        let phrase = phrase_for(&account, now);
        pending.push((key, account, phrase));
    }

    if pending.is_empty() {
        return json!({"sent": 0, "skipped": skipped, "failed": 0, "results": []});
    }

    // 并发发：账号之间同时开始，谁也不等谁。
    //
    // 每个任务先过一遍 `ensure_fresh_token`：签到路径刷新的是它的副本，这里拿到的可能
    // 还是旧 token；不刷新的话，最需要补对话的那批账号反而会 401 失败。
    let cfg = crate::modules::config::load_checkin_config();
    let mut tasks = tokio::task::JoinSet::new();
    for (key, account, phrase) in pending {
        let email = crate::modules::account::account_display_name(&account);
        let cfg = cfg.clone();
        tasks.spawn(async move {
            let account = crate::modules::refresh::ensure_fresh_token(account, &cfg).await;
            let outcome =
                crate::modules::growth_tasks::chat_with_prompt(&account, phrase).await;
            (key, email, phrase, outcome)
        });
    }

    let mut results: Vec<Value> = Vec::new();
    let mut sent = 0usize;
    let mut failed = 0usize;
    let mut new_rows: Vec<Value> = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        let Ok((key, email, phrase, outcome)) = joined else {
            failed += 1;
            continue;
        };
        match outcome {
            Ok((conversation_id, _request_id)) => {
                sent += 1;
                results.push(json!({"email": email, "phrase": phrase, "ok": true}));
                new_rows.push(json!({
                    "ts": now_ms(),
                    "day": day,
                    "accountKey": key,
                    "email": email,
                    "phrase": phrase,
                    "ok": true,
                    "conversationId": conversation_id,
                }));
            }
            Err(error) => {
                failed += 1;
                results.push(json!({"email": email, "phrase": phrase, "ok": false, "error": error}));
                // 失败也落盘（`ok: false`）：`sent_today` 只看成功记录，下一轮会自动重试，
                // 但日志里能看到到底为什么没发出去。
                new_rows.push(json!({
                    "ts": now_ms(),
                    "day": day,
                    "accountKey": key,
                    "email": email,
                    "phrase": phrase,
                    "ok": false,
                    "error": error,
                }));
            }
        }
    }

    // 统一一次写入：并发任务各自写盘会互相覆盖。
    let mut merged = prune_rows(load_logs(), &day);
    merged.append(&mut new_rows);
    let _ = std::fs::create_dir_all(store_dir());
    let _ = atomic_write(
        &logs_file(),
        &serde_json::to_string_pretty(&merged).unwrap_or_default(),
    );

    json!({"sent": sent, "skipped": skipped, "failed": failed, "results": results})
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// 固定时间点：2026-11-06 09:30 / 12:30 / 15:30 / 20:30 / 02:00（本地时区无关的近似值，
    /// 只用来区分时段桶，不参与断言具体小时）。
    fn at(hour: u32) -> i64 {
        let date = chrono::NaiveDate::from_ymd_opt(2026, 11, 6).unwrap();
        let time = chrono::NaiveTime::from_hms_opt(hour, 30, 0).unwrap();
        Local
            .from_local_datetime(&date.and_time(time))
            .single()
            .unwrap()
            .timestamp_millis()
    }

    #[test]
    fn pools_are_time_aware_and_never_empty() {
        assert!(pool_for(8).contains(&"早上好"));
        assert!(pool_for(12).contains(&"中午好"));
        assert!(pool_for(15).contains(&"下午好"));
        assert!(pool_for(20).contains(&"晚上好"));
        assert!(pool_for(2).contains(&"夜深了"));
        for hour in 0..24 {
            assert!(!pool_for(hour).is_empty(), "{hour} 点的词库不该为空");
        }
    }

    #[test]
    fn phrase_always_comes_from_the_matching_bucket() {
        for hour in [8u32, 12, 15, 20, 2] {
            let pool = pool_for(hour);
            for salt in 0..40u64 {
                let phrase = phrase_for_key("acct", at(hour), salt);
                assert!(pool.contains(&phrase), "{hour} 点抽到越界词：{phrase}");
            }
        }
    }

    /// 词库不能退化成一句话：同一时段连续取 40 次，至少要出现 5 种不同的句子。
    #[test]
    fn library_is_not_monotonous() {
        let mut seen = std::collections::HashSet::new();
        for salt in 0..40u64 {
            seen.insert(phrase_for_key("acct", at(9), salt));
        }
        assert!(seen.len() >= 5, "词库过于单一，只出现 {} 种", seen.len());
    }

    /// 同一时刻不同账号不能全撞同一句（这是「多号同时发」最容易暴露的问题）。
    #[test]
    fn different_accounts_get_different_phrases() {
        let mut seen = std::collections::HashSet::new();
        for index in 0..12 {
            seen.insert(phrase_for_key(&format!("uid-{index}"), at(9), 7));
        }
        assert!(seen.len() >= 3, "不同账号只抽到 {} 种句子", seen.len());
    }

    #[test]
    fn sent_today_only_counts_successful_rows() {
        let rows = vec![
            json!({"day": "2026-11-06", "accountKey": "a", "ok": true}),
            json!({"day": "2026-11-06", "accountKey": "b", "ok": false}),
            json!({"day": "2026-11-05", "accountKey": "c", "ok": true}),
        ];
        assert!(sent_today(&rows, "a", "2026-11-06"));
        assert!(!sent_today(&rows, "b", "2026-11-06"), "失败记录不算今天的额度");
        assert!(!sent_today(&rows, "c", "2026-11-06"));
    }

    #[test]
    fn prune_keeps_recent_rows_only() {
        let rows = vec![
            json!({"day": "2026-11-06", "accountKey": "a"}),
            json!({"day": "2026-10-07", "accountKey": "b"}),
            json!({"day": "2026-09-01", "accountKey": "c"}),
            json!({"accountKey": "no-day"}),
        ];
        let kept = prune_rows(rows, "2026-11-06");
        let keys: Vec<&str> = kept
            .iter()
            .filter_map(|row| row.get("accountKey").and_then(Value::as_str))
            .collect();
        assert_eq!(keys, vec!["a", "b"], "30 天内的保留，超期的和缺日期的丢弃");
    }

    #[test]
    fn empty_input_short_circuits() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let value = runtime.block_on(ensure_sent_today(Vec::new()));
        assert_eq!(value["sent"], 0);
        assert_eq!(value["results"].as_array().map(Vec::len), Some(0));
    }
}
