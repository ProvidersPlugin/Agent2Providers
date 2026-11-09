use serde_json::{json, Value};

pub(super) const INSTRUCTIONS: &str = "本机部署会自动完成登录；服务器或 Docker 部署时，若授权后跳到无法访问的 localhost 或 127.0.0.1 页面，请把地址栏中的完整回调地址粘贴回来。AWS 登录如需继续授权，请打开下一步授权页，完成后再次回填。请勿修改地址或把回调链接发给他人。";
pub(super) const WAIT_HINT: &str =
    "等待浏览器授权；若本机回调页面无法打开，请复制地址栏中的完整地址回填。";
pub(super) const AWS_HINT: &str =
    "还需完成 AWS 授权：请打开下一步授权页，完成后回填新的完整回调地址。";

pub(super) fn methods() -> Value {
    json!({
        "methods": [
            {
                "methodIndex": 0, "type": "oauth", "label": "Kiro 网页登录",
                "instructions": INSTRUCTIONS,
                "prompts": [
                    {
                        "key": "loginMethod", "type": "select", "label": "登录方式",
                        "default": "portal", "options": [
                            { "value": "portal", "label": "Kiro 登录页（Google / GitHub 等）" },
                            { "value": "builderid", "label": "AWS Builder ID（直接授权，减少一次回填）" },
                            { "value": "enterprise", "label": "IAM Identity Center（企业账号，直接授权）" }
                        ]
                    },
                    {
                        "key": "startUrl", "type": "text", "label": "组织登录地址（Start URL）",
                        "placeholder": "https://d-xxxxxxxxxx.awsapps.com/start", "optional": false,
                        "when": { "key": "loginMethod", "op": "eq", "value": "enterprise" }
                    },
                    {
                        "key": "region", "type": "text", "label": "AWS 地区（账号目录所在地区）",
                        "placeholder": "us-east-1", "default": "us-east-1", "optional": true,
                        "when": { "key": "loginMethod", "op": "eq", "value": "enterprise" }
                    }
                ],
                "localOnly": false, "manualCallback": true, "callbackInput": "url",
                "validatesCallback": true, "browserMode": "external"
            },
            {
                "methodIndex": 1, "type": "oauth", "label": "导入 kiro-cli / Kiro IDE 登录态",
                "instructions": "读取网关所在机器上 kiro-cli 或 Kiro IDE 的登录态，不是当前浏览器所在电脑的账号；续期后会写回原处。",
                "prompts": [], "localOnly": true, "manualCallback": false, "callbackInput": "url"
            },
            {
                "methodIndex": 2, "type": "api", "label": "Kiro API key",
                "instructions": "粘贴 ksk_ 开头的 API key。",
                "prompts": [{ "key": "apiKey", "type": "text", "label": "API key",
                    "placeholder": "ksk_…", "secret": true, "optional": false }],
                "localOnly": false, "manualCallback": false, "callbackInput": "url"
            }
        ]
    })
}
