//! 指定专家任务只发真实对话，任务完成与奖励仍由上游回读结果决定。
//! 任务链接只用于提取专家身份，不能作为携带账号凭证的请求地址。

use futures::StreamExt;
use serde_json::{json, Value};
use url::Url;

use super::spec::ObjectId;
use super::GrowthAccount;

const MODEL: &str = "fast-model";
const PROMPT: &str = "请用一句话介绍你的专长。";

pub async fn resolve(account: &GrowthAccount, task: &Value) -> Result<ObjectId, String> {
    let link = task.get("jump_url").and_then(Value::as_str).unwrap_or("").trim();
    let supplied_id = expert_id(link)?;
    let code = task.get("task_code").and_then(Value::as_str).unwrap_or("");
    let keyword = match code {
        "expert_use_maidehao" => "中国电商运营专家",
        "expert_use_douyinyin" => "抖音策略师",
        "expert_use_shangjia_xuanchuan" => "商家宣传页与海报设计师",
        "expert_use_bowangwang" => "直播电商教练",
        "WorkBuddy_Valorant" => "无畏契约拼豆专家",
        _ => return Err("未知的指定专家任务".to_string()),
    };
    if supplied_id.is_none() && code != "WorkBuddy_Valorant" {
        return Err("任务未提供有效的指定专家身份，请到官方客户端完成".to_string());
    }
    let experts = super::spec::fetch_market_experts(account, false, Some(keyword)).await;
    let mut matches = experts.into_iter().filter(|expert| {
        if let Some(id) = &supplied_id { return &expert.id == id; }
        valid_id(&expert.id) && ["name", "profession"].into_iter().any(|key| {
            text(&expert.meta, key) == keyword
        })
    });
    let expert = matches.next().ok_or_else(|| "市场未返回任务指定的专家，无法确认身份".to_string())?;
    if matches.next().is_some() {
        return Err("专家市场存在多个同名专家，无法确认任务对象".to_string());
    }
    if text(&expert.meta, "displayName").is_empty() || text(&expert.meta, "profession").is_empty() {
        return Err("专家市场未返回完整的昵称和职业信息，无法确认使用记录".to_string());
    }
    Ok(expert)
}

fn expert_id(link: &str) -> Result<Option<String>, String> {
    if link.is_empty() { return Ok(None); }
    let mut url = Url::parse(link).map_err(|_| "任务跳转链接格式无法识别".to_string())?;
    if matches!(url.scheme(), "http" | "https") {
        let host = url.host_str().unwrap_or("");
        if !["workbuddy.cn", "codebuddy.cn", "tencent.com"].into_iter()
            .any(|domain| host == domain || host.ends_with(&format!(".{domain}"))) {
            return Err("任务跳转链接不属于 WorkBuddy".to_string());
        }
        let links: Vec<_> = url.query_pairs().filter(|(key, _)| key == "deeplink")
            .map(|(_, value)| value.into_owned()).collect();
        if links.is_empty() { return Ok(None); }
        if links.len() != 1 { return Err("任务跳转链接包含重复的专家入口".to_string()); }
        url = Url::parse(&links[0]).map_err(|_| "任务专家入口格式无法识别".to_string())?;
    }
    if url.scheme() != "workbuddy" || !matches!(url.host_str(), Some("experts" | "expert"))
        || !matches!(url.path(), "" | "/") || url.fragment().is_some() {
        return Err("任务未提供可识别的专家入口".to_string());
    }
    let ids: Vec<_> = url.query_pairs().filter(|(key, _)| key == "expertId")
        .map(|(_, value)| value.into_owned()).collect();
    if ids.is_empty() { return Ok(None); }
    if ids.len() != 1 || !valid_id(&ids[0]) {
        return Err("任务指定的专家身份无效".to_string());
    }
    Ok(ids.into_iter().next())
}

fn valid_id(id: &str) -> bool {
    id.strip_prefix("ex_").is_some_and(|suffix| {
        (6..=64).contains(&suffix.len()) && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
    })
}

