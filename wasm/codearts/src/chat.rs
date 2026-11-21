use crate::{
    binding::Account,
    catalog,
    endpoints::{self, Target},
    errors, signer, tool_names,
};
use agent2api_wasm_sdk::{self as sdk, json, Result, Value};

pub fn forward(mut params: Value) -> Result<Value> {
    let account = Account::current()?;
    account.readable()?;
    let requested = params["wire_model"]
        .as_str()
        .or_else(|| params["body"]["model"].as_str())
        .filter(|model| !model.trim().is_empty() && model.len() <= 512)
        .ok_or_else(|| errors::invalid("请选择 CodeArts 模型"))?
        .to_string();
    if !params["body"]["messages"]
        .as_array()
        .is_some_and(|rows| !rows.is_empty() && rows.len() <= 512)
    {
        return Err(errors::invalid("messages 必须是非空数组"));
    }
    let mut list = catalog::list(&params)?;
    if list["models"].as_array().is_none_or(|rows| {
        !rows.iter().any(|row| {
            row["id"]
                .as_str()
                .is_some_and(|id| id.eq_ignore_ascii_case(&requested))
        })
    }) {
        list = catalog::refresh(&params)?;
    }
    let target_model = list["models"].as_array().into_iter().flatten().find(|row| {
        row["id"]
            .as_str()
            .is_some_and(|id| id.eq_ignore_ascii_case(&requested))
    });
    if target_model.is_none() {
        return Err(errors::invalid("当前账号没有此模型目录，请先获取模型清单"));
    }
    let upstream = target_model
        .and_then(|row| row["providerModel"].as_str().or_else(|| row["id"].as_str()))
        .ok_or_else(|| errors::protocol("CodeArts 目录缺少模型发送名"))?
        .to_string();
    let max_output = target_model.and_then(|row| row["maxOutputTokens"].as_i64());
    let is_benefit = target_model.is_some_and(|row| row["benefit"] == true);
    drop(list);
    let mut body = sdk::take_body(&mut params)
        .filter(Value::is_object)
        .ok_or_else(|| errors::invalid("CodeArts 请求体必须是 JSON 对象"))?;
    drop(params);
    let display = body["model"].as_str().unwrap_or(&requested).to_string();
    body["model"] = json!(upstream);
    body["stream"] = json!(true);
    body["stream_options"] = json!({"include_usage":true});
    // stop 是上游明确拒绝的字段：字符串与数组都会被判成参数非法（InferHub.001001005.400），
    // 删除比改写可靠；不把客户端的无效参数透传给上游。
    if let Some(fields) = body.as_object_mut() {
        fields.remove("stop");
    }
    for key in ["max_tokens", "max_completion_tokens"] {
        if body[key]
            .as_i64()
            .is_some_and(|number| number > endpoints::MAX_OUTPUT)
        {
            body[key] = json!(endpoints::MAX_OUTPUT);
        }
    }
    for key in ["reasoning_effort", "reasoningEffort", "effort"] {
        if body[key].as_str().is_some_and(|value| {
            value.trim().eq_ignore_ascii_case("none") || value.trim().eq_ignore_ascii_case("off")
        }) {
            body.as_object_mut().map(|fields| fields.remove(key));
        }
    }
    // thinking:{type:"disabled"} 是上游已验证可用的开关，不与被拒绝的off/none档位混同。
    if body["enable_thinking"] == false {
        body.as_object_mut()
            .map(|fields| fields.remove("enable_thinking"));
    }
    reserve(&mut body, max_output);
    if body["tool_choice"]["type"] == "function"
        && body["tool_choice"]["function"]["name"]
            .as_str()
            .is_some_and(|name| !name.is_empty())
    {
        body["tool_choice"] = json!("required");
    }
    let restorer = tool_names::shorten(&mut body)?;
    if let Some(rows) = body["messages"].as_array_mut() {
        for row in rows {
            if row["role"] == "assistant" {
                if let Some(row) = row.as_object_mut() {
                    row.remove("reasoning_content");
                    row.remove("reasoningContent");
                    row.remove("reasoning");
                }
            }
        }
    }
    let bytes = serde_json::to_vec(&body).map_err(|_| errors::invalid("CodeArts 请求无法编码"))?;
    drop(body);
    if bytes.len() > 8 * 1024 * 1024 {
        return Err(errors::invalid("CodeArts 请求超过 8 MiB"));
    }
    account.unchanged()?;
    let session = sdk::host("codearts.session.open", json!({}))?;
    let mut started = false;
    let outcome = (|| {
        let id = session["id"]
            .as_str()
            .filter(|id| id.len() == 32)
            .ok_or_else(|| errors::protocol("会话心跳没有返回本次编号"))?;
        let mut headers = vec![
            ("Content-Type".into(), "application/json".into()),
            ("Accept".into(), "text/event-stream".into()),
            (
                "client_version".into(),
                format!("Vscode_{}", endpoints::VERSION),
            ),
            ("Agent-Type".into(), "ChatAgent".into()),
            ("X-Language".into(), endpoints::LANGUAGE.into()),
            ("is_confidential".into(), "false".into()),
            ("plugin-name".into(), endpoints::PLUGIN.into()),
            ("plugin-version".into(), endpoints::VERSION.into()),
            ("model-id".into(), upstream.clone()),
            ("model-name".into(), upstream.clone()),
            ("x-model-id".into(), upstream.clone()),
            ("User-Session-Id".into(), id.into()),
        ];
        if is_benefit {
            headers.push(("maas_type".into(), "benefit".into()));
        }
        let target = Target {
            origin: endpoints::BASE,
            path: "/api/v2/chat/completions".into(),
            query: String::new(),
            label: "对话",
            include_host: false,
            domainless: false,
            headers,
        };
        account.unchanged()?;
        let headers = signer::signed_request("POST", &target, &account.credential, &bytes)?;
        let mut response = sdk::http::open_raw("POST", &target.url(), headers, Some(&bytes))
            .map_err(errors::transport)?;
        if response.status != 200 {
            let status = response.status;
            let payload = response.bytes(256 * 1024).map_err(errors::transport)?;
            let value: Value = serde_json::from_slice(&payload).unwrap_or(Value::Null);
            return Err(errors::classified(
                status,
                &value,
                &account.credential.secrets(),
            ));
        }
        if !response
            .header_values("content-type")
            .iter()
            .any(|value| value.starts_with("text/event-stream"))
        {
            return Err(errors::protocol("CodeArts 没有返回流式回答"));
        }
        let mut decoder = sdk::sse::SseDecoder::default();
        let mut done = false;
        let mut answer = false;
        let mut received = 0usize;
        while let Some(bytes) = response.read().map_err(errors::transport)? {
            if bytes.len() > 64 * 1024 * 1024 - received {
                return Err(errors::limited("CodeArts 输出超过流式大小上限"));
            }
            received += bytes.len();
            for event in decoder.push(&bytes)? {
                if event.trim() == "[DONE]" {
                    done = true;
                    break;
                }
                let mut value: Value = serde_json::from_str(&event)
                    .map_err(|_| errors::protocol("CodeArts 回答事件无效"))?;
                if errors::failure(&value) {
                    return Err(errors::classified(200, &value, &account.credential.secrets()));
                }
                let content_blocked = value["choices"].as_array().is_some_and(|choices| {
                    choices
                        .iter()
                        .any(|choice| choice["finish_reason"] == "content_filter")
                });
                restorer.apply(&mut value);
                let has_answer = errors::has_answer(&value);
                answer |= has_answer;
                if content_blocked && !has_answer {
                    return Err(sdk::Error::http(403, "CodeArts 内容策略拒绝了本次请求")
                        .with_code("codearts_content_blocked"));
                }
                if !value["choices"].is_array() && !value["usage"].is_object() {
                    continue;
                }
                if !started {
                    if !has_answer {
                        continue;
                    }
                    account.unchanged()?;
                    sdk::stream_head(200)?;
                    started = true;
                }
                value["model"] = json!(display);
                sdk::stream_event(&value)?;
                if content_blocked {
                    return Err(sdk::Error::http(403, "CodeArts 内容策略终止了本次回答")
                        .with_code("codearts_content_blocked"));
                }
            }
            if done {
                break;
            }
        }
        if !done || !answer {
            return Err(errors::protocol("CodeArts 回答为空或在完整结束标记前中断"));
        }
        Ok(Value::Null)
    })();
    let cleanup = sdk::host("codearts.session.close", json!({}));
    let result = outcome.and_then(|value| cleanup.map(|_| value));
    match result {
        Ok(value) => {
            sdk::stream_bytes(b"data: [DONE]\n\n")?;
            Ok(value)
        }
        Err(error) => {
            if started {
                let _ = sdk::stream_event(
                    &json!({"error":{"message":error.message,"status":error.status,"code":error.code}}),
                );
            }
            Err(error)
        }
    }
}

fn reserve(body: &mut Value, maximum: Option<i64>) {
    // 与原生同口径：低输出预算会被思考耗尽；显式关闭思考时不增加客户预算。
    if body["thinking"]["type"] == "disabled" || body["reasoning"]["enabled"] == false {
        return;
    }
    let Some(key) = ["max_tokens", "max_completion_tokens"]
        .into_iter()
        .find(|key| body[*key].as_i64().is_some())
    else {
        return;
    };
    let Some(client) = body[key]
        .as_i64()
        .filter(|value| *value > 0 && *value < 1024)
    else {
        return;
    };
    let next = client.saturating_add(1024).min(
        maximum
            .filter(|maximum| *maximum > 0)
            .unwrap_or(endpoints::MAX_OUTPUT),
    );
    if next > client {
        body[key] = json!(next);
    }
}
