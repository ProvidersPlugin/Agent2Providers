//! 指定专家任务是真实对话；任务链接只能提取身份，绝不携凭据访问任务给出的URL。
use super::{spec::ObjectId, GrowthAccount};
use crate::{headers, network, util};
use agent2api_wasm_sdk::{json, protocol::Converter, sse::SseDecoder, Value};

const PROMPT: &str = "请用一句话介绍你的专长。";
fn valid_id(id: &str) -> bool {
    id.strip_prefix("ex_").is_some_and(|s| {
        (6..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric())
    })
}
fn expert_id(link: &str) -> Result<Option<String>, String> {
    if link.is_empty() {
        return Ok(None);
    }
    let mut url = url::Url::parse(link).map_err(|_| "任务专家链接无法解析".to_string())?;
    if matches!(url.scheme(), "http" | "https") {
        let host = url.host_str().unwrap_or("");
        if !["workbuddy.cn", "codebuddy.cn", "tencent.com"]
            .iter()
            .any(|d| host == *d || host.ends_with(&format!(".{d}")))
        {
            return Err("专家链接不属于WorkBuddy".into());
        }
        let links: Vec<_> = url
            .query_pairs()
            .filter(|(k, _)| k == "deeplink")
            .map(|(_, v)| v.into_owned())
            .collect();
        if links.is_empty() {
            return Ok(None);
        }
        if links.len() != 1 {
            return Err("任务包含重复专家入口".into());
        }
        url = url::Url::parse(&links[0]).map_err(|_| "专家入口无效".to_string())?;
    }
    if url.scheme() != "workbuddy"
        || !matches!(url.host_str(), Some("expert" | "experts"))
        || !matches!(url.path(), "" | "/")
        || url.fragment().is_some()
    {
        return Err("任务专家入口无法确认".into());
    }
    let ids: Vec<_> = url
        .query_pairs()
        .filter(|(k, _)| k == "expertId")
        .map(|(_, v)| v.into_owned())
        .collect();
    if ids.is_empty() {
        return Ok(None);
    }
    if ids.len() != 1 || !valid_id(&ids[0]) {
        return Err("任务专家身份无效".into());
    }
    Ok(ids.into_iter().next())
}
pub fn resolve(account: &GrowthAccount, task: &Value) -> Result<ObjectId, String> {
    let id = expert_id(&util::text(task, &["jump_url"]))?;
    let code = util::text(task, &["task_code"]);
    let keyword = match code.as_str() {
        "expert_use_maidehao" => "中国电商运营专家",
        "expert_use_douyinyin" => "抖音策略师",
        "expert_use_shangjia_xuanchuan" => "商家宣传页与海报设计师",
        "expert_use_bowangwang" => "直播电商教练",
        "WorkBuddy_Valorant" => "无畏契约拼豆专家",
        _ => return Err("未知指定专家任务".into()),
    };
    if id.is_none() && code != "WorkBuddy_Valorant" {
        return Err("任务没有指定专家，请到官方客户端完成".into());
    }
    let matches: Vec<_> = super::spec::fetch_market_experts(account, false, Some(keyword))
        .into_iter()
        .filter(|v| {
            if let Some(id) = &id {
                v.id == *id
            } else {
                valid_id(&v.id) && ["name", "profession"].iter().any(|k| v.meta[*k] == keyword)
            }
        })
        .collect();
    if matches.len() != 1 {
        return Err("专家市场未唯一确认任务对象".into());
    }
    let expert = matches.into_iter().next().ok_or("专家未返回")?;
    if util::text(&expert.meta, &["displayName"]).is_empty()
        || util::text(&expert.meta, &["profession"]).is_empty()
    {
        return Err("专家昵称或职业信息不全".into());
    }
    Ok(expert)
}
pub fn use_expert(account: &GrowthAccount, expert: &ObjectId) -> Result<(), String> {
    account.check()?;
    let mut headers = headers::chat(&account.session.secret).map_err(|e| e.message)?;
    headers["X-Expert-Id"] = json!(expert.id);
    let conversation = util::text(&headers, &["X-Conversation-ID"]);
    let body = json!({ "model": "fast-model", "stream": true, "max_tokens": 512, "stream_options": { "include_usage": true },
        "messages": [{ "role": "system", "content": "请使用简体中文回答。" }, { "role": "user", "content": PROMPT }] });
    let mut response = network::open(
        "POST",
        &account.session.endpoint().map_err(|e| e.message)?,
        "/v2/chat/completions",
        headers,
        Some(&body),
    )
    .map_err(|e| e.message)?;
    if !(200..300).contains(&response.status) {
        return Err(format!("专家对话HTTP {}", response.status));
    }
    let mut converter = Converter::new("chat", "fast-model").map_err(|e| e.message)?;
    let mut decoder = SseDecoder::default();
    let mut answer = Answer::default();
    let mut bytes_total = 0;
    while let Some(bytes) = response.read().map_err(|e| e.message)? {
        account.check()?;
        bytes_total += bytes.len();
        if bytes_total > 1024 * 1024 {
            return Err("专家对话超过1MiB护栏".into());
        }
        for event in decoder
            .push(&converter.push(&bytes).map_err(|e| e.message)?)
            .map_err(|e| e.message)?
        {
            answer.observe(&event, account)?;
        }
        if answer.done {
            break;
        }
    }
    let tail = converter.finish().map_err(|e| e.message)?;
    let mut events = decoder.push(&tail).map_err(|e| e.message)?;
    events.extend(decoder.push(b"\n\n").map_err(|e| e.message)?);
    for event in events {
        answer.observe(&event, account)?;
    }
    if !answer.done
        || !answer.content
        || answer.id.is_empty()
        || answer.model.is_empty()
        || answer.finish.is_empty()
    {
        return Err("专家对话未完整返回可确认的答复，未上报使用记录".into());
    }
    account.pause(super::GROWTH_WRITE_GAP_MS)?;
    let mut events =
        super::events::desktop_chat_sequence(account, &conversation, &answer.id, &answer.id);
    for event in &mut events {
        if event.get("messageId").is_some() {
            event["messageId"] = json!(answer.id);
        }
        match event["eventCode"].as_str().unwrap_or("") {
            "agent_task_created" => {
                event["has_expert"] = json!(true);
                event["expert_id"] = json!(expert.id);
                event["expert_name"] = expert.meta["displayName"].clone();
                event["expert_industry_id"] = expert.meta["category"].clone();
            }
            "chat_request_send" => {
                event["inputLength"] = json!(PROMPT.chars().count());
                event["expertId"] = json!(expert.id);
            }
            "chat_message_response" | "chat_request_response" => {
                if let Some(fields) = event.as_object_mut() {
                    for k in [
                        "inputToken",
                        "outputToken",
                        "totalToken",
                        "cachedTokens",
                        "cachedWriteTokens",
                        "cachedMissTokens",
                    ] {
                        fields.remove(k);
                    }
                }
                event["finishReason"] = json!(answer.finish);
                if event["eventCode"] == "chat_message_response" {
                    event["responseModelId"] = json!(answer.model);
                }
                for (from, to) in [
                    ("prompt_tokens", "inputToken"),
                    ("completion_tokens", "outputToken"),
                    ("total_tokens", "totalToken"),
                ] {
                    if let Some(v) = answer.usage.get(from).filter(|v| v.as_u64().is_some()) {
                        event[to] = v.clone();
                    }
                }
            }
            _ => {}
        }
    }
    super::events::report_desktop_events(account, &events)?;
    account.pause(super::GROWTH_WRITE_GAP_MS)?;
    let mut event = json!({ "eventCode": "expert_actual_use", "timestamp": agent2api_wasm_sdk::now(), "reportDelay": 0,
        "mode": "LOCAL", "id": expert.id, "name": expert.meta["displayName"], "expertTitle": expert.meta["profession"], "type": expert.meta["category"],
        "expertType": expert.meta["expertType"], "source": "builtin", "version": expert.meta["version"], "characterCount": PROMPT.chars().count(),
        "conversationId": conversation, "requestId": answer.id, "messageId": answer.id, "requestModelId": answer.model, "requestModelName": answer.model, "userId": account.uid });
    if let Some(cost) = util::number(&answer.usage["cost"]).filter(|v| *v >= 0.0) {
        event["cost"] = json!(cost);
    }
    super::events::report_regular(account, &event)
}
#[derive(Default)]
struct Answer {
    id: String,
    model: String,
    finish: String,
    content: bool,
    done: bool,
    usage: Value,
}
impl Answer {
    fn observe(&mut self, event: &str, account: &GrowthAccount) -> Result<(), String> {
        if self.done {
            return Err("专家结束标记后出现额外事件".into());
        }
        if event.trim() == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let value: Value = serde_json::from_str(event).map_err(|_| "专家流无效".to_string())?;
        if crate::errors::failed(200, &value) {
            return Err(crate::errors::upstream(200, &value, &account.session.secrets()).message);
        }
        if let Some(id) = value["id"].as_str().filter(|s| !s.is_empty()) {
            if id.len() > 128
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
                || !self.id.is_empty() && id != self.id
            {
                return Err("专家消息身份不一致".into());
            }
            self.id = id.into();
        }
        if let Some(model) = value["model"].as_str().filter(|s| !s.is_empty()) {
            if model.len() > 256
                || model.chars().any(char::is_control)
                || !self.model.is_empty() && model != self.model
            {
                return Err("专家响应模型前后不一致".into());
            }
            self.model = model.into();
        }
        if value["usage"].is_object() {
            self.usage = value["usage"].clone();
        }
        for choice in value["choices"].as_array().into_iter().flatten() {
            self.content |= choice["delta"]["content"]
                .as_str()
                .is_some_and(|s| !s.trim().is_empty());
            if let Some(finish) = choice["finish_reason"].as_str().filter(|s| !s.is_empty()) {
                self.finish = finish.into();
            }
        }
        Ok(())
    }
}
