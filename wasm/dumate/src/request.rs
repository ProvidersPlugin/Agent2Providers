use agent2api_wasm_sdk::{json, Error, Result, Value};

use crate::{endpoints, models};

pub struct Request {
    pub path: &'static str,
    pub body: Value,
    pub qianfan: bool,
}

pub fn build(params: &Value) -> Result<Request> {
    let mut body = params["body"].clone();
    let fields = body
        .as_object_mut()
        .ok_or_else(|| Error::http(400, "对话内容必须是 JSON 对象"))?;
    let model = fields
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| {
            !model.is_empty() && model.len() <= 256 && !model.chars().any(char::is_control)
        })
        .ok_or_else(|| Error::http(400, "请指定百度搭子的模型名称"))?
        .to_string();
    if model == "dumate-image2.2" {
        return Err(Error::http(400, "文生图模型请使用图片生成入口"));
    }
    let qianfan = model.starts_with("qianfan/");
    let wire = if qianfan {
        model
            .strip_prefix("qianfan/")
            .filter(|model| !model.is_empty())
            .ok_or_else(|| Error::http(400, "请在 qianfan/ 后填写千帆模型名称"))?
            .to_string()
    } else {
        model.clone()
    };
    if !qianfan
        && fields
            .get("response_format")
            .is_some_and(|value| !value.is_null() && value["type"] != "text")
    {
        return Err(Error::http(
            400,
            "百度搭子普通对话接口不支持结构化输出，请使用 qianfan/ 模型或移除 response_format",
        ));
    }
    if !fields
        .get("messages")
        .and_then(Value::as_array)
        .is_some_and(|messages| !messages.is_empty())
    {
        return Err(Error::http(400, "百度搭子对话至少需要一条消息"));
    }
    for key in ["max_tokens", "max_completion_tokens"] {
        if let Some(value) = fields.get(key).filter(|value| !value.is_null()) {
            if !value.as_u64().is_some_and(|number| number > 0) {
                return Err(Error::http(400, "最大输出长度必须是正整数"));
            }
        }
    }
    if fields.get("max_tokens").is_none_or(Value::is_null) {
        if let Some(value) = fields.remove("max_completion_tokens") {
            fields.insert("max_tokens".into(), value);
        }
    } else {
        fields.remove("max_completion_tokens");
    }
    let cap = (!qianfan)
        .then(models::rows)
        .into_iter()
        .flatten()
        .find(|row| row["id"] == model)
        .and_then(|row| row["maxOutputTokens"].as_u64());
    if let Some(cap) = cap {
        if let Some(value) = fields.get_mut("max_tokens") {
            if let Some(number) = value.as_u64() {
                *value = json!(number.min(cap));
            }
        }
    }
    fields.insert("model".into(), json!(wire));
    fields.insert("stream".into(), json!(true));
    let mut options = fields
        .get("stream_options")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    options.insert("include_usage".into(), json!(true));
    fields.insert("stream_options".into(), Value::Object(options));
    Ok(Request {
        path: if qianfan {
            endpoints::QIANFAN_CHAT
        } else {
            endpoints::CHAT
        },
        body,
        qianfan,
    })
}
