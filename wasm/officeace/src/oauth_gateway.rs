//! 静态控制面获取身份和模型访问凭据；不访问动态模型网关，因此登录不需动态来源权限。

use super::{client, credentials::Credentials, endpoints, signer::Algorithm, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub struct Permission {
    pub account: String,
    pub principal: String,
    pub model: Value,
}

pub fn permission(credential: &Credentials) -> Result<Permission> {
    let value = client::cloud(
        credential,
        "GET",
        "/v1/claw/client-permission-validate",
        None,
        true,
    )?;
    Ok(Permission {
        account: util::text(&value, &["account_id"]),
        principal: util::text(&value, &["principal_id"]),
        model: value
            .get("model_info")
            .or_else(|| value.pointer("/subscription/model_info"))
            .or_else(|| value.get("modelInfo"))
            .filter(|value| value.is_object())
            .cloned()
            .unwrap_or(Value::Null),
    })
}

pub fn project(credential: &Credentials) -> Result<String> {
    let value = client::signed(
        credential,
        "GET",
        &format!("{}/v3/projects", endpoints::IAM),
        Algorithm::Sdk,
        None,
        json!({}),
    )?;
    value["projects"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|row| row["name"] == endpoints::REGION && row["enabled"] != false)
        })
        .map(|row| util::text(row, &["id"]))
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            Error::http(
                409,
                "果办所在区域尚未初始化，请先在官方客户端登录；这里不会创建云资源",
            )
        })
}

pub fn has_model(model: &Value) -> bool {
    let auth = &model["model_auth_info"];
    !util::text(auth, &["model_app_key"]).is_empty()
        && !util::text(auth, &["model_app_secret"]).is_empty()
        && !util::text(model, &["model_api_url_base"]).is_empty()
}

pub fn apply_model(secret: &mut Value, model: &Value) -> Result<()> {
    let auth = &model["model_auth_info"];
    let key = util::text(auth, &["model_app_key"]);
    let password = util::text(auth, &["model_app_secret"]);
    if key.is_empty() || password.is_empty() {
        return Err(Error::http(502, "果办返回的模型信息缺少网关 Basic 凭据"));
    }
    let raw = util::text(model, &["model_api_url_base"]);
    let raw = if raw.contains("://") {
        raw
    } else {
        format!("https://{raw}")
    };
    let base = endpoints::official_base(&raw)?;
    secret["baseUrl"] = json!(base);
    secret["modelAppKey"] = json!(key);
    secret["modelAppSecret"] = json!(password);
    Ok(())
}

/// 仅在当前登录流程获得明确确认后调用；这是取得模型访问凭据，不是零元订单。
/// 不携带协议同意字段、推广码、价格或续费参数；套餐另由 activation 严格验价处理。
pub fn bind_model(credential: &Credentials) -> Result<Value> {
    let value = client::cloud(
        credential,
        "POST",
        "/v1/claw/client-subscription",
        Some(&json!({})),
        true,
    )?;
    value
        .get("model_info")
        .filter(|value| has_model(value))
        .cloned()
        .ok_or_else(|| {
            Error::http(
                502,
                "模型访问初始化结果尚未确认，请在官方客户端查看后重新登录",
            )
            .with_code(super::errors::UNCERTAIN)
        })
}
