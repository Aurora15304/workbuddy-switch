//! 临时端到端验证：拿本机第一个账号真跑一遍「一键完成」核心链路。
//!
//! 用法：`cargo test -p wb-switch-core --lib growth_live -- --ignored --nocapture`
//!
//! 这些测试会真实调用上游（上报事件、发对话、领奖），默认不跑。

use crate::modules::account;
use crate::modules::growth_auto;
use crate::modules::growth_tasks;

/// 取本机第 N 个账号（跳过没有明文 token 的）。
fn account_at(index: usize) -> Option<serde_json::Value> {
    account::load_accounts()
        .into_iter()
        .filter(|acc| acc.get("access_token").and_then(serde_json::Value::as_str).is_some())
        .nth(index)
}

/// 取本机第一个账号（跳过没有明文 token 的）。
fn first_account() -> Option<serde_json::Value> {
    account_at(0)
}

#[tokio::test]
#[ignore = "真实调用上游"]
async fn growth_live_list_tasks() {
    let acc = first_account().expect("本机应有账号");
    let tasks = growth_tasks::list_tasks(&acc, false).await.expect("任务列表");
    println!("默认口径任务数：{}", tasks.len());
    for task in tasks.iter().take(30) {
        println!(
            "  {} | {} | {}/{} | accept={} | claimed={}",
            task.code, task.title, task.current, task.target, task.accept_status, task.claimed
        );
    }
}

#[tokio::test]
#[ignore = "真实调用上游"]
async fn growth_live_list_tasks_mp() {
    let acc = first_account().expect("本机应有账号");
    let tasks = growth_tasks::list_tasks(&acc, true).await.expect("mp 任务列表");
    println!("小程序口径任务数：{}", tasks.len());
    for task in tasks.iter().take(40) {
        println!(
            "  {} | {} | {}/{} | accept={} | claimed={}",
            task.code, task.title, task.current, task.target, task.accept_status, task.claimed
        );
    }
}

#[tokio::test]
#[ignore = "真实调用上游"]
async fn growth_live_report_one_event() {
    // 验证 report 通道能接受一条桌面指纹事件（空数组不计分，只看是否 200）。
    let acc = first_account().expect("本机应有账号");
    let events = growth_auto::desktop_chat_sequence(
        "11111111-2222-4333-8444-555555555555",
        "aaaaaaaabbbbbbbbccccccccdddddddd",
        "msg-live-test",
        "fast-model",
        "fast-model",
    );
    growth_auto::report_desktop_events(&acc, events)
        .await
        .expect("上报应成功");
    println!("桌面事件上报成功");
}

#[tokio::test]
#[ignore = "真实调用上游"]
async fn growth_live_chat_request_id() {
    // 验证真实对话能拿到服务端 requestId——专家类任务成败完全取决于此。
    let acc = first_account().expect("本机应有账号");
    let result = growth_tasks::chat_for_request_id_public(&acc, "").await;
    match result {
        Ok((conv, req)) => println!("对话成功：conv={conv} requestId={req}"),
        Err(error) => panic!("对话失败：{error}"),
    }
}

#[tokio::test]
#[ignore = "真实调用上游"]
async fn growth_live_market_experts() {
    let acc = first_account().expect("本机应有账号");
    let experts = growth_tasks::market_experts_public(&acc, "agent", None)
        .await
        .expect("专家市场");
    println!("专家数：{}", experts.len());
    for expert in experts.iter().take(5) {
        println!("  {} | {} | {}", expert.expert_id, expert.display_name, expert.profession);
    }
}

/// 指定序号的账号跑「一键完成」，前后对比。
///
/// 用环境变量 `GROWTH_LIVE_INDEX` 指定账号序号（默认 0）。
#[tokio::test]
#[ignore = "真实调用上游"]
async fn growth_live_run_account_at_index() {
    let index: usize = std::env::var("GROWTH_LIVE_INDEX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let acc = account_at(index).expect("账号应存在");
    println!("\n########## 账号序号 {index} ##########");

    let before = growth_tasks::list_tasks(&acc, false).await.expect("列表");
    println!("=== 执行前 ===");
    for task in before.iter() {
        println!(
            "  {} | {}/{} | claimed={}",
            task.code, task.current, task.target, task.claimed
        );
    }

    let result = growth_tasks::run_all(&acc).await;
    println!("=== 执行结果 ===");
    for item in result["results"].as_array().cloned().unwrap_or_default() {
        println!(
            "  [{}] {} | {} (+{})",
            item["status"].as_str().unwrap_or("?"),
            item["code"].as_str().unwrap_or("?"),
            item["message"].as_str().unwrap_or(""),
            item["credit"].as_i64().unwrap_or(0)
        );
    }
    println!("积分合计：{}", result["creditTotal"].as_i64().unwrap_or(0));

    let after = growth_tasks::list_tasks(&acc, false).await.expect("列表");
    println!("=== 执行后 ===");
    for task in after.iter() {
        println!(
            "  {} | {}/{} | claimed={}",
            task.code, task.current, task.target, task.claimed
        );
    }
}
