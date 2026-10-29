//! 成长任务的**事件构造器与三套指纹**（从参考项目 task_runner.py 的
//! `build_event` / `desktop_fingerprint` / `report_*` 逐条移植）。
//!
//! ── 三条上报通道（各有各的指纹与域）─────────────────────────
//! ```text
//! 桌面通道   POST {bill}/v2/report      常规任务：每个事件注入 desktopFingerprint
//! 桌面事件组 POST {chat}/v2/report      6 连对话链 / 五连 Buddy 事件（chat 域）
//! web 通道   POST {web}/v2/report       Library_read（web 指纹 + x-client-platform: web）
//! MP 通道    POST {bill}/v2/report      小程序任务组（mp 指纹，无 X-Client-Platform）
//! ```
//! `X-Client-Platform: miniprogram` 只在**任务端点**（list/accept/claim）需要，
//! 上报事件的 mp 身份由 **body 里的指纹**表达（ideName=wx_app_cloud +
//! ideType=WorkBuddy_MP + source=mini_program）—— 这是实测沉淀，别混。
//!
//! ── 形状纪律 ────────────────────────────────────────────────
//! 每个构造器上方的注释标注了形状来源（哪个客户端产物 / 哪条实测）。服务端按
//! 形状归账：**不要「顺手统一」字段、不要多带键** —— 桌面 automation 与小程序
//! automated_task 是不同的域口径（前者带 rrule 对象、后者不带），照抄会失去
//! 关联。这些差异都写在各自构造器旁边。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{Value, json};

use super::spec::ObjectId;
use super::{DESKTOP_UA, GrowthAccount, WEB_BASE, WEB_UA, bill_call};

/// 小程序任务端点头（list / accept / claim 用）。
pub fn mp_platform_header(mp: bool) -> Vec<(String, String)> {
    if mp {
        vec![("X-Client-Platform".to_string(), "miniprogram".to_string())]
    } else {
        Vec::new()
    }
}

/// 夜猫窗口判定：23:00-08:00（**北京时间**；上游按 CST 判时段）。
pub fn within_night_window() -> bool {
    use chrono::Timelike;
    let hour = crate::beijing::now().hour();
    hour >= 23 || hour < 8
}

fn now_ms() -> i64 {
    crate::logging::now_ms()
}

// ─── 桌面指纹（fork desktopFingerprint 同款公共字段）──────────

/// 桌面指纹：注入每个桌面事件，覆盖同名业务键。
fn desktop_fingerprint(account: &GrowthAccount) -> Value {
    let now = now_ms();
    json!({
        "timezone": "Asia/Shanghai", "reportDelay": 2000,
        "userId": account.uid,
        "username": account.nick, "userNickname": account.nick,
        "product": "SaaS", "releaseDate": 1789036585355i64,
        "commit": "5f9692923c93033111c51ad7b003eb80204a9b75",
        "ideName": "WorkBuddy", "ideType": "WorkBuddy", "ideVersion": "5.5.6",
        "machineId": account.derive_id("machine"), "sessionId": account.derive_id("session"),
        "extName": "workbuddy-desktop", "extVersion": "5.5.6",
        "os": "win32", "arch": "x64", "osVersion": "10.0.26220",
        "cpuCores": 20, "memorySize": 24,
        "timestamp": now, "presentAt": now,
    })
}

/// 小程序公共遥测字段（小程序源码 module 22015 的真实形状）。
///
/// ideVersion/extVersion/product 取源码常量模块 25439（2.2.8 是小程序包版本
/// 而非 SaaS 状态 —— school 段那份 extVersion="SaaS" 是早前凭观察拼的，
/// 以源码为准则）。
fn mp_device_fingerprint(account: &GrowthAccount) -> Value {
    json!({
        "ideType": "WorkBuddy_MP", "ideVersion": "2.2.8",
        "extName": "workbuddy-mp", "extVersion": "2.2.8", "product": "SaaS",
        "os": "android", "osVersion": "14", "arch": "arm64",
        "timezone": "Asia/Shanghai",
        "machineId": account.derive_id("machine"),
    })
}