pub async fn use_expert(account: &GrowthAccount, expert: &ObjectId) -> Result<(), String> {
    if account.uid.trim().is_empty() || !valid_id(&expert.id) {
        return Err("账号或专家身份缺失，无法开始对话".to_string());
    }
    let request = async {
        let session = json!({
            "account": { "uid": account.uid, "nickname": account.nick },
            "auth": { "accessToken": account.token, "domain": account.domain },
        });
        let region = super::super::Region::Cn;
        let mut headers = super::super::chat_headers(&session, region, Some("text/event-stream"));
        headers.push(("X-Expert-Id".to_string(), expert.id.clone()));
        let conversation_id = headers.iter().find(|(key, _)| key == "X-Conversation-ID")
            .map(|(_, value)| value.clone()).ok_or_else(|| "专家对话缺少会话身份".to_string())?;
        let body = json!({
            // 不带 `agent: "cli"`：那个字段让上游按 CLI agent 处理，返回的是
            // 一串 `tool_calls`（模型想执行 Bash 命令）而不是自然语言答复，
            // 实测 173 个 tool_calls 分片、content 为空、finish_reason 为
            // `tool_calls`。本任务要的是「与专家对话一次」，走普通 chat 形态。
            "model": MODEL, "stream": true, "max_tokens": 512,
            "stream_options": { "include_usage": true },
            "messages": [
                { "role": "system", "content": "请使用简体中文回答。" },
                { "role": "user", "content": PROMPT },
            ],
        });
        let client = crate::http::stream_client(account.proxy.as_ref(), "direct");
        let response = crate::http::post_stream(
            &client, &super::super::chat_completions_url(&session, region), &headers, &body,
            "text/event-stream",
        ).await?;
        if !response.is_success() {
            return Err(format!("专家对话返回 HTTP {}：{}", response.status, response.error_head()));
        }
        let raw = response.response.ok_or_else(|| "专家对话未返回响应流".to_string())?;
        let answer = read_answer(raw).await?;
        Ok((conversation_id, answer))
    };
    let (conversation_id, answer) = tokio::time::timeout(std::time::Duration::from_secs(45), request).await
        .map_err(|_| "专家对话超时，本轮未确认完成".to_string())??;
    tokio::time::sleep(std::time::Duration::from_millis(super::GROWTH_WRITE_GAP_MS)).await;
    report_use(account, expert, &conversation_id, &answer).await
}

async fn report_use(
    account: &GrowthAccount,
    expert: &ObjectId,
    conversation_id: &str,
    answer: &Answer,
) -> Result<(), String> {
    let name = text(&expert.meta, "displayName");
    let title = text(&expert.meta, "profession");
    let mut events = super::events::desktop_chat_sequence(account, conversation_id, &answer.id, &answer.id);
    for event in &mut events {
        if event.get("messageId").is_some() { event["messageId"] = json!(answer.id); }
        match text(event, "eventCode") {
            "agent_task_created" => {
                event["has_expert"] = json!(true);
                event["expert_id"] = json!(expert.id);
                event["expert_name"] = json!(name);
                event["expert_industry_id"] = json!(text(&expert.meta, "category"));
            }
            "chat_request_send" => {
                event["inputLength"] = json!(PROMPT.chars().count());
                event["expertId"] = json!(expert.id);
            }
            "chat_message_response" | "chat_request_response" => {
                if let Some(object) = event.as_object_mut() {
                    for key in ["inputToken", "outputToken", "totalToken", "cachedTokens", "cachedWriteTokens", "cachedMissTokens"] {
                        object.remove(key);
                    }
                }
                event["finishReason"] = json!(answer.finish_reason);
                if text(event, "eventCode") == "chat_message_response" {
                    event["messageId"] = json!(answer.id);
                    event["responseModelId"] = json!(answer.model);
                }
                if let Some(usage) = answer.usage.as_object() {
                    for (source, target) in [("prompt_tokens", "inputToken"), ("completion_tokens", "outputToken"), ("total_tokens", "totalToken")] {
                        if let Some(value) = usage.get(source).filter(|value| value.as_u64().is_some()) {
                            event[target] = value.clone();
                        }
                    }
                }
            }
            _ => {}
        }
    }
    super::events::report_desktop_events(account, &events).await?;
    tokio::time::sleep(std::time::Duration::from_millis(super::GROWTH_WRITE_GAP_MS)).await;
    let mut used = json!({
        "eventCode": "expert_actual_use", "timestamp": crate::logging::now_ms(), "reportDelay": 0,
        "mode": "LOCAL", "id": expert.id, "name": name, "expertTitle": title,
        "type": text(&expert.meta, "category"), "expertType": text(&expert.meta, "expertType"),
        "source": "builtin", "version": text(&expert.meta, "version"),
        "characterCount": PROMPT.chars().count(), "conversationId": conversation_id,
        "requestId": answer.id, "messageId": answer.id, "requestModelId": answer.model,
        "requestModelName": answer.model, "userId": account.uid,
    });
    if let Some(cost) = answer.usage.get("cost").filter(|value| {
        value.as_f64().is_some_and(|cost| cost.is_finite() && cost >= 0.0)
    }) {
        used["cost"] = cost.clone();
    }
    super::events::report_regular(account, &used).await
}

