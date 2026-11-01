//! 成长任务自动完成：客户端行为指纹与事件上报。
//!
//! 成长任务中心的服务端按「行为事件」计分：客户端做了某件事（打开资料库、
//! 进入 Buddy 应用、召唤专家……），就把对应埋点事件报到 `POST /v2/report`，
//! 服务端据此点亮任务进度；进度达标后再调领奖端点把奖励收下。
//!
//! 这里复刻官方三类客户端指纹，把事件按真机形态发出去：
//!
//! * **桌面指纹**（`extName=workbuddy-desktop`）：PC 侧事件族，对话链、模板、
//!   灵感、专家召唤、定时任务创建、外观皮肤等走这条。
//! * **网页指纹**：资料库阅读等 web 域埋点。
//! * **小程序指纹**（`extName=workbuddy-mp`）：小程序口径的对话与专家事件。
//!
//! 三处最容易踩空、照抄真机抓包纠过的细节：
//!
//! 1. `/v2/report` **不带** `x-client-platform`、`Origin`、`Referer`、`sec-ch-*`。
//!    官方 report 是纯 Node 形态的 11 个头，补上 Client Hints 反而破功。
//!    但 `/console/account` 与任务领奖端点**确实**带 `x-client-platform: web`。
//! 2. `X-Domain` 是**裸主机名**（`www.workbuddy.cn`），不是带 scheme 的 URL。
//! 3. 桌面指纹的版本三元组（version / commit / releaseDate）按 realm 分开，
//!    且 commit / releaseDate 是打包期常量、无法从版本号推导；国际版不带
//!    `qimei36`（真机 CN 28 键 vs 国际 27 键，差集恰好只有它）。

use std::collections::HashMap;

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::modules::account::build_auth_headers;
use crate::modules::config::http_request;

/// 桌面端上报通道路径。
const REPORT_PATH: &str = "/v2/report";

/// 国内版桌面端三元组（5.9.3 官方分发包实测）。
const DESKTOP_VERSION_CN: &str = "5.9.3";
const DESKTOP_COMMIT_CN: &str = "d46924219e79332f49c37ccebf27292f87001b19";
const DESKTOP_RELEASE_CN: i64 = 1_793_093_056_315;

/// 国际版桌面端三元组（5.9.4 官方分发包实测）。
const DESKTOP_VERSION_GLOBAL: &str = "5.9.4";
const DESKTOP_COMMIT_GLOBAL: &str = "fa0a0901906addaa1a4259f6df347388a5563ded";
const DESKTOP_RELEASE_GLOBAL: i64 = 1_793_288_804_029;

/// 官方 CLI 内核版本（UA 第三段）。
const CLI_VERSION: &str = "2.165.0";

/// 小程序端版本。
const MP_VERSION: &str = "2.4.0";

/// 账号是否国际版（`variant=ai`）。
pub fn is_global(account: &Value) -> bool {
    account
        .get("variant")
        .and_then(Value::as_str)
        .map(|v| v.eq_ignore_ascii_case("ai"))
        .unwrap_or(false)
}

/// 上报通道的裸主机名。国内版走 `www.workbuddy.cn`，国际版走 `www.workbuddy.ai`。
///
/// 真机实测 `X-Domain` 恒为裸主机名——发成带 scheme 的 URL 是最容易被上游
/// 按「非客户端形态」识别出来的一处。
pub fn report_host(account: &Value) -> &'static str {
    if is_global(account) {
        "www.workbuddy.ai"
    } else {
        "www.workbuddy.cn"
    }
}

/// 上报通道的完整 base URL。
fn report_base(account: &Value) -> String {
    format!("https://{}", report_host(account))
}

/// 由 uid 稳定派生 36 位十六进制设备标识。
///
/// 模拟固定设备：同一账号每次派生结果相同，跨重启恒定。
fn derive_id(uid: &str, salt: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(b":");
    hasher.update(uid.as_bytes());
    let digest = hasher.finalize();
    digest[..18].iter().map(|b| format!("{b:02x}")).collect()
}

/// 桌面端 UA：两段品牌一致（官方 SDK 的 buildUserAgent 形态）。
///
/// 国内版 `WorkBuddy/5.9.3 WorkBuddy/5.9.3 CLI/2.165.0`；
/// 国际版 `workbuddy-ai/5.9.4 workbuddy-ai/5.9.4 CLI/2.165.0`。
pub fn desktop_ua(account: &Value) -> String {
    if is_global(account) {
        format!("workbuddy-ai/{DESKTOP_VERSION_GLOBAL} workbuddy-ai/{DESKTOP_VERSION_GLOBAL} CLI/{CLI_VERSION}")
    } else {
        format!("WorkBuddy/{DESKTOP_VERSION_CN} WorkBuddy/{DESKTOP_VERSION_CN} CLI/{CLI_VERSION}")
    }
}