/// chat 域桌面指纹请求头（fork ReportDesktopEvent 同款）。
fn desktop_headers(account: &GrowthAccount) -> Vec<(String, String)> {
    vec![
        ("Accept".to_string(), "application/json, text/plain, */*".to_string()),
        ("Content-Type".to_string(), "application/json;charset=UTF-8".to_string()),
        ("User-Agent".to_string(), DESKTOP_UA.to_string()),
        ("X-Domain".to_string(), super::CHAT_BASE.to_string()),
        ("X-Product".to_string(), "SaaS".to_string()),
        (
            "X-Request-ID".to_string(),
            format!("{}{}", account.derive_id("req"), now_ms() % 1_000_000),
        ),
        ("X-User-Id".to_string(), account.uid.clone()),
    ]
}

/// web 域事件请求头（fork ReportWebEvent 同款）。
fn web_event_headers(account: &GrowthAccount, page_url: &str) -> Vec<(String, String)> {
    vec![
        ("Accept".to_string(), "application/json".to_string()),
        ("Content-Type".to_string(), "application/json".to_string()),
        ("x-client-platform".to_string(), "web".to_string()),
        ("Origin".to_string(), WEB_BASE.to_string()),
        ("Referer".to_string(), page_url.to_string()),
        ("User-Agent".to_string(), WEB_UA.to_string()),
        ("X-User-Id".to_string(), account.uid.clone()),
        ("X-Domain".to_string(), WEB_BASE.to_string()),
    ]
}

/// web 域领奖头（fork ClaimReward 同款，claim web 降级用）。
pub fn web_claim_headers(account: &GrowthAccount) -> Vec<(String, String)> {
    vec![
        ("Accept".to_string(), "application/json, text/plain, */*".to_string()),
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Origin".to_string(), WEB_BASE.to_string()),
        ("Referer".to_string(), format!("{WEB_BASE}/profile/growth-center")),
        ("x-client-platform".to_string(), "web".to_string()),
        ("User-Agent".to_string(), WEB_UA.to_string()),
        ("X-User-Id".to_string(), account.uid.clone()),
        ("X-Domain".to_string(), WEB_BASE.to_string()),
    ]
}

// ─── 事件构造器（照抄各 task_*.py 已验证形状，必带 userId）────

/// 事件会话 id：`wb-run-<ms>-<idx>`（参考项目同款命名，便于与参考脚本对日志）。
fn conversation(index: usize) -> (String, String) {
    let now = now_ms();
    let cid = format!("wb-run-{now}-{index}");
    let rid = format!("{cid}-{now}");
    (cid, rid)
}

