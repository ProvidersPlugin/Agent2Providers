//! 只使用本轮响应明确携带的商业限制标记，不按正文关键词推断余额或订阅。

use agent2api_wasm_sdk::{Error, Value};

const MAX_PARAMS: usize = 64 * 1024;

fn object(value: &Value) -> Option<Value> {
    match value {
        Value::Object(_) => Some(value.clone()),
        Value::String(text) if text.len() <= MAX_PARAMS => serde_json::from_str::<Value>(text)
            .ok()
            .filter(Value::is_object),
        _ => None,
    }
}

pub fn error(ext: &Value) -> Option<Error> {
    if ext["not_in_chat_context"] != "commerce_intake_block"
        && ext["expose_scene"] != "commerce-moa-intake_cta"
    {
        return None;
    }
    let params = object(&ext["inner_paywall_cta_param"]);
    let snapshot = params.as_ref().and_then(|params| object(&params["snapshot"]));
    if snapshot.as_ref().is_some_and(|snapshot| {
        snapshot["user_tier"] == "free"
            && snapshot["is_free"] == false
            && snapshot["remaining_free_quota"] == false
    }) {
        Some(Error::http(403, "豆包工作官方确认当前账号不能使用此订阅专属模型；这不是登录过期，也不是已确认的请求频率限制")
            .with_code("doubaowork_subscription_required"))
    } else {
        Some(Error::http(403, "豆包工作官方限制了当前模型的使用；请在官方客户端核对订阅与模型权限，未将限制提示当作正常回答")
            .with_code("doubaowork_model_restricted"))
    }
}
