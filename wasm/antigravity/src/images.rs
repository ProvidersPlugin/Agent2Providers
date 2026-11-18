//! 标准生成/编辑入口折为同一Gemini image_gen信封，固定sandbox，不另造付费或验证旁路。

use super::{
    account, aliases, client, credentials,
    endpoints::{self, Plane},
    errors, image_config, image_output, outbound, project, refresh,
    sse::Reader,
    util,
};
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub fn generate(params: &Value, editing: bool) -> Result<Value> {
    let body = &params["body"];
    if !body.is_object() {
        return Err(Error::http(400, "Antigravity 图片请求必须是对象"));
    }
    let model = aliases::upstream(
        params["wire_model"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| body["model"].as_str())
            .unwrap_or(""),
    );
    let config = image_config::config(body, &model, editing)?;
    let record = project::ensure(refresh::ensure(false, None)?, true)?;
    let secret = credentials::parse(&record["secret"])?;
    let mut content = vec![json!({ "type": "text", "text": body["prompt"] })];
    content.extend(
        body["images"].as_array().into_iter().flatten().map(
            |image| json!({ "type": "image_url", "image_url": { "url": image["image_url"] } }),
        ),
    );
    let mut chat = json!({ "model": model, "messages": [{ "role": "user" }] });
    chat["messages"][0]["content"] = Value::Array(content);
    let mut envelope = outbound::build(
        &chat,
        &model,
        &util::text(&secret, &["projectId"]),
        credentials::enterprise(&secret),
        None,
    )?;
    drop(chat);
    envelope["request"]["generationConfig"]["responseModalities"] = json!(["TEXT", "IMAGE"]);
    envelope["request"]["generationConfig"]["imageConfig"] = config;
    account::unchanged(&record["secret"])?;
    let response = client::stream(
        &endpoints::stream(endpoints::SANDBOX),
        endpoints::headers(&credentials::token(&secret)?, "", Plane::Content, false)?,
        &envelope,
    )?;
    if !(200..300).contains(&response.status) {
        let reply = client::read(response, 4 * 1024 * 1024)?;
        return Err(errors::upstream(reply.status, &reply.body));
    }
    let mime = response
        .headers
        .as_object()
        .into_iter()
        .flatten()
        .find(|(key, _)| key.eq_ignore_ascii_case("content-type"))
        .and_then(|(_, v)| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if mime.contains("application/json") || mime.contains("text/html") {
        let reply = client::read(response, 4 * 1024 * 1024)?;
        if errors::failed(&reply.body) {
            return Err(errors::upstream(reply.status, &reply.body));
        }
        return Err(image_output::uncertain("没有返回图片事件流"));
    }
    let result = image_output::collect(Reader::new(response, 32 * 1024 * 1024))?;
    account::unchanged(&record["secret"])?;
    Ok(result)
}