/// 单个事件构造（kind 分派；`index` 只用于生成互不相同的会话 id）。
pub fn build_event(account: &GrowthAccount, kind: &str, object: &ObjectId, index: usize) -> Value {
    let now = now_ms();
    let (cid, rid) = conversation(index);
    let uid = account.uid.clone();
    let meta = if object.meta.is_null() { json!({}) } else { object.meta.clone() };
    let name = meta.get("name").and_then(Value::as_str).unwrap_or("").to_string();
    let object_id = object.id.as_str();
    match kind {
        "canvas" => json!({
            "eventCode": "wbx_design_canvas_task_create", "timestamp": now,
            "reportDelay": 0, "conversationId": cid, "requestId": rid,
            "source": "summon_keyword", "isCustomModel": false, "name": "",
            "inputLength": 12, "id": object_id, "cost": 0, "isSuccessful": true, "userId": uid,
        }),
        "template" => json!({
            "eventCode": "agent_task_created_with_template", "timestamp": now,
            "reportDelay": 0, "isCustomModel": true, "id": object_id,
            "name": name, "requestId": rid, "conversationId": cid, "userId": uid,
        }),
        "expert" | "lighthouse" => json!({
            "eventCode": "expert_actual_use", "timestamp": now, "reportDelay": 0,
            "mode": "CLOUD", "id": object_id, "name": if name.is_empty() { object_id } else { &name },
            "expertTitle": name, "type": meta.get("category").and_then(Value::as_str).unwrap_or(""),
            "expertType": meta.get("expertType").and_then(Value::as_str).unwrap_or("agent"),
            "source": "builtin", "version": meta.get("version").and_then(Value::as_str).unwrap_or(""),
            "cost": 0, "characterCount": 12, "conversationId": cid, "requestId": rid,
            "messageId": rid, "requestModelId": "deepseek-v4-flash",
            "requestModelName": "DeepSeek V4 Flash", "userId": uid,
        }),
        "team" => json!({
            "eventCode": "expert_actual_use", "timestamp": now, "reportDelay": 0,
            "mode": "CLOUD", "id": object_id, "name": if name.is_empty() { object_id } else { &name },
            "expertTitle": name, "type": meta.get("category").and_then(Value::as_str).unwrap_or(""),
            "expertType": "team", "source": "builtin",
            "version": meta.get("version").and_then(Value::as_str).unwrap_or(""),
            "cost": 0, "characterCount": 12, "conversationId": cid, "requestId": rid,
            "messageId": rid, "requestModelId": "deepseek-v4-flash",
            "requestModelName": "DeepSeek V4 Flash", "userId": uid,
        }),
        "skill" => json!({
            "eventCode": "skill_info", "timestamp": now, "reportDelay": 0,
            "skillId": object_id, "skillName": if name.is_empty() { object_id } else { &name },
            "skillVersion": "", "action": "use", "conversationId": cid,
            "requestId": rid, "userId": uid,
        }),
        "automation" => json!({
            "eventCode": "automated_task_create_suc", "timestamp": now,
            "reportDelay": 0, "name": "每周五自动生成周报", "source": "manually",
            "modelId": "deepseek-v4-flash", "modelIsThinking": false,
            "expertId": "", "expertMarketplace": "", "connectorIds": "",
            "connectorCount": 0, "skills": "", "skillCount": 0,
            "scheduleType": "recurring", "pushToWeChat": false, "pushToWecomBot": false,
            "conversationId": cid, "requestId": rid,
            "schedule": {"type": "recurring", "rrule": "FREQ=WEEKLY;BYDAY=FR;BYHOUR=9;BYMINUTE=0"},
            "prompt": "每周五自动整理本周工作，生成一份周报。", "userId": uid,
        }),
        "playbook" => {
            let skills = meta.get("skills").and_then(Value::as_array).cloned().unwrap_or_default();
            let experts = meta.get("experts").and_then(Value::as_array).cloned().unwrap_or_default();
            let category = meta
                .get("categories")
                .and_then(Value::as_array)
                .and_then(|items| items.first())
                .and_then(Value::as_str)
                .unwrap_or("");
            let first_expert = experts.first().cloned().unwrap_or(Value::Null);
            json!({
                "eventCode": "playbook_prompt_send", "timestamp": now, "reportDelay": 0,
                "id": object_id,
                "name": meta.get("title").and_then(Value::as_str).unwrap_or(object_id),
                "type": meta.get("artifact_type").and_then(Value::as_str).unwrap_or("other"),
                "promptLength": meta.get("prompt").and_then(Value::as_str).unwrap_or("").chars().count(),
                "isOfficial": 1,
                "skills": skills.iter().filter_map(|s| s.get("skill_id").and_then(Value::as_str)).collect::<Vec<_>>().join(","),
                "skillNames": skills.iter().filter_map(|s| s.get("name").and_then(Value::as_str)).collect::<Vec<_>>().join(","),
                "expertId": first_expert.get("expert_id").or_else(|| first_expert.get("id")).and_then(Value::as_str).unwrap_or(""),
                "expertName": first_expert.get("name").and_then(Value::as_str).unwrap_or(""),
                "categoryId": category, "categoryName": "", "query": "",
                "source": "discover", "conversationId": cid, "requestId": rid,
                "ext1": "discover", "userId": uid,
            })
        }
        "skin" => json!({
            "eventCode": "appearance_skin_apply", "timestamp": now, "reportDelay": 0,
            "action": "apply", "source": "settings_close", "id": object_id,
            "vipLevel": meta.get("vipLevel").and_then(Value::as_str).unwrap_or("free"),
            "series": meta.get("series").and_then(Value::as_str).unwrap_or("craft"),
            "type": "unknown", "name": if name.is_empty() { object_id } else { &name },
            "userId": uid,
        }),
        "chat" | "glmchat" | "cat" => {
            let (model_id, model_name) = if kind == "chat" {
                ("deepseek-v4-flash", "DeepSeek V4 Flash")
            } else {
                ("glm-5.2", "GLM-5.2")
            };
            let mode = if kind == "cat" { "night" } else { "craft" };
            json!({
                "eventCode": "chat_request_send", "timestamp": now, "reportDelay": 0,
                "mode": mode, "conversationId": cid, "requestId": cid,
                "inputLength": 12, "requestModelId": model_id, "requestModelName": model_name,
                "isPlan": false, "isAutoExecuteTerminal": false, "isAutoModify": false,
                "codebaseEnable": false, "maxToken": 0, "maxSteps": 0, "temperature": 0,
                "maxRetries": 0, "mentionContexts": [], "knowledgeId": [],
                "knowledgeName": [], "codebaseId": "", "mentionContextCount": 0,
                "command": "", "expertId": "", "recommendId": "", "skillId": "",
                "skillCount": 0, "totalCount": 0, "fileUri": "", "presentAt": now,
                "traceId": "", "rootRequestId": cid, "parentConversationId": cid,
                "agentName": "default", "agentType": "conversation", "userId": uid,
            })
        }
        // ── 小程序任务组（mp 指纹）──
        "minichat" => json!({
            // Sequential_Tasks_1 判据：mini 指纹 chat_request_send，无 activityId
            "eventCode": "chat_request_send", "timestamp": now, "reportDelay": 0,
            "source": "mini_program", "ideName": "wx_app_cloud",
            "ideType": "WorkBuddy_MP", "extName": "workbuddy-mp",
            "extVersion": "2.4.0", "mode": "chat",
            "conversationId": cid, "requestId": cid, "inputLength": 12,
            "mentionContexts": [], "mentionContextCount": 0, "userId": uid,
        }),
        "miniexpert" => json!({
            // Sequential_Tasks_2 判据：mp 指纹 expert_actual_use
            //（无 conversationId/activityId；type 固定 send_message）
            "eventCode": "expert_actual_use", "timestamp": now, "reportDelay": 0,
            "ideName": "wx_app_cloud", "ideType": "WorkBuddy_MP",
            "extName": "workbuddy-mp", "extVersion": "2.2.8", "product": "SaaS",
            "source": "mini_program", "os": "android", "osVersion": "14",
            "arch": "arm64", "timezone": "Asia/Shanghai",
            "machineId": account.derive_id("machine"),
            "userId": uid, "id": object_id, "name": object_id,
            "expertTitle": if name.is_empty() { object_id } else { &name },
            "type": "send_message", "characterCount": 12,
            "expertType": meta.get("expertType").and_then(Value::as_str).unwrap_or("agent"),
        }),
        "miniautomation" => json!({
            // Sequential_Tasks_4 判据：mp 指纹 automated_task_create_suc
            //（**不带** schedule/rrule 对象 —— 与桌面 automation_1 不同源）
            "eventCode": "automated_task_create_suc", "timestamp": now,
            "reportDelay": 0, "ideName": "wx_app_cloud",
            "ideType": "WorkBuddy_MP", "ideVersion": "2.2.8",
            "extName": "workbuddy-mp", "extVersion": "2.2.8", "product": "SaaS",
            "source": "mini_program", "mode": "CLOUD",
            "os": "android", "osVersion": "14", "arch": "arm64",
            "timezone": "Asia/Shanghai",
            "machineId": account.derive_id("machine"),
            "userId": uid, "userNickname": account.nick,
            "name": "每日读书提醒", "skills": "", "skillCount": 0,
            "scheduleType": "daily",
        }),
        "miniglmchat" => {
            let mut event = json!({
                // Sequential_Tasks_5 判据：mini 指纹 chat_request_send + 模型上下文
                "eventCode": "chat_request_send", "timestamp": now, "reportDelay": 0,
                "ideName": "wx_app_cloud", "mode": "chat",
                "source": "mini_program",
                "conversationId": cid, "requestId": cid, "inputLength": 12,
                "requestModelId": "glm-5.2", "requestModelName": "GLM-5.2",
                "mentionContexts": [], "mentionContextCount": 0,
                "command": "", "traceId": cid,
                "userId": uid,
            });
            if let (Some(base), Some(fp)) =
                (event.as_object_mut(), mp_device_fingerprint(account).as_object())
            {
                for (key, value) in fp {
                    base.insert(key.clone(), value.clone());
                }
            }
            event
        }
        // school_season 走 school 模块的构造器（单一事实源），见 school.rs 的调用
        _ => json!({ "eventCode": kind, "timestamp": now, "userId": uid }),
    }
}

