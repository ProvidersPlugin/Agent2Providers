//! 国际版 OAuth → z/login → 默认项目 zcode-api-key → apiKey.secretKey。
//! 原生 Region::Intl 要求完整 secret；不能照国内版回退成单段 API Key。
//! 一次轮询只执行一个阶段，OAuth/biz 令牌仅存于调用方的 flow 隔离状态。
use super::{
    account,
    network::{self, text, BIZ, ID_PREFIX},
};
use agent2api_wasm_sdk::{json, Error, Result, Value};

const NAME: &str = "zcode-api-key";

pub fn credential(token: &str) -> Result<()> {
    if token.is_empty() || token.len() > 32 * 1024 || token.contains(['\r', '\n', '\0']) {
        return Err(Error::http(502, "ZCode 国际版上游凭据缺失或格式无效"));
    }
    Ok(())
}

fn request(
    method: &str,
    path: &str,
    authorization: Option<&str>,
    body: Option<&Value>,
) -> Result<Value> {
    let mut headers = json!({ "Content-Type": "application/json" });
    if let Some(authorization) = authorization {
        headers["Authorization"] = json!(authorization);
    }
    let reply =
        network::request(method, &format!("{BIZ}{path}"), headers, body).map_err(|mut error| {
            // copy/{apiKey} 的 URL 含密钥；宿主网络诊断可能带原 URL，不能回显登录错误。
            if error.code.as_deref() != Some("forbidden") {
                error.message = "ZCode 国际版编码密钥接口连接失败或响应中断，请重新登录核对".into();
            }
            error
        })?;
    if !(200..300).contains(&reply.status) {
        return Err(Error::http(
            if matches!(reply.status, 401 | 403) {
                401
            } else {
                502
            },
            format!("ZCode 国际版编码密钥换取失败（HTTP {}）", reply.status),
        ));
    }
    if !reply.value.is_object() && !reply.value.is_array() {
        return Err(Error::http(502, "ZCode 国际版密钥接口返回无效数据"));
    }
    if let Some(code) = reply
        .value
        .get("code")
        .or_else(|| reply.value.get("status"))
    {
        let ok = network::integer(code).is_some_and(|code| matches!(code, 0 | 200))
            || code.as_str() == Some("");
        if !ok {
            return Err(Error::http(502, "ZCode 国际版编码密钥换取被拒"));
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
    if state["phase"] == "business-login" {
        let oauth = text(state, &["oauth"]);
        credential(&oauth)?;
        // z/login 不带 Authorization，空头也不发送。
        let data = request(
            "POST",
            "/api/auth/z/login",
            None,
            Some(&json!({ "token": oauth })),
        )?;
        let mut token = text(&data, &["access_token", "accessToken"]);
        if token.is_empty() {
            token = text(&data["data"], &["access_token", "accessToken"]);
        }
        credential(&token)?;
        state["bizToken"] = json!(token);
        state["oauth"] = Value::Null;
        state["phase"] = json!("organization");
        return Ok(None);
    }
    let biz = text(state, &["bizToken"]);
    credential(&biz)?;
    let authorization = format!("Bearer {biz}");
    match state["phase"].as_str().unwrap_or("") {
        "organization" => {
            let data = request(
                "GET",
                "/api/biz/customer/getCustomerInfo",
                Some(&authorization),
                None,
            )?;
            let orgs = data["organizations"]
                .as_array()
                .or_else(|| data["orgs"].as_array())
                .ok_or_else(|| Error::http(502, "ZCode 国际版账号没有可用机构"))?;
            // 默认名称片段及回退首项与原生国际分支一致，不自行假设英语字段值。
            let org = orgs
                .iter()
                .find(|item| text(item, &["organizationName", "name"]).contains("默认机构"))
                .or_else(|| orgs.first())
                .ok_or_else(|| Error::http(502, "ZCode 国际版账号没有可用机构"))?;
            let projects = org["projects"]
                .as_array()
                .ok_or_else(|| Error::http(502, "ZCode 国际版默认机构没有可用项目"))?;
            let project = projects
                .iter()
                .find(|item| text(item, &["projectName", "name"]).contains("默认项目"))
                .or_else(|| projects.first())
                .ok_or_else(|| Error::http(502, "ZCode 国际版默认机构没有可用项目"))?;
            let org_id = text(org, &["organizationId", "id", "orgId"]);
            let project_id = text(project, &["projectId", "id"]);
            if [&org_id, &project_id].iter().any(|id| {
                id.is_empty()
                    || id.len() > 256
                    || matches!(id.as_str(), "." | "..")
                    || id.chars().any(char::is_control)
            }) {
                return Err(Error::http(502, "ZCode 国际版默认机构或项目标识缺失或无效"));
            }
            state["orgId"] = json!(org_id);
            state["projectId"] = json!(project_id);
            state["phase"] = json!("list-key");
        }
        "list-key" => {
            let data = request("GET", &keys_path(state), Some(&authorization), None)?;
            let items = data
                .as_array()
                .or_else(|| data["list"].as_array())
                .or_else(|| data["apiKeys"].as_array())
                .ok_or_else(|| Error::http(502, "ZCode 国际版密钥列表格式无效，未创建重复密钥"))?;
            let existing = items
                .iter()
                .find(|item| item["name"] == NAME && !text(item, &["apiKey"]).is_empty())
                .map(|item| text(item, &["apiKey"]));
            if let Some(key) = existing {
                credential(&key)?;
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
                Some(&authorization),
                Some(&json!({ "name": NAME })),
            )?;
            let key = text(&data, &["apiKey"]);
            credential(&key)?;
            state["apiKey"] = json!(key);
            state["phase"] = json!("copy-secret");
        }
        "copy-secret" => {
            let key = text(state, &["apiKey"]);
            credential(&key)?;
            if matches!(key.as_str(), "." | "..") {
                return Err(Error::http(502, "ZCode 国际版密钥标识无效"));
            }
            let data = request(
                "GET",
                &format!("{}/copy/{}", keys_path(state), network::query(&key)),
                Some(&authorization),
                None,
            )?;
            let secret = text(&data, &["secretKey", "secret_key"]);
            if secret.is_empty() {
                return Err(Error::http(502, "ZCode 国际版未返回密钥 secret，不能保存半条推理凭据；请在 Z.AI 控制台确认编码套餐已开通后重试"));
            }
            credential(&secret)?;
            let access = format!("{key}.{secret}");
            credential(&access)?;
            let mut account = json!({ "accessToken": access, "oauthAccessToken": biz, "jwt": state["jwt"],
                "deviceMid": state["deviceMid"], "userId": state["userId"], "idPrefix": ID_PREFIX,
                "zcodePlan": account::CODING });
            let name = text(state, &["name"]);
            if !name.is_empty() {
                account["name"] = json!(name);
            }
            let email = text(state, &["email"]);
            if !email.is_empty() {
                account["email"] = json!(email);
            }
            return Ok(Some(account));
        }
        _ => return Err(Error::new("ZCode 国际版登录流程阶段无效")),
    }
    Ok(None)
}