async fn read_answer(response: reqwest::Response) -> Result<Answer, String> {
    let mut stream = response.bytes_stream();
    let mut decoder = crate::sse_events::EventDecoder::default();
    let mut received = 0_usize;
    let mut answer = Answer::default();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("专家对话响应读取失败：{error}"))?;
        received = received.saturating_add(chunk.len());
        if received > 1024 * 1024 { return Err("专家对话响应超过读取上限".to_string()); }
        decoder.feed(&chunk, |event| answer.observe(event));
        if answer.error.is_some() || answer.done { break; }
    }
    decoder.finish(|event| answer.observe(event));
    if let Some(error) = answer.error.take() { return Err(error); }
    // 完成判据只看「拿到过非空文本」：`finish_reason` 缺席（有的网关只在
    // 最后一帧给）不代表失败，但一个字的正文都没有则一定是没聊成。
    if !answer.content {
        return Err("专家对话未返回完整答复，本轮未确认完成".to_string());
    }
    if !valid_message_id(&answer.id) || answer.model.is_empty() {
        return Err("专家对话缺少服务端消息身份或模型信息，不上报使用记录".to_string());
    }
    Ok(answer)
}

#[derive(Default)]
struct Answer {
    id: String,
    model: String,
    usage: Value,
    content: bool,
    finish_reason: String,
    done: bool,
    error: Option<String>,
}

impl Answer {
    fn observe(&mut self, event: &[u8]) -> bool {
        if crate::sse_events::is_done(event) { self.done = true; return true; }
        let Ok(value) = serde_json::from_slice::<Value>(event) else {
            self.error = Some("专家对话返回无法识别的流数据".to_string());
            return true;
        };
        if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
            self.error = Some(format!("专家对话失败：{}", text(error, "message")));
            return true;
        }
        if let Some(id) = value.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()) {
            if !self.id.is_empty() && self.id != id {
                self.error = Some("专家对话响应混有不同的服务端消息身份".to_string());
                return true;
            }
            self.id = id.to_string();
        }
        if let Some(model) = value.get("model").and_then(Value::as_str).filter(|model| !model.is_empty()) {
            self.model = model.to_string();
        }
        if let Some(usage) = value.get("usage").filter(|value| value.is_object()) { self.usage = usage.clone(); }
        for choice in value.get("choices").and_then(Value::as_array).into_iter().flatten() {
            self.content |= choice.pointer("/delta/content").and_then(Value::as_str)
                .is_some_and(|text| !text.trim().is_empty());
            // `finish_reason` 只做记录、不做否决：上游对同一条请求可能给
            // `stop` / `length` / `tool_calls` / `content_filter` 等多种值，
            // 早期把非 stop/length 一律判成失败，会让「模型正常答完但带了
            // 工具意图」的响应整条作废。真正的完成判据是下面
            // 「有文本内容」这一条，`finish_reason` 只用于上报事件。
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                if !reason.is_empty() {
                    self.finish_reason = reason.to_string();
                }
            }
        }
        false
    }
}

fn valid_message_id(id: &str) -> bool {
    // 上游的 `id` 是纯 32 位小写 hex（uuid7 去掉连字符），实测形如
    // `01a16ec189bc78d583f7e808213b066f`。这里刻意**不要求** `cmb-` 前缀：
    // 那个前缀是早期凭猜测加的，上游从不发它，加上等于把所有真实响应判成
    // 「缺少服务端消息身份」而整条任务失败（实测 5 条专家任务全挂在这）。
    // 判据放宽到「非空、长度合理、只含 id 允许的字符」，既能挡住空值与
    // 异常内容，又不会把合法响应误杀。
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("").trim()
}