/// 桌面端 6 连「成功对话」事件链（点亮 RichMeow_Chat；fork DesktopChatSequence）。
///
/// `pub(super)`：school.rs 的 desktop_chat_1_time 判据复用同一份形状
/// （那里额外注入 activityId）—— 同一事实源，不抄第二份。
pub(super) fn desktop_chat_sequence(
    account: &GrowthAccount,
    conversation_id: &str,
    request_id: &str,
    message_id: &str,
) -> Vec<Value> {
    let now = now_ms();
    vec![
        json!({
            "eventCode": "agent_task_created",
            "source": "LOCAL", "name": "working", "task_target": "local", "mode": "craft",
            "requestModelId": "fast-model", "requestModelName": "fast-model",
            "has_repo": false, "repo_type": "none", "workspace_type": "empty",
            "has_connector": false, "connector_types": [],
            "has_mention": false, "mention_types": [],
            "has_template": false, "action": "", "template_name": "",
            "has_expert": false, "expert_id": "", "expert_name": "", "expert_industry_id": "",
            "has_skill": false, "skill_names": [],
            "conversationId": conversation_id, "messageId": message_id,
            "buddyId": "", "buddyName": "",
        }),
        json!({
            "eventCode": "chat_message_send",
            "messageId": format!("{message_id}-assistant"), "historyCount": 0,
            "isContextTruncated": false, "currentStepCount": 1,
            "traceId": request_id, "rootRequestId": request_id,
            "parentConversationId": conversation_id,
            "agentName": "cli", "agentType": "main",
        }),
        json!({
            "eventCode": "chat_request_send",
            "inputLength": 24, "isPlan": false, "isAutoExecuteTerminal": false,
            "isAutoModify": false, "codebaseEnable": false, "maxToken": 0,
            "maxSteps": 500, "temperature": 0, "maxRetries": 0,
            "mentionContexts": [], "knowledgeId": [], "knowledgeName": [],
            "codebaseId": "", "mentionContextCount": 0, "command": "",
            "recommendId": "", "skillId": "", "skillCount": 0, "totalCount": 0,
            "traceId": request_id, "rootRequestId": request_id,
            "parentConversationId": conversation_id,
            "agentName": "cli", "agentType": "main",
            "codebuddy.session_id": conversation_id,
            "codebuddy.conversation_request_id": request_id,
        }),
        json!({
            "eventCode": "chat_message_response",
            "messageId": format!("{message_id}-assistant"), "responseModelId": "fast-model",
            "inputToken": 120, "outputToken": 80, "totalToken": 200,
            "cachedTokens": 0, "cachedWriteTokens": 0, "cachedMissTokens": 0,
            "isSuccessful": true, "messageErrorCode": "", "finishReason": "stop",
            "firstTokenAt": now, "traceId": request_id,
            "conversationId": conversation_id,
            "rootRequestId": request_id, "parentConversationId": conversation_id,
            "agentName": "cli", "agentType": "main",
            "codebuddy.session_id": conversation_id,
            "codebuddy.conversation_request_id": request_id,
        }),
        json!({
            "eventCode": "chat_message_status",
            "messageId": format!("{message_id}-assistant"), "messageErrorCode": "0",
            "traceId": request_id, "rootRequestId": request_id,
            "parentConversationId": conversation_id,
            "agentName": "cli", "agentType": "main",
        }),
        json!({
            "eventCode": "chat_request_response",
            "mode": "craft", "toolCallCount": 0,
            "inputToken": 120, "outputToken": 80, "totalToken": 200,
            "cachedTokens": 0, "cachedWriteTokens": 0, "cachedMissTokens": 0,
            "isSuccessful": true, "messageErrorCode": "", "finishReason": "stop",
            "rootRequestId": request_id, "parentConversationId": conversation_id,
        }),
    ]
    .into_iter()
    .map(|mut event| {
        // 注入桌面指纹（覆盖同名业务键）
        if let (Some(base), Some(fp)) =
            (event.as_object_mut(), desktop_fingerprint(account).as_object())
        {
            for (key, value) in fp {
                base.insert(key.clone(), value.clone());
            }
        }
        event
    })
    .collect()
}

