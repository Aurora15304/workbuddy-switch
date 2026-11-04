//! Token 刷新与保活。
//!
//! 对照 server.py `refresh_account_token` / `ensure_fresh_token` /
//! `run_keepalive_cycle`。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;

use crate::modules::account::{build_auth_headers, upsert_account};
use crate::modules::config::{http_request, load_checkin_config, norm_ts, now_ms, RunFlagGuard};

static KEEPALIVE_RUNNING: AtomicBool = AtomicBool::new(false);

/// 账号库写入失败的可观测标记（只存在于返回对象，绝不落盘）。
///
/// 与 `refresh_error`（本地锁失败的瞬态语义）严格区分：写盘失败是另一类故障，
/// 复用 `refresh_error` 会让保活错误文案盖掉真正的 `needs_relogin_reason`，
/// 也会把「健康账号因本地锁没刷新」与「刷新成功但没保存」混成同一个信号。
const PERSIST_ERROR_KEY: &str = "persist_error";

// ---------------------------------------------------------------------------
// 刷新结果落盘：I/O 边界 + 失败可观测策略
// ---------------------------------------------------------------------------

/// 唯一写入接缝：落盘前剥掉瞬态诊断字段（绝不让它进账号库），写失败时记
/// error.log + stderr 做可观测上报，再把 Result 原样交给纯策略折叠。
///
/// 5 个调用点全部收敛到这里，避免「上报」逻辑散落复制。
fn persist_refresh_account(account: &mut Value) -> std::io::Result<()> {
    if let Some(map) = account.as_object_mut() {
        map.remove(PERSIST_ERROR_KEY);
    }
    let result = upsert_account(account);
    if let Err(error) = &result {
        let message = format!("刷新结果写入账号库失败（本次变更未持久化）: {error}");
        crate::modules::error_log::record("backend", "刷新结果落盘失败", &message);
        eprintln!("[refresh] {message}");
    }
    result
}

/// 纯策略（可测接缝）：把写盘结果折叠进返回对象。
///
/// - `Ok`：清除历史瞬态标记，原样返回（正常路径逐字段不变）；
/// - `Err`：打上 `persist_error` 让保活报告判 failed（不再假绿 `"ok"`），
///   并**保留**调用方已设的 `needs_relogin`，绝不因写失败把它抹掉。
///
/// 标记只在返回对象里，`persist_refresh_account` 已在写盘前把它剥掉，
/// 因此永远不会落盘污染下个周期的账号库。
fn fold_persist_outcome(mut account: Value, result: std::io::Result<()>) -> Value {
    let Some(map) = account.as_object_mut() else {
        return account;
    };
    match result {
        Ok(()) => {
            map.remove(PERSIST_ERROR_KEY);
        }
        Err(error) => {
            map.insert(
                PERSIST_ERROR_KEY.to_string(),
                json!(format!("账号库写入失败（本次刷新结果未持久化）: {error}")),
            );
        }
    }
    account
}

