use serde_json::{json, Value};

pub(super) fn methods() -> Value {
    json!({ "methods": [
        { "methodIndex": 0, "type": "oauth", "label": "网页登录",
          "prompts": [], "manualCallback": true, "callbackInput": "url", "browserMode": "external" },
        { "methodIndex": 1, "type": "api", "label": "填写凭证", "prompts": [
            { "key": "refreshToken", "type": "text", "label": "Google refresh token", "placeholder": "1//…", "secret": true },
            { "key": "name", "type": "text", "label": "备注名", "placeholder": "可选", "optional": true }
        ] }
    ] })
}