/// 五连「进入 Buddy 应用」事件（点亮 Buddy_App / Buddy_App_QQ；fork DesktopBuddyAppSequence）。
fn desktop_buddy5_sequence(account: &GrowthAccount, buddy_id: &str, buddy_name: &str) -> Vec<Value> {
    let mut events = Vec::new();
    let mut push = |code: &str, extra: Value| {
        let mut event = json!({
            "eventCode": code, "mode": "LOCAL",
            "buddyId": buddy_id, "buddyName": buddy_name,
        });
        if let (Some(base), Some(more)) = (event.as_object_mut(), extra.as_object()) {
            for (key, value) in more {
                base.insert(key.clone(), value.clone());
            }
        }
        events.push(event);
    };
    push("buddyapp_discover_click", json!({}));
    push("buddyapp_show", json!({ "elementId": buddy_id, "elementName": buddy_name, "position": 2 }));
    push(
        "buddyapp_enter_click",
        json!({ "elementId": buddy_id, "elementName": buddy_name, "position": 2, "isFirstPage": "1" }),
    );
    push(
        "buddyapp_auth_confirm_click",
        json!({ "elementId": buddy_id, "elementName": buddy_name }),
    );
    push(
        "buddyapp_bindaccount_skip_click",
        json!({ "elementId": buddy_id, "elementName": buddy_name }),
    );
    let _ = account;
    events
}