/// 纯策略（可测接缝）：从刷新返回对象生成保活报告。
///
/// 写盘失败（`persist_error`）与 `needs_relogin` / `refresh_error` 并列为失败信号，
/// 保证「刷新成功但没落盘」不再被报告成 `"ok"`。错误文案按根因优先级拼接，
/// 正常成功 / 失败 / 本地锁失败三条路径的输出与改造前逐字一致。
fn keepalive_outcome(fresh: &Value) -> (&'static str, Option<String>) {
    let failed = fresh.get("needs_relogin").and_then(Value::as_bool) == Some(true)
        || fresh.get("refresh_error").is_some()
        || fresh.get(PERSIST_ERROR_KEY).is_some();
    if !failed {
        return ("ok", None);
    }
    let reason = [
        fresh.get("refresh_error"),
        fresh.get("needs_relogin_reason"),
        fresh.get(PERSIST_ERROR_KEY),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .collect::<Vec<_>>()
    .join("; ");
    ("failed", (!reason.is_empty()).then_some(reason))
}

/// 刷新接口 URL：域名与路径前缀都按账号自身档位选（缺档位字段 → 国内版，零回归）。
fn refresh_url(account: &Value) -> String {
    let variant = crate::modules::account::variant_of(account);
    format!(
        "{}{}/auth/token/refresh",
        variant.api_endpoint(),
        variant.api_prefix()
    )
}

/// 构造刷新请求头：鉴权头 + 刷新会话头。
///
/// `X-Auth-Refresh-Source: plugin` 必须保留：两个档位的官方客户端刷新时都发送该头，
/// 缺失时网关会把这次刷新判定成另一个 client 来源，国际版实测返回
/// `invalid_grant: Invalid refresh token`（code 12153）。抽成纯函数是为了让单测
/// 能把这个头固定住，避免将来被顺手删掉。
fn refresh_headers(account: &Value, refresh_token: &str) -> HashMap<String, String> {
    let mut headers = build_auth_headers(account);
    headers.insert("X-Refresh-Token".to_string(), refresh_token.to_string());
    headers.insert("X-Auth-Refresh-Source".to_string(), "plugin".to_string());
    headers
}

/// 刷新单账号 token（POST /v2/plugin/auth/token/refresh），成功则落盘并返回新账号。
///
/// 刷新失败（refresh token 失效等）时给账号标记 needs_relogin，避免无限重试。
pub async fn refresh_account_token(mut account: Value) -> Value {
    // Cover the request and its persistence with the same cross-process lock as
    // switch_account's snapshot/export, including all early-return branches.
    let _guard =
        match crate::modules::auth_transaction::wait_auth(std::time::Duration::from_secs(30)).await
        {
            Ok(guard) => guard,
            Err(error) => {
                // A local lock failure must not persist needs_relogin on a healthy account.
                account["refresh_error"] = json!(error.to_string());
                return account;
            }
        };
    // The caller may have queued an older snapshot while another refresh ran.
    let id = crate::modules::account::get_str(&account, "id")
        .or_else(|| crate::modules::account::get_str(&account, "uid"));
    if let Some(latest) = id
        .as_deref()
        .and_then(crate::modules::account::find_account)
    {
        account = latest;
    }
    let previous_access_token = account
        .get("access_token")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let rt = account
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_default();
    if rt.is_empty() {
        account["needs_relogin"] = json!(true);
        account["needs_relogin_reason"] = json!("缺少 refresh token，无法刷新，需重新登录");
        let result = persist_refresh_account(&mut account);
        return fold_persist_outcome(account, result);
    }

    let headers = refresh_headers(&account, &rt);
    // 刷新必须走账号自身档位的域名：把国际版 token 打到国内网关等于登录失效。
    let url = refresh_url(&account);
    let resp = http_request(&url, "POST", Some(json!({})), Some(&headers)).await;
    let code = resp.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code != 0 && code != 200 {
        account["needs_relogin"] = json!(true);
        account["needs_relogin_reason"] = json!(format!(
            "刷新失败(code={code}): {}",
            resp.get("message")
                .or_else(|| resp.get("msg"))
                .and_then(|v| v.as_str())
                .unwrap_or("未知错误")
        ));
        let result = persist_refresh_account(&mut account);
        return fold_persist_outcome(account, result);
    }

    let data = resp.get("data").cloned().unwrap_or_else(|| json!({}));
    let new_at = data
        .get("accessToken")
        .and_then(|v| v.as_str())
        .or_else(|| data.get("access_token").and_then(|v| v.as_str()))
        .map(|s| s.to_string());
    let Some(new_at) = new_at else {
        account["needs_relogin"] = json!(true);
        account["needs_relogin_reason"] = json!("刷新响应缺少 accessToken");
        let result = persist_refresh_account(&mut account);
        return fold_persist_outcome(account, result);
    };

    account["access_token"] = json!(new_at);
    if let Some(new_rt) = data
        .get("refreshToken")
        .and_then(|v| v.as_str())
        .or_else(|| data.get("refresh_token").and_then(|v| v.as_str()))
    {
        account["refresh_token"] = json!(new_rt);
    }
    // 官方接口只返回相对 expiresIn（秒），需换算为绝对时间戳
    let new_exp = norm_ts(data.get("expiresAt").or_else(|| data.get("expires_at")));
    let new_exp = match new_exp {
        Some(v) => Some(v),
        None => data
            .get("expiresIn")
            .and_then(|v| v.as_i64())
            .map(|e| now_ms() + e * 1000),
    };
    if let Some(v) = new_exp {
        account["expiresAt"] = json!(v);
    }
    let fallback_rt_exp = norm_ts(
        account
            .get("auth_raw")
            .and_then(|a| a.get("refreshExpiresAt")),
    );
    let mut new_rt_exp = norm_ts(
        data.get("refreshExpiresAt")
            .or_else(|| data.get("refresh_expires_at")),
    );
    if new_rt_exp.is_none() {
        new_rt_exp = fallback_rt_exp;
    }
    let new_rt_exp = match new_rt_exp {
        Some(v) => Some(v),
        None => data
            .get("refreshExpiresIn")
            .and_then(|v| v.as_i64())
            .map(|e| now_ms() + e * 1000),
    };
    if let Some(v) = new_rt_exp {
        account["refreshExpiresAt"] = json!(v);
    }
    account["refreshedAt"] = json!(now_ms());
    let map = account.as_object_mut().unwrap();
    map.remove("needs_relogin");
    map.remove("needs_relogin_reason");
    let result = persist_refresh_account(&mut account);
    account = fold_persist_outcome(account, result);
    // Windows 不执行 apiKeyHelper；当前 CLI 账号刷新后同步 settings env。
    // 同步失败不阻断 WorkBuddy 保活；状态接口会根据 settings 与账号库是否
    // 一致显示“待同步”，避免把认证配置错误混入账号数据。
    if cfg!(windows) {
        let _ = crate::modules::codebuddy_cli::sync_windows_env_for_account(
            &account,
            previous_access_token.as_deref(),
        );
    }
    account
}

/// 惰性刷新：expiresAt 缺失或剩余 < lazy_refresh_hours 则刷新。返回最新账号。
pub async fn ensure_fresh_token(mut account: Value, cfg: &Value) -> Value {
    let lazy_h = cfg
        .get("lazy_refresh_hours")
        .and_then(|v| v.as_i64())
        .unwrap_or(24);
    let exp = account.get("expiresAt").and_then(|v| v.as_i64());
    let stale = match exp {
        Some(e) => now_ms() >= e || e - now_ms() < lazy_h * 3600 * 1000,
        None => true,
    };
    let has_rt = !account
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .is_empty();
    if stale && has_rt {
        account = refresh_account_token(account).await;
    }
    account
}

/// 保活检查：每天由后台循环调用一次，默认（keepalive_days <= 0）无条件刷新
/// 全部带 refresh token 的账号；keepalive_days > 0 时仅刷新剩余不足该天数的账号。
///
/// 高频保活是为了避免官方服务端清理闲置的 refresh 会话——曾出现闲置数天后
/// 刷新返回 12153 invalid_grant（Session doesn't have required client）导致
/// 账号被迫重新登录。
pub async fn run_keepalive_cycle() -> Value {
    let Some(_guard) = RunFlagGuard::try_acquire(&KEEPALIVE_RUNNING) else {
        return json!({"skipped": "already_running"});
    };
    let cfg = load_checkin_config();
    let keep_days = cfg
        .get("keepalive_days")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let accounts = crate::modules::account::load_accounts();
    let total = accounts.len();
    let mut results: Vec<Value> = Vec::new();
    for mut acc in accounts {
        let exp = acc.get("expiresAt").and_then(|v| v.as_i64());
        let stale = keep_days <= 0
            || match exp {
                Some(e) => now_ms() >= e || e - now_ms() < keep_days * 24 * 3600 * 1000,
                None => true,
            };
        if !stale {
            continue;
        }
        if acc
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .is_empty()
        {
            acc["needs_relogin"] = json!(true);
            acc["needs_relogin_reason"] = json!("缺少 refresh token，无法保活，需重新登录");
            let result = persist_refresh_account(&mut acc);
            let acc = fold_persist_outcome(acc, result);
            let mut entry = json!({
                "email": crate::modules::account::account_display_name(&acc),
                "status": "missing_rt",
            });
            // 写盘失败时把可观测错误带进报告，绝不静默丢掉 needs_relogin 的落盘结果。
            if let Some(error) = acc.get(PERSIST_ERROR_KEY).and_then(Value::as_str) {
                entry["error"] = json!(error);
            }
            results.push(entry);
            continue;
        }
        let fresh = refresh_account_token(acc).await;
        let (status, error) = keepalive_outcome(&fresh);
        results.push(json!({
            "email": crate::modules::account::account_display_name(&fresh),
            "status": status,
            "error": error,
        }));
    }
    json!({"checked": total, "refreshed": results})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_url_follows_account_variant() {
        // 旧账号（无档位字段）→ 国内版，与改造前逐字一致。
        assert_eq!(
            refresh_url(&json!({"uid": "u-1"})),
            format!(
                "{}/v2/plugin/auth/token/refresh",
                crate::modules::variant::WbVariant::Cn.api_endpoint()
            )
        );
        assert_eq!(
            refresh_url(&json!({"uid": "u-1", "access_token": "t"})),
            "https://www.codebuddy.cn/v2/plugin/auth/token/refresh"
        );
        // 国际版账号 → 国际版域名。
        assert_eq!(
            refresh_url(&json!({"uid": "u-2", "variant": "ai"})),
            format!(
                "{}/v2/plugin/auth/token/refresh",
                crate::modules::variant::WbVariant::Ai.api_endpoint()
            )
        );
        assert_ne!(
            refresh_url(&json!({"variant": "ai"})),
            refresh_url(&json!({"variant": "cn"}))
        );
    }

    /// AC11：刷新请求头必须带 `X-Auth-Refresh-Source: plugin`，两个档位都覆盖。
    ///
    /// 断言为 `Some("plugin")`：头被删掉时为 `None`，用例立即失败，不会被写成恒真。
    #[test]
    fn refresh_headers_always_declare_plugin_refresh_source() {
        let cases = [
            // 国内版：旧账号（无档位字段）与显式档位各一例。
            json!({"uid": "u-cn", "access_token": "at", "refresh_token": "rt"}),
            json!({"uid": "u-cn", "access_token": "at", "variant": "cn"}),
            // 国际版：D1 的失败现场。
            json!({"uid": "u-ai", "access_token": "at", "variant": "ai"}),
        ];
        for account in cases {
            let headers = refresh_headers(&account, "rt-value");
            assert_eq!(
                headers.get("X-Auth-Refresh-Source").map(String::as_str),
                Some("plugin"),
                "刷新请求头缺少 X-Auth-Refresh-Source: plugin（{account}）"
            );
            // 刷新会话头不能被这次改动挤掉。
            assert_eq!(
                headers.get("X-Refresh-Token").map(String::as_str),
                Some("rt-value")
            );
            // 刷新不带 platform 查询参数/头：官方刷新同样不带（research §2 已推翻旧假设）。
            assert!(!headers.contains_key("platform"));
        }
    }

    // -----------------------------------------------------------------------
    // T7 写盘失败治理（纯策略接缝：注入 io::Result，绝不触碰真实账号库）
    // -----------------------------------------------------------------------

    /// 模拟磁盘满 / 只读目录等写盘失败。
    fn disk_full() -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::Other, "模拟磁盘已满 / no space left")
    }

    /// 验收：成功路径写盘失败 → 报告不得为 ok，且不得把健康账号误标 needs_relogin。
    #[test]
    fn successful_refresh_with_failed_write_is_never_reported_as_ok() {
        let account = json!({
            "id": "acc-1",
            "access_token": "fresh-at",
            "refresh_token": "fresh-rt",
        });
        let written = fold_persist_outcome(account, Err(disk_full()));
        assert!(
            written
                .get(PERSIST_ERROR_KEY)
                .and_then(Value::as_str)
                .is_some(),
            "写失败必须留下可观测标记"
        );
        assert!(
            written.get("needs_relogin").is_none(),
            "写盘失败不得把健康账号标记成需重新登录"
        );
        let (status, error) = keepalive_outcome(&written);
        assert_eq!(status, "failed", "写盘失败绝不能假报 ok");
        assert!(error.unwrap().contains("磁盘已满"), "错误文案必须带根因");
    }

    /// 验收：失败路径写盘失败 → needs_relogin 不得静默丢失，写失败也要可观测。
    #[test]
    fn failed_refresh_with_failed_write_keeps_needs_relogin_observable() {
        let account = json!({
            "id": "acc-1",
            "needs_relogin": true,
            "needs_relogin_reason": "刷新失败(code=12153)",
        });
        let written = fold_persist_outcome(account, Err(disk_full()));
        assert_eq!(
            written["needs_relogin"], true,
            "needs_relogin 不得被静默抹掉"
        );
        assert!(written.get(PERSIST_ERROR_KEY).is_some(), "写失败必须可观测");

        let (status, error) = keepalive_outcome(&written);
        assert_eq!(status, "failed");
        let error = error.unwrap();
        assert!(
            error.contains("刷新失败(code=12153)"),
            "根因不能被写盘错误盖掉: {error}"
        );
        assert!(error.contains("磁盘已满"), "写盘失败本身也要上报: {error}");
    }

    /// 不变式：正常成功 / 失败 / 本地锁失败三条路径输出与改造前逐字一致。
    #[test]
    fn keepalive_outcome_is_unchanged_on_normal_paths() {
        let (status, error) = keepalive_outcome(&json!({"id": "a", "access_token": "t"}));
        assert_eq!(status, "ok");
        assert!(error.is_none());

        let (status, error) = keepalive_outcome(&json!({
            "needs_relogin": true,
            "needs_relogin_reason": "刷新失败(code=12153)",
        }));
        assert_eq!(status, "failed");
        assert_eq!(error.as_deref(), Some("刷新失败(code=12153)"));

        let (status, error) = keepalive_outcome(&json!({"refresh_error": "auth lock timeout"}));
        assert_eq!(status, "failed");
        assert_eq!(error.as_deref(), Some("auth lock timeout"));
    }

    /// 写成功时清除瞬态标记：诊断字段绝不残留、绝不落盘污染下个周期的判定。
    #[test]
    fn successful_write_clears_stale_persist_marker() {
        let account = json!({
            "id": "a",
            "access_token": "t",
            "persist_error": "上一轮的瞬态诊断",
        });
        let written = fold_persist_outcome(account, Ok(()));
        assert!(written.get(PERSIST_ERROR_KEY).is_none());
        assert_eq!(written["access_token"], "t");
        assert_eq!(keepalive_outcome(&written).0, "ok");
    }

    /// 非对象 Value 折叠不 panic（定义错误出局）。
    #[test]
    fn persist_outcome_handles_non_object_values() {
        let value = json!("not an object");
        assert_eq!(fold_persist_outcome(value.clone(), Err(disk_full())), value);
    }
}
