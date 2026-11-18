//! 只认本轮官方商业限制标记，不将订阅提示正文当作正常回答。

use serde_json::Value;

use crate::errors::GatewayError;

fn object(value: &Value) -> Option<Value> {
    match value {
        Value::Object(_) => Some(value.clone()),
        Value::String(raw) if raw.len() <= 64 * 1024 => serde_json::from_str::<Value>(raw)
            .ok().filter(Value::is_object),
        _ => None,
    }
}

pub(super) fn error(ext: &Value) -> Option<GatewayError> {
    if ext["not_in_chat_context"] != "commerce_intake_block" && ext["expose_scene"] != "commerce-moa-intake_cta" {
        return None;
    }
    let params = object(&ext["inner_paywall_cta_param"]);
    let snapshot = params.as_ref().and_then(|params| object(&params["snapshot"]));
    if snapshot.is_some_and(|snapshot| snapshot["user_tier"] == "free"
        && snapshot["is_free"] == false && snapshot["remaining_free_quota"] == false) {
        Some(GatewayError::with_status(403, "豆包工作官方确认当前账号不能使用此订阅专属模型；这不是登录过期，也不是已确认的请求频率限制")
            .with_code("doubaowork_subscription_required"))
    } else {
        Some(GatewayError::with_status(403, "豆包工作官方限制了当前模型的使用；请在官方客户端核对订阅与模型权限，未将限制提示当作正常回答")
            .with_code("doubaowork_model_restricted"))
    }
}