/// 一次常规「chat_request_send」上报（billing 域；领养前置解锁用）。
pub async fn report_chat_send(account: &GrowthAccount, night: bool) -> Result<(), String> {
    let object = ObjectId { id: String::new(), meta: Value::Null };
    let kind = if night { "cat" } else { "chat" };
    let event = build_event(account, kind, &object, 0);
    let call = bill_call(account, "POST", "/v2/report", Some(&json!([event])), &[]).await?;
    if call.ok() {
        Ok(())
    } else {
        Err(format!("report HTTP {} code={:?}", call.status, call.code()))
    }
}

/// # 分派点亮动作（按 kind 选择域与事件形状；返回 None 表示本组全部成功）。
///
/// 各分派的域选择是实测沉淀：桌面事件组走 **chat 域**（发 codebuddy.cn 不点亮）、
/// 常规事件走 **billing 域**、web 事件走 **web 域**、MP 事件走 billing 域的 mp 指纹。
pub async fn light_up(
    account: &GrowthAccount,
    code: &str,
    kind: &str,
    objects: &[ObjectId],
    need: usize,
) -> Option<String> {
    match kind {
        // 桌面 6 连对话链：单组 = 一次对话；一次上报即点亮
        "richmeow" => {
            for index in 0..need.max(1) {
                let now = now_ms();
                let conv = format!("wb-run-rm-{now}-{index}");
                let req = format!("wb-run-rm-req-{now}-{index}");
                let msg = format!("req-{now}-{index}-user");
                let events = desktop_chat_sequence(account, &conv, &req, &msg);
                if let Err(error) = report_desktop_events(account, &events).await {
                    return Some(error);
                }
                tokio::time::sleep(std::time::Duration::from_millis(super::GROWTH_WRITE_GAP_MS))
                    .await;
            }
            None
        }
        // 五连 Buddy 事件；失败降级为单发 buddyapp_enter_click（旧形状兜底）
        "buddy5" => {
            let (buddy_id, buddy_name) = buddy_app_of(code);
            for _ in 0..need.max(1) {
                let events = desktop_buddy5_sequence(account, buddy_id, buddy_name);
                match report_desktop_events(account, &events).await {
                    Ok(()) => {}
                    Err(error) => {
                        // 降级单发（旧形状兜底，实测亦可点亮）
                        let single = json!({
                            "eventCode": "buddyapp_enter_click", "timestamp": now_ms(),
                            "reportDelay": 0, "elementId": buddy_id,
                            "elementName": buddy_name, "position": 1,
                            "isFirstPage": "0", "userId": account.uid,
                        });
                        if let Err(error2) = report_desktop_events(account, &[single]).await {
                            return Some(format!("{error}；降级单发亦失败：{error2}"));
                        }
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(super::GROWTH_WRITE_GAP_MS))
                    .await;
            }
            None
        }
        // web 域单事件（web_element_click）
        "library" => {
            for _ in 0..need.max(1) {
                if let Err(error) = report_web_library(account).await {
                    return Some(error);
                }
                tokio::time::sleep(std::time::Duration::from_millis(super::GROWTH_WRITE_GAP_MS))
                    .await;
            }
            None
        }
        // MP 事件组：走 billing 域 /v2/report（mp 指纹在 body 里）
        "minichat" | "miniexpert" | "miniautomation" | "miniglmchat" => {
            for (index, object) in objects.iter().enumerate().take(need.max(1)) {
                let event = build_event(account, kind, object, index);
                if let Err(error) = report_mp_event(account, &event).await {
                    return Some(error);
                }
                tokio::time::sleep(std::time::Duration::from_millis(super::GROWTH_WRITE_GAP_MS))
                    .await;
            }
            None
        }
        // school_season：复用 school 模块构造器（单一事实源）
        "schoolseason" => {
            for _ in 0..need.max(1) {
                let event = super::school::mini_chat_event(account, None);
                if let Err(error) = report_mp_event(account, &event).await {
                    return Some(error);
                }
                tokio::time::sleep(std::time::Duration::from_millis(super::GROWTH_WRITE_GAP_MS))
                    .await;
            }
            None
        }
        // 领养链路（first_buddy）不走批量上报：report→agreement→buddy/first 的
        // 专用路径在 growth::process_first_buddy / adopt_buddy。这里防御性空操作
        // （误走时什么都不发，而不是发一个形状错误的事件）。
        "buddyfirst" => None,
        // 其余常规：billing 域逐事件上报
        _ => {
            let count = if objects.is_empty() { need.max(1) } else { objects.len().min(need.max(1)) };
            for index in 0..count {
                let object = objects
                    .get(index)
                    .cloned()
                    .unwrap_or(ObjectId { id: String::new(), meta: Value::Null });
                let event = build_event(account, kind, &object, index);
                if let Err(error) = report_regular(account, &event).await {
                    return Some(error);
                }
                if index + 1 < count {
                    tokio::time::sleep(std::time::Duration::from_millis(super::GROWTH_WRITE_GAP_MS))
                        .await;
                }
            }
            None
        }
    }
}

/// Buddy 应用承载（实测用企鹅教师助手，同一组事件同时满足 Buddy_App 与 _QQ）。
fn buddy_app_of(code: &str) -> (&'static str, &'static str) {
    let _ = code;
    ("cb_y5Dy46tPQGGWtueMxXbe", "企鹅教师助手")
}

/// 桌面事件组上报：chat 域（`{chat}/v2/report`），注入桌面指纹已在构造时完成。
pub async fn report_desktop_events(account: &GrowthAccount, events: &[Value]) -> Result<(), String> {
    let headers = desktop_headers(account);
    let call = super::growth_request(
        account,
        "POST",
        &format!("{}/v2/report", super::CHAT_BASE),
        Some(&Value::Array(events.to_vec())),
        &headers,
    )
    .await?;
    if call.ok() {
        Ok(())
    } else {
        Err(format!("桌面事件上报 HTTP {} code={:?}", call.status, call.code()))
    }
}

/// 常规事件上报：billing 域（`{bill}/v2/report`）。
pub(super) async fn report_regular(account: &GrowthAccount, event: &Value) -> Result<(), String> {
    let call = bill_call(
        account,
        "POST",
        "/v2/report",
        Some(&json!([event])),
        &[],
    )
    .await?;
    if call.ok() {
        Ok(())
    } else {
        Err(format!("上报 HTTP {} code={:?}", call.status, call.code()))
    }
}

/// MP 事件上报：billing 域（不含 X-Client-Platform —— mp 身份在 body 指纹里）。
async fn report_mp_event(account: &GrowthAccount, event: &Value) -> Result<(), String> {
    report_regular(account, event).await
}

/// web 域资料库介绍点击（web_element_click）。
async fn report_web_library(account: &GrowthAccount) -> Result<(), String> {
    const LIBRARY_DOC_URL: &str = "https://www.workbuddy.cn/space/d/o0KWYeynteVv06UnAZqIFm";
    let now = now_ms();
    let event = json!({
        "eventCode": "web_element_click", "timestamp": now, "reportDelay": 0,
        "pageURL": LIBRARY_DOC_URL, "elementId": "library_doc_intro_click",
        "elementName": "WorkBuddy资料库介绍",
        "os": "Win32", "arch": "", "osVersion": "10.0", "userAgent": WEB_UA,
        "machineId": account.derive_id("webmachine"), "userId": account.uid,
        "userNickname": account.nick,
    });
    let headers = web_event_headers(account, LIBRARY_DOC_URL);
    let call = super::growth_request(
        account,
        "POST",
        &format!("{WEB_BASE}/v2/report"),
        Some(&json!([event])),
        &headers,
    )
    .await?;
    if call.ok() {
        Ok(())
    } else {
        Err(format!("web 事件上报 HTTP {} code={:?}", call.status, call.code()))
    }
}
