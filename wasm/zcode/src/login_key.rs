//! 国内版 OAuth 登录态 → 默认项目名为 zcode-api-key 的推理密钥。
//! 每次轮询只做一个阶段；已发送的创建请求结果不明时停止，不重复创建密钥。
use super::{
    account,
    network::{self, text, BIZ},
};
use agent2api_wasm_sdk::{json, Error, Result, Value};

const NAME: &str = "zcode-api-key";

fn request(method: &str, path: &str, oauth: &str, body: Option<&Value>) -> Result<Value> {
    let reply = network::request(
        method,
        &format!("{BIZ}{path}"),
        json!({ "Authorization": oauth, "Content-Type": "application/json" }),
        body,
    )?;
    if !(200..300).contains(&reply.status) {
        return Err(Error::http(
            reply.status,
            format!("ZCode 编码密钥换取失败（HTTP {}）", reply.status),
        ));
    }
    if !reply.value.is_object() && !reply.value.is_array() {
        return Err(Error::http(502, "ZCode 编码密钥接口返回无效数据"));
    }
    if let Some(code) = reply
        .value
        .get("code")
        .or_else(|| reply.value.get("status"))
    {
        let ok = network::integer(code).is_some_and(|code| matches!(code, 0 | 200))
            || code.as_str() == Some("");
        if !ok {
            return Err(Error::http(502, "ZCode 编码密钥换取被拒"));
        }
    }
    Ok(reply.value.get("data").cloned().unwrap_or(reply.value))
}

fn keys_path(state: &Value) -> String {
    format!(
        "/api/biz/v1/organization/{}/projects/{}/api_keys",
        network::query(&text(state, &["orgId"])),
        network::query(&text(state, &["projectId"]))
    )
}

pub fn advance(state: &mut Value) -> Result<Option<Value>> {
    let oauth = text(state, &["oauth"]);
    if oauth.is_empty() {
        return Err(Error::http(401, "ZCode 登录流程缺少授权凭据"));
    }
    match state["phase"].as_str().unwrap_or("") {
        "organization" => {
            let data = request("GET", "/api/biz/customer/getCustomerInfo", &oauth, None)?;
            let orgs = data["organizations"]
                .as_array()
                .or_else(|| data["orgs"].as_array())
                .ok_or_else(|| Error::http(502, "ZCode 账号没有可用机构"))?;
            let org = orgs
                .iter()
                .find(|item| text(item, &["organizationName", "name"]).contains("默认机构"))
                .or_else(|| orgs.first())
                .ok_or_else(|| Error::http(502, "ZCode 账号没有可用机构"))?;
            let projects = org["projects"]
                .as_array()
                .ok_or_else(|| Error::http(502, "ZCode 默认机构没有可用项目"))?;
            let project = projects
                .iter()
                .find(|item| text(item, &["projectName", "name"]).contains("默认项目"))
                .or_else(|| projects.first())
                .ok_or_else(|| Error::http(502, "ZCode 默认机构没有可用项目"))?;
            let org_id = text(org, &["organizationId", "id", "orgId"]);
            let project_id = text(project, &["projectId", "id"]);
            if org_id.is_empty() || project_id.is_empty() {
                return Err(Error::http(502, "ZCode 默认机构或项目标识缺失"));
            }
            state["orgId"] = json!(org_id);
            state["projectId"] = json!(project_id);
            state["phase"] = json!("list-key");
        }
        "list-key" => {
            let result = request("GET", &keys_path(state), &oauth, None);
            let data = match result {
                Ok(data) => data,
                Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
                // 仅明确没有列表权限或旧接口不存在时沿用创建兼容；超时/未知结果不能冒充空清单。
                Err(error)
                    if error
                        .status
                        .is_some_and(|status| matches!(status, 403 | 404)) =>
                {
                    Value::Null
                }
                Err(error) => return Err(error),
            };
            if !data.is_array() && !data.is_null() {
                return Err(Error::http(502, "ZCode 编码密钥列表格式无效，未创建新密钥"));
            }
            let existing = data
                .as_array()
                .into_iter()
                .flatten()
                .find(|item| item["name"] == NAME && !text(item, &["apiKey"]).is_empty())
                .map(|item| text(item, &["apiKey"]));
            if let Some(key) = existing {
                state["apiKey"] = json!(key);
                state["phase"] = json!("copy-secret");
            } else {
                state["phase"] = json!("create-key");
            }
        }
        "create-key" => {
            let data = request(
                "POST",
                &keys_path(state),
                &oauth,
                Some(&json!({ "name": NAME })),
            )?;
            let key = text(&data, &["apiKey"]);
            if key.is_empty() {
                return Err(Error::http(502, "ZCode 未返回新建的编码套餐密钥"));
            }
            state["apiKey"] = json!(key);
            state["phase"] = json!("copy-secret");
        }
        "copy-secret" => {
            let key = text(state, &["apiKey"]);
            if key.is_empty() {
                return Err(Error::http(502, "ZCode 登录流程缺少编码密钥"));
            }
            let data = match request(
                "GET",
                &format!("{}/copy/{}", keys_path(state), network::query(&key)),
                &oauth,
                None,
            ) {
                Ok(data) => data,
                Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
                // 国内版允许单段密钥；仅明确旧 copy 接口不可用时兼容，不掩盖一次未完成读取。
                Err(error)
                    if error
                        .status
                        .is_some_and(|status| matches!(status, 403 | 404)) =>
                {
                    Value::Null
                }
                Err(error) => return Err(error),
            };
            let secret = text(&data, &["secretKey", "secret_key"]);
            let mut account = json!({ "accessToken": if secret.is_empty() { key } else { format!("{key}.{secret}") },
                "oauthAccessToken": oauth,
                "jwt": state["jwt"], "deviceMid": state["deviceMid"], "userId": state["userId"],
                "name": state["name"], "email": state["email"],
                "idPrefix": "zcode-user-", "zcodePlan": account::CODING });
            let refresh = text(state, &["refreshToken"]);
            if !refresh.is_empty() {
                account["oauthRefreshToken"] = json!(refresh);
            }
            return Ok(Some(account));
        }
        _ => return Err(Error::new("ZCode 登录流程阶段无效")),
    }
    Ok(None)
}