/// 账号昵称（缺失时退回空串）。
fn nickname(account: &Value) -> String {
    account
        .get("nickname")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// 桌面端公共指纹字段：设备 + 包信息。
///
/// 缺 `cpuModel` 是最明显的「非桌面端」特征，故设备字段按真机报名补齐。
fn desktop_fingerprint(account: &Value) -> Map<String, Value> {
    let uid = account
        .get("uid")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let now = now_ms();
    let (version, commit, release) = if is_global(account) {
        (
            DESKTOP_VERSION_GLOBAL,
            DESKTOP_COMMIT_GLOBAL,
            DESKTOP_RELEASE_GLOBAL,
        )
    } else {
        (DESKTOP_VERSION_CN, DESKTOP_COMMIT_CN, DESKTOP_RELEASE_CN)
    };

    let mut fp = Map::new();
    fp.insert("timezone".into(), json!("Asia/Shanghai"));
    fp.insert("reportDelay".into(), json!(2000));
    fp.insert("userId".into(), json!(uid));
    fp.insert("username".into(), json!(nickname(account)));
    fp.insert("userNickname".into(), json!(nickname(account)));
    fp.insert("product".into(), json!("SaaS"));
    fp.insert("releaseDate".into(), json!(release));
    fp.insert("commit".into(), json!(commit));
    fp.insert("ideName".into(), json!("WorkBuddy"));
    fp.insert("ideType".into(), json!("WorkBuddy"));
    fp.insert("ideVersion".into(), json!(version));
    fp.insert("machineId".into(), json!(derive_id(&uid, "machine")));
    fp.insert("sessionId".into(), json!(derive_id(&uid, "session")));
    fp.insert("extName".into(), json!("workbuddy-desktop"));
    fp.insert("extVersion".into(), json!(version));
    fp.insert("os".into(), json!("win32"));
    fp.insert("arch".into(), json!("x64"));
    fp.insert("osVersion".into(), json!("10.0.28000"));
    fp.insert(
        "cpuModel".into(),
        json!("11th Gen Intel(R) Core(TM) i5-11320H @ 3.20GHz"),
    );
    fp.insert("cpuCores".into(), json!(8));
    fp.insert("memorySize".into(), json!(16));
    fp.insert("timestamp".into(), json!(now));
    fp.insert("presentAt".into(), json!(now));
    // qimei36：腾讯设备标识。国内版每个 report 事件都带，国际版一条都不带
    // （真机键数差集恰好只有它）——给国际版补这个键反而是伪造。
    if !is_global(account) {
        fp.insert("qimei36".into(), json!(derive_id(&uid, "qimei")));
    }
    fp
}

/// 小程序端公共指纹。
fn mp_fingerprint(account: &Value) -> Map<String, Value> {
    let mut fp = Map::new();
    fp.insert("timestamp".into(), json!(now_ms()));
    fp.insert("ideType".into(), json!("WorkBuddy_MP"));
    fp.insert("ideVersion".into(), json!(MP_VERSION));
    fp.insert("extName".into(), json!("workbuddy-mp"));
    fp.insert("extVersion".into(), json!(MP_VERSION));
    fp.insert("product".into(), json!("SaaS"));
    fp.insert("ideName".into(), json!("wx_app_cloud"));
    fp.insert("platform".into(), json!("mini_program"));
    fp.insert("os".into(), json!("windows"));
    fp.insert("osVersion".into(), json!("11"));
    fp.insert("arch".into(), json!("x64"));
    fp.insert(
        "machineId".into(),
        json!("0655736a-607f-4d9d-b430-58176ee9a090"),
    );
    fp.insert("timezone".into(), json!("Asia/Shanghai"));
    fp.insert(
        "userId".into(),
        json!(account
            .get("uid")
            .and_then(Value::as_str)
            .unwrap_or_default()),
    );
    fp.insert("userNickname".into(), json!(nickname(account)));
    fp
}

/// 当前毫秒时间戳。
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 新会话 ID（v4 UUID 形态）。
pub fn new_conversation_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 新消息 / 请求 ID（32 位十六进制）。
pub fn new_message_id() -> String {
    let mut hasher = Sha256::new();
    hasher.update(uuid::Uuid::new_v4().as_bytes());
    hasher.update(b":msg");
    let digest = hasher.finalize();
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// report 通道请求头：逐项照抄真机夹具，顺序与取值都不自己发挥。
fn report_headers(account: &Value, ua: &str) -> HashMap<String, String> {
    let mut headers = build_auth_headers(account);
    headers.insert(
        "Accept".to_string(),
        "application/json, text/plain, */*".to_string(),
    );
    headers.insert(
        "Content-Type".to_string(),
        "application/json;charset=UTF-8".to_string(),
    );
    headers.insert("X-Product".to_string(), "SaaS".to_string());
    headers.insert("User-Agent".to_string(), ua.to_string());
    headers.insert("X-Request-ID".to_string(), new_message_id());
    headers.insert("X-Domain".to_string(), report_host(account).to_string());
    // 真机 /v2/report 不带 X-Device-Token（只有 billing 族带），故不注入。
    headers.remove("X-Enterprise-Id");
    headers.remove("X-Tenant-Id");
    headers
}

/// 上报一批事件到 `POST /v2/report`。
///
/// `fingerprint` 是公共指纹，`events` 是业务载荷（业务字段优先，可覆盖同名字段）。
async fn report_events(
    account: &Value,
    fingerprint: Map<String, Value>,
    ua: &str,
    events: Vec<Map<String, Value>>,
) -> Result<(), String> {
    if events.is_empty() {
        return Err("没有可上报的事件".to_string());
    }
    let mut array = Vec::with_capacity(events.len());
    for event in events {
        let mut merged = fingerprint.clone();
        for (key, value) in event {
            merged.insert(key, value);
        }
        array.push(Value::Object(merged));
    }
    let url = format!("{}{}", report_base(account), REPORT_PATH);
    let headers = report_headers(account, ua);
    let resp = http_request(&url, "POST", Some(Value::Array(array)), Some(&headers)).await;
    if let Some(code) = resp.get("code").and_then(Value::as_i64) {
        if code != 0 {
            let msg = resp
                .get("msg")
                .and_then(Value::as_str)
                .unwrap_or("上报被上游拒绝");
            return Err(format!("{msg}（code {code}）"));
        }
    }
    Ok(())
}

/// 以桌面端指纹上报一批事件。
pub async fn report_desktop_events(
    account: &Value,
    events: Vec<Map<String, Value>>,
) -> Result<(), String> {
    report_events(
        account,
        desktop_fingerprint(account),
        &desktop_ua(account),
        events,
    )
    .await
}

/// 以网页端指纹上报一个 web 埋点事件。
///
/// 资料库阅读这类判据只认 web 域的 `web_element_click`。
pub async fn report_web_event(
    account: &Value,
    event_code: &str,
    page_url: &str,
    element_id: &str,
    element_name: &str,
) -> Result<(), String> {
    let uid = account
        .get("uid")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut event = Map::new();
    event.insert("eventCode".into(), json!(event_code));
    event.insert("timestamp".into(), json!(now_ms()));
    event.insert("reportDelay".into(), json!(0));
    event.insert("pageURL".into(), json!(page_url));
    event.insert("elementId".into(), json!(element_id));
    event.insert("elementName".into(), json!(element_name));
    event.insert("os".into(), json!("Win32"));
    event.insert("arch".into(), json!(""));
    event.insert("osVersion".into(), json!("10.0"));
    event.insert("userAgent".into(), json!(desktop_ua(account)));
    event.insert(
        "machineId".into(),
        json!(derive_id(&uid, "webmachine")),
    );
    event.insert("userId".into(), json!(uid));
    event.insert("userNickname".into(), json!(nickname(account)));

    // web 事件的指纹就是事件本身（没有设备包信息段），直接当公共指纹上报。
    report_events(account, event.clone(), &desktop_ua(account), vec![event]).await
}

/// 以小程序指纹上报一批事件。
pub async fn report_mp_events(
    account: &Value,
    events: Vec<Map<String, Value>>,
) -> Result<(), String> {
    let mut headers = build_auth_headers(account);
    headers.insert("Accept".to_string(), "application/json".to_string());
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    headers.insert("X-Client-Product".to_string(), "workbuddy-mp".to_string());
    headers.insert("X-Client-Version".to_string(), MP_VERSION.to_string());
    headers.insert("X-Client-Platform".to_string(), "mp-weixin".to_string());
    headers.insert("X-Platform".to_string(), "wechatmp".to_string());

    let url = format!("{}{}", report_base(account), REPORT_PATH);
    let fingerprint = mp_fingerprint(account);
    let mut array = Vec::with_capacity(events.len());
    for event in events {
        let mut merged = fingerprint.clone();
        for (key, value) in event {
            merged.insert(key, value);
        }
        array.push(Value::Object(merged));
    }
    let resp = http_request(&url, "POST", Some(Value::Array(array)), Some(&headers)).await;
    if let Some(code) = resp.get("code").and_then(Value::as_i64) {
        if code != 0 {
            let msg = resp
                .get("msg")
                .and_then(Value::as_str)
                .unwrap_or("小程序上报被上游拒绝");
            return Err(format!("{msg}（code {code}）"));
        }
    }
    Ok(())
}

/// 桌面端「一次成功对话」的完整事件链。
///
/// `agent_task_created → chat_message_send → chat_request_send →
/// chat_message_response(isSuccessful) → chat_message_status → chat_request_response`。
///
/// 服务端对这条链有真实性校验倾向：`chat_message_response` 必须带
/// `isSuccessful=true`，否则不计入对话类任务。
pub fn desktop_chat_sequence(
    conversation_id: &str,
    request_id: &str,
    message_id: &str,
    model_id: &str,
    model_name: &str,
) -> Vec<Map<String, Value>> {
    let trace = request_id.to_string();
    let mut events: Vec<Map<String, Value>> = Vec::new();

    let mut task = Map::new();
    task.insert("eventCode".into(), json!("agent_task_created"));
    task.insert("source".into(), json!("LOCAL"));
    task.insert("name".into(), json!("working"));
    task.insert("task_target".into(), json!("local"));
    task.insert("mode".into(), json!("craft"));
    task.insert("requestModelId".into(), json!(model_id));
    task.insert("requestModelName".into(), json!(model_name));
    task.insert("has_repo".into(), json!(false));
    task.insert("repo_type".into(), json!("none"));
    task.insert("workspace_type".into(), json!("empty"));
    task.insert("has_connector".into(), json!(false));
    task.insert("connector_types".into(), json!([]));
    task.insert("has_mention".into(), json!(false));
    task.insert("mention_types".into(), json!([]));
    task.insert("has_template".into(), json!(false));
    task.insert("action".into(), json!(""));
    task.insert("template_name".into(), json!(""));
    task.insert("has_expert".into(), json!(false));
    task.insert("expert_id".into(), json!(""));
    task.insert("expert_name".into(), json!(""));
    task.insert("expert_industry_id".into(), json!(""));
    task.insert("has_skill".into(), json!(false));
    task.insert("skill_names".into(), json!([]));
    task.insert("conversationId".into(), json!(conversation_id));
    task.insert("messageId".into(), json!(message_id));
    task.insert("buddyId".into(), json!(""));
    task.insert("buddyName".into(), json!(""));
    events.push(task);

    let mut send = Map::new();
    send.insert("eventCode".into(), json!("chat_message_send"));
    send.insert("messageId".into(), json!(format!("{message_id}-assistant")));
    send.insert("historyCount".into(), json!(0));
    send.insert("isContextTruncated".into(), json!(false));
    send.insert("currentStepCount".into(), json!(1));
    send.insert("traceId".into(), json!(trace));
    send.insert("rootRequestId".into(), json!(request_id));
    send.insert("parentConversationId".into(), json!(conversation_id));
    send.insert("agentName".into(), json!("cli"));
    send.insert("agentType".into(), json!("main"));
    events.push(send);

    let mut request = Map::new();
    request.insert("eventCode".into(), json!("chat_request_send"));
    request.insert("inputLength".into(), json!(24));
    request.insert("isPlan".into(), json!(false));
    request.insert("isAutoExecuteTerminal".into(), json!(false));
    request.insert("isAutoModify".into(), json!(false));
    request.insert("codebaseEnable".into(), json!(false));
    request.insert("maxToken".into(), json!(0));
    request.insert("maxSteps".into(), json!(500));
    request.insert("temperature".into(), json!(0));
    request.insert("maxRetries".into(), json!(0));
    request.insert("mentionContexts".into(), json!([]));
    request.insert("knowledgeId".into(), json!([]));
    request.insert("knowledgeName".into(), json!([]));
    request.insert("codebaseId".into(), json!(""));
    request.insert("mentionContextCount".into(), json!(0));
    request.insert("command".into(), json!(""));
    request.insert("recommendId".into(), json!(""));
    request.insert("skillId".into(), json!(""));
    request.insert("skillCount".into(), json!(0));
    request.insert("totalCount".into(), json!(0));
    request.insert("traceId".into(), json!(trace));
    request.insert("rootRequestId".into(), json!(request_id));
    request.insert("parentConversationId".into(), json!(conversation_id));
    request.insert("agentName".into(), json!("cli"));
    request.insert("agentType".into(), json!("main"));
    request.insert("codebuddy.session_id".into(), json!(conversation_id));
    request.insert(
        "codebuddy.conversation_request_id".into(),
        json!(request_id),
    );
    events.push(request);

    let mut response = Map::new();
    response.insert("eventCode".into(), json!("chat_message_response"));
    response.insert(
        "messageId".into(),
        json!(format!("{message_id}-assistant")),
    );
    response.insert("responseModelId".into(), json!(model_id));
    response.insert("inputToken".into(), json!(120));
    response.insert("outputToken".into(), json!(80));
    response.insert("totalToken".into(), json!(200));
    response.insert("cachedTokens".into(), json!(0));
    response.insert("cachedWriteTokens".into(), json!(0));
    response.insert("cachedMissTokens".into(), json!(0));
    response.insert("isSuccessful".into(), json!(true));
    response.insert("messageErrorCode".into(), json!(""));
    response.insert("finishReason".into(), json!("stop"));
    response.insert("firstTokenAt".into(), json!(now_ms()));
    response.insert("traceId".into(), json!(trace));
    response.insert("conversationId".into(), json!(conversation_id));
    response.insert("rootRequestId".into(), json!(request_id));
    response.insert("parentConversationId".into(), json!(conversation_id));
    response.insert("agentName".into(), json!("cli"));
    response.insert("agentType".into(), json!("main"));
    response.insert("codebuddy.session_id".into(), json!(conversation_id));
    response.insert(
        "codebuddy.conversation_request_id".into(),
        json!(request_id),
    );
    events.push(response);

    let mut status = Map::new();
    status.insert("eventCode".into(), json!("chat_message_status"));
    status.insert(
        "messageId".into(),
        json!(format!("{message_id}-assistant")),
    );
    status.insert("messageErrorCode".into(), json!("0"));
    status.insert("traceId".into(), json!(trace));
    status.insert("rootRequestId".into(), json!(request_id));
    status.insert("parentConversationId".into(), json!(conversation_id));
    status.insert("agentName".into(), json!("cli"));
    status.insert("agentType".into(), json!("main"));
    events.push(status);

    let mut request_response = Map::new();
    request_response.insert("eventCode".into(), json!("chat_request_response"));
    request_response.insert("mode".into(), json!("craft"));
    request_response.insert("toolCallCount".into(), json!(0));
    request_response.insert("inputToken".into(), json!(120));
    request_response.insert("outputToken".into(), json!(80));
    request_response.insert("totalToken".into(), json!(200));
    request_response.insert("cachedTokens".into(), json!(0));
    request_response.insert("cachedWriteTokens".into(), json!(0));
    request_response.insert("cachedMissTokens".into(), json!(0));
    request_response.insert("isSuccessful".into(), json!(true));
    request_response.insert("messageErrorCode".into(), json!(""));
    request_response.insert("finishReason".into(), json!("stop"));
    request_response.insert("rootRequestId".into(), json!(request_id));
    events.push(request_response);

    events
}

/// 桌面端「进入 Buddy 应用」五连事件。
///
/// `discover → show → enter_click → auth_confirm → bindaccount_skip`。
/// 同一组事件同时覆盖「进入任一应用」与「进入指定应用」两类判据。
pub fn desktop_buddy_app_sequence(
    buddy_id: &str,
    buddy_name: &str,
) -> Vec<Map<String, Value>> {
    let mk = |code: &str, extra: Vec<(&str, Value)>| -> Map<String, Value> {
        let mut event = Map::new();
        event.insert("eventCode".into(), json!(code));
        event.insert("mode".into(), json!("LOCAL"));
        event.insert("buddyId".into(), json!(buddy_id));
        event.insert("buddyName".into(), json!(buddy_name));
        for (key, value) in extra {
            event.insert(key.to_string(), value);
        }
        event
    };

    vec![
        mk("buddyapp_discover_click", vec![]),
        mk(
            "buddyapp_show",
            vec![
                ("elementId", json!(buddy_id)),
                ("elementName", json!(buddy_name)),
                ("position", json!(2)),
            ],
        ),
        mk(
            "buddyapp_enter_click",
            vec![
                ("elementId", json!(buddy_id)),
                ("elementName", json!(buddy_name)),
                ("position", json!(2)),
                ("isFirstPage", json!("1")),
            ],
        ),
        mk(
            "buddyapp_auth_confirm_click",
            vec![
                ("elementId", json!(buddy_id)),
                ("elementName", json!(buddy_name)),
            ],
        ),
        mk(
            "buddyapp_bindaccount_skip_click",
            vec![
                ("elementId", json!(buddy_id)),
                ("elementName", json!(buddy_name)),
            ],
        ),
    ]
}

/// 桌面端「定时任务创建成功」事件。
pub fn desktop_automation_create_event(name: &str) -> Map<String, Value> {
    let mut event = Map::new();
    event.insert("eventCode".into(), json!("automated_task_create_suc"));
    event.insert("name".into(), json!(name));
    event.insert("source".into(), json!("manually"));
    event.insert("modelId".into(), json!("fast-model"));
    event.insert("modelIsThinking".into(), json!(true));
    event.insert("connectorCount".into(), json!(0));
    event.insert("skills".into(), json!(""));
    event.insert("skillCount".into(), json!(0));
    event.insert("scheduleType".into(), json!("once"));
    event.insert("mode".into(), json!("LOCAL"));
    event
}

/// 桌面端「使用模板创建任务」事件组（JOIN 一条完整对话链）。
pub fn desktop_template_use_sequence(
    conversation_id: &str,
    request_id: &str,
    template_id: &str,
    template_name: &str,
) -> Vec<Map<String, Value>> {
    let mut events =
        desktop_chat_sequence(conversation_id, request_id, &format!("msg-{template_id}"), "fast-model", "fast-model");

    let mut created = Map::new();
    created.insert(
        "eventCode".into(),
        json!("agent_task_created_with_template"),
    );
    created.insert("mode".into(), json!("working"));
    created.insert("isCustomModel".into(), json!(false));
    created.insert("id".into(), json!(template_id));
    created.insert("name".into(), json!(template_name));
    created.insert("requestId".into(), json!(request_id));
    events.push(created);

    let mut used = Map::new();
    used.insert("eventCode".into(), json!("template_used"));
    used.insert("template_id".into(), json!(template_id));
    used.insert("task_mode".into(), json!("working"));
    events.push(used);

    events
}

/// 桌面端「灵感案例做同款」事件组（JOIN 一条完整对话链）。
pub fn desktop_playbook_prompt_sequence(
    conversation_id: &str,
    request_id: &str,
    case_id: &str,
    case_name: &str,
) -> Vec<Map<String, Value>> {
    let mut events = desktop_chat_sequence(
        conversation_id,
        request_id,
        "msg-pb",
        "fast-model",
        "fast-model",
    );

    let mut click = Map::new();
    click.insert("eventCode".into(), json!("web_element_click"));
    click.insert("pageName".into(), json!("playbook_detail"));
    click.insert("elementId".into(), json!("playbook_ctaClick"));
    click.insert("elementName".into(), json!(case_name));
    click.insert("source".into(), json!("discover"));
    events.push(click);

    let mut cta = Map::new();
    cta.insert("eventCode".into(), json!("playbook_cta_click"));
    cta.insert("source".into(), json!("discover"));
    cta.insert("position".into(), json!(0));
    cta.insert("id".into(), json!(case_id));
    cta.insert("name".into(), json!(case_name));
    cta.insert("type".into(), json!("document"));
    cta.insert("categoryId".into(), json!(""));
    cta.insert("categoryName".into(), json!(""));
    events.push(cta);

    let mut send = Map::new();
    send.insert("eventCode".into(), json!("playbook_prompt_send"));
    send.insert("source".into(), json!("discover"));
    send.insert("conversationId".into(), json!(conversation_id));
    send.insert("requestId".into(), json!(request_id));
    send.insert("id".into(), json!(case_id));
    send.insert("name".into(), json!(case_name));
    send.insert("type".into(), json!("document"));
    send.insert("categoryId".into(), json!(""));
    send.insert("categoryName".into(), json!(""));
    events.push(send);

    events
}

/// 专家市场的一个专家（只保留判据需要的字段）。
#[derive(Debug, Clone)]
pub struct MarketExpert {
    pub expert_id: String,
    pub expert_type: String,
    pub display_name: String,
    pub profession: String,
    pub version: String,
    pub category: String,
}

impl MarketExpert {
    /// 专家版本兜底：真机事件必带 version，缺失时按 1.0.0 补。
    pub fn version_or_default(&self) -> String {
        if self.version.is_empty() {
            "1.0.0".to_string()
        } else {
            self.version.clone()
        }
    }
}

/// 桌面端「召唤平台专家」事件组。
pub fn desktop_expert_summon_sequence(expert: &MarketExpert) -> Vec<Map<String, Value>> {
    let category = if expert.category.is_empty() {
        "expert-all".to_string()
    } else {
        expert.category.clone()
    };
    let version = expert.version_or_default();

    let mut click = Map::new();
    click.insert("eventCode".into(), json!("web_element_click"));
    click.insert("source".into(), json!(expert.expert_id));
    click.insert("type".into(), json!(category));
    click.insert("version".into(), json!(version));
    click.insert("elementId".into(), json!("expert_summon_click"));
    click.insert("elementName".into(), json!("立即召唤"));
    click.insert(
        "pageURL".into(),
        json!("/C:/Program%20Files/WorkBuddy/resources/app.asar/renderer/index.html"),
    );

    let mut summon = Map::new();
    summon.insert("eventCode".into(), json!("expert_summon_click"));
    summon.insert("id".into(), json!(expert.expert_id));
    summon.insert("name".into(), json!(expert.display_name));
    summon.insert("expertTitle".into(), json!(expert.profession));
    summon.insert("type".into(), json!("expert-all"));
    summon.insert("position".into(), json!(0));
    summon.insert("expertType".into(), json!(expert.expert_type));
    summon.insert("version".into(), json!(version));
    summon.insert("mode".into(), json!("LOCAL"));

    let mut summoned = Map::new();
    summoned.insert("eventCode".into(), json!("expert_summoned"));
    summoned.insert("id".into(), json!(expert.expert_id));
    summoned.insert("name".into(), json!(expert.display_name));
    summoned.insert("expertTitle".into(), json!(expert.profession));
    summoned.insert("type".into(), json!("expert-all"));

    vec![click, summon, summoned]
}

/// 桌面端「专家真实使用」事件。
///
/// `request_id` 必须是服务端返回的真实 id——自造 UUID 服务端不计数。
pub fn desktop_expert_actual_use_event(
    expert: &MarketExpert,
    conversation_id: &str,
    request_id: &str,
    mode: &str,
) -> Map<String, Value> {
    let category = if expert.category.is_empty() {
        "expert-all".to_string()
    } else {
        expert.category.clone()
    };
    let tail: String = request_id
        .chars()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    let mut event = Map::new();
    event.insert("eventCode".into(), json!("expert_actual_use"));
    event.insert("id".into(), json!(expert.expert_id));
    event.insert("name".into(), json!(expert.display_name));
    event.insert("expertTitle".into(), json!(expert.profession));
    event.insert("type".into(), json!(category));
    event.insert("expertType".into(), json!(expert.expert_type));
    event.insert("source".into(), json!("builtin"));
    event.insert("version".into(), json!(expert.version_or_default()));
    event.insert("cost".into(), json!(9000));
    event.insert("characterCount".into(), json!(14));
    event.insert("conversationId".into(), json!(conversation_id));
    event.insert("requestId".into(), json!(request_id));
    event.insert("messageId".into(), json!(format!("msg-{tail}")));
    event.insert("requestModelId".into(), json!("fast-model"));
    event.insert("requestModelName".into(), json!("fast-model"));
    event.insert("mode".into(), json!(mode));
    event
}

/// 小程序端「一次对话」事件（`Sequential_*` 对话任务计数）。
pub fn mini_chat_send_event(conversation_id: &str) -> Map<String, Value> {
    let rid = new_message_id();
    let tail: String = rid
        .chars()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    let mut event = Map::new();
    event.insert("eventCode".into(), json!("chat_request_send"));
    event.insert("inputLength".into(), json!(14));
    event.insert("isPlan".into(), json!(false));
    event.insert("isAutoExecuteTerminal".into(), json!(false));
    event.insert("isAutoModify".into(), json!(false));
    event.insert("codebaseEnable".into(), json!(false));
    event.insert("maxToken".into(), json!(0));
    event.insert("maxSteps".into(), json!(500));
    event.insert("temperature".into(), json!(0));
    event.insert("maxRetries".into(), json!(0));
    event.insert("mentionContexts".into(), json!([]));
    event.insert("knowledgeId".into(), json!([]));
    event.insert("knowledgeName".into(), json!([]));
    event.insert("codebaseId".into(), json!(""));
    event.insert("mentionContextCount".into(), json!(0));
    event.insert("command".into(), json!(""));
    event.insert("recommendId".into(), json!(""));
    event.insert("skillId".into(), json!(""));
    event.insert("skillCount".into(), json!(0));
    event.insert("totalCount".into(), json!(0));
    event.insert("traceId".into(), json!(rid));
    event.insert("rootRequestId".into(), json!(rid));
    event.insert("parentConversationId".into(), json!(conversation_id));
    event.insert("conversationId".into(), json!(conversation_id));
    event.insert("messageId".into(), json!(format!("msg-{tail}")));
    event.insert("agentName".into(), json!("mp"));
    event.insert("agentType".into(), json!("main"));
    event.insert("codebuddy.session_id".into(), json!(conversation_id));
    event.insert("codebuddy.conversation_request_id".into(), json!(rid));
    event
}

/// 小程序端「选中专家并完成有效对话」事件。
///
/// 不带 conversationId；`expert_id` 必须是专家市场真实 id，空 id 服务端不入账。
pub fn mini_expert_use_event(expert_id: &str, expert_name: &str) -> Map<String, Value> {
    let mut event = Map::new();
    event.insert("eventCode".into(), json!("expert_actual_use"));
    event.insert("reportDelay".into(), json!(0));
    event.insert("extVersion".into(), json!("2.2.8"));
    event.insert("source".into(), json!("mini_program"));
    event.insert("id".into(), json!(expert_id));
    event.insert("name".into(), json!(expert_id));
    event.insert("expertTitle".into(), json!(expert_name));
    event.insert("type".into(), json!("send_message"));
    event.insert("characterCount".into(), json!(12));
    event.insert("expertType".into(), json!("agent"));
    event
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(variant: &str) -> Value {
        json!({
            "uid": "uid-1",
            "nickname": "测试用户",
            "variant": variant,
            "domain": "www.codebuddy.cn",
        })
    }

    #[test]
    fn desktop_fingerprint_carries_cn_triple_and_qimei() {
        let fp = desktop_fingerprint(&account("cn"));
        assert_eq!(fp["ideVersion"], json!(DESKTOP_VERSION_CN));
        assert_eq!(fp["commit"], json!(DESKTOP_COMMIT_CN));
        assert_eq!(fp["releaseDate"], json!(DESKTOP_RELEASE_CN));
        assert_eq!(fp["extName"], json!("workbuddy-desktop"));
        // 国内版每个事件都带 qimei36。
        assert!(fp.get("qimei36").is_some());
        assert_eq!(fp["qimei36"].as_str().unwrap().len(), 36);
    }

    #[test]
    fn global_fingerprint_uses_own_triple_and_no_qimei() {
        let fp = desktop_fingerprint(&account("ai"));
        assert_eq!(fp["ideVersion"], json!(DESKTOP_VERSION_GLOBAL));
        assert_eq!(fp["commit"], json!(DESKTOP_COMMIT_GLOBAL));
        assert_eq!(fp["releaseDate"], json!(DESKTOP_RELEASE_GLOBAL));
        // 国际版一条都不带 qimei36——补上反而是伪造。
        assert!(fp.get("qimei36").is_none());
    }

    #[test]
    fn desktop_ua_differs_by_realm() {
        assert_eq!(
            desktop_ua(&account("cn")),
            "WorkBuddy/5.9.3 WorkBuddy/5.9.3 CLI/2.165.0"
        );
        assert_eq!(
            desktop_ua(&account("ai")),
            "workbuddy-ai/5.9.4 workbuddy-ai/5.9.4 CLI/2.165.0"
        );
    }

    #[test]
    fn derive_id_is_stable_and_hex36() {
        let a = derive_id("uid-1", "machine");
        assert_eq!(a, derive_id("uid-1", "machine"));
        assert_eq!(a.len(), 36);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, derive_id("uid-2", "machine"));
        assert_ne!(a, derive_id("uid-1", "session"));
    }

    #[test]
    fn report_host_is_bare_hostname() {
        assert_eq!(report_host(&account("cn")), "www.workbuddy.cn");
        assert_eq!(report_host(&account("ai")), "www.workbuddy.ai");
        // 不能带 scheme——真机 X-Domain 是裸主机名。
        assert!(!report_host(&account("cn")).contains("https://"));
    }

    #[test]
    fn chat_sequence_has_successful_response_event() {
        let events = desktop_chat_sequence("c1", "r1", "m1", "fast-model", "fast-model");
        let codes: Vec<&str> = events
            .iter()
            .map(|e| e["eventCode"].as_str().unwrap())
            .collect();
        assert_eq!(
            codes,
            vec![
                "agent_task_created",
                "chat_message_send",
                "chat_request_send",
                "chat_message_response",
                "chat_message_status",
                "chat_request_response",
            ]
        );
        // 服务端对这条链有真实性校验：isSuccessful 必须为 true。
        let response = events
            .iter()
            .find(|e| e["eventCode"] == json!("chat_message_response"))
            .unwrap();
        assert_eq!(response["isSuccessful"], json!(true));
    }

    #[test]
    fn buddy_app_sequence_has_five_events() {
        let events = desktop_buddy_app_sequence("cb_x", "企鹅教师助手");
        assert_eq!(events.len(), 5);
        assert_eq!(events[0]["eventCode"], json!("buddyapp_discover_click"));
        assert_eq!(
            events[4]["eventCode"],
            json!("buddyapp_bindaccount_skip_click")
        );
        assert_eq!(events[0]["buddyId"], json!("cb_x"));
    }

    #[test]
    fn template_sequence_appends_template_used() {
        let events = desktop_template_use_sequence("c1", "r1", "1", "深度研究");
        let last = events.last().unwrap();
        assert_eq!(last["eventCode"], json!("template_used"));
        assert_eq!(last["template_id"], json!("1"));
    }

    #[test]
    fn expert_actual_use_carries_server_request_id() {
        let expert = MarketExpert {
            expert_id: "ex_1".into(),
            expert_type: "agent".into(),
            display_name: "某专家".into(),
            profession: "某专家".into(),
            version: "1.0.0".into(),
            category: String::new(),
        };
        let event = desktop_expert_actual_use_event(&expert, "c1", "cmb-abc123", "craft");
        assert_eq!(event["requestId"], json!("cmb-abc123"));
        assert_eq!(event["mode"], json!("craft"));
        assert_eq!(event["type"], json!("expert-all"));
    }

    #[test]
    fn mini_expert_use_event_has_no_conversation_id() {
        let event = mini_expert_use_event("ex_1", "某专家");
        assert_eq!(event["eventCode"], json!("expert_actual_use"));
        assert_eq!(event["source"], json!("mini_program"));
        assert_eq!(event["type"], json!("send_message"));
        // 不带 conversationId——小程序源码的发射点就是这种形状。
        assert!(event.get("conversationId").is_none());
    }

    #[test]
    fn mini_chat_event_has_expected_shape() {
        let event = mini_chat_send_event("conv-1");
        assert_eq!(event["eventCode"], json!("chat_request_send"));
        assert_eq!(event["agentName"], json!("mp"));
        assert_eq!(event["conversationId"], json!("conv-1"));
        assert!(event["rootRequestId"].as_str().unwrap().len() >= 32);
    }

    #[test]
    fn mp_fingerprint_marks_mini_program() {
        let fp = mp_fingerprint(&account("cn"));
        assert_eq!(fp["extName"], json!("workbuddy-mp"));
        assert_eq!(fp["platform"], json!("mini_program"));
        assert_eq!(fp["ideName"], json!("wx_app_cloud"));
    }
}
