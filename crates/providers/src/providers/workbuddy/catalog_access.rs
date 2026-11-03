//! 企业模型授权按账号身份保存；过期或身份不符的目录不能用于授权。

use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::{catalog, catalog_state, region::Region};
use crate::errors::GatewayError;

const LEDGER: &str = "workbuddy-model-permissions";
const TTL_MS: i64 = 30 * 60_000;

pub(super) fn authority(session: &Value) -> Value {
    let mut value = serde_json::Map::new();
    for key in [
        "uid",
        "type",
        "enterpriseId",
        "oneidAccountId",
        "departmentFullName",
        "idp",
    ] {
        if let Some(field) = session.get("account").and_then(|account| account.get(key)) {
            value.insert(key.into(), field.clone());
        }
    }
    for key in ["domain", "tokenType"] {
        if let Some(field) = session.get("auth").and_then(|auth| auth.get(key)) {
            value.insert(key.into(), field.clone());
        }
    }
    let region = Region::from_edition_id(session.get("edition").and_then(Value::as_str));
    value.insert(
        "endpoint".into(),
        json!(super::session_endpoint_of(session, region)),
    );
    value.insert("region".into(), json!(region.id()));
    Value::Object(value)
}

pub(super) fn enterprise(session: &Value) -> bool {
    session
        .pointer("/account/enterpriseId")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.trim().is_empty())
}

pub(super) struct Access {
    pub models: Vec<Value>,
    pub complete: bool,
    pub fetched_at: i64,
}

pub(super) async fn load(ctx: &PluginCtx, session: &Value) -> Option<Access> {
    let id = session
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?;
    let saved = ctx.ledger_get(id, LEDGER).await.ok()?;
    let now = crate::logging::now_ms();
    let fetched = saved.get("fetchedAt").and_then(Value::as_i64)?;
    if saved.get("authority") != Some(&authority(session))
        || fetched > now
        || now.saturating_sub(fetched) >= TTL_MS
    {
        return None;
    }
    Some(Access {
        models: saved.get("models").and_then(Value::as_array)?.clone(),
        complete: saved
            .get("complete")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        fetched_at: fetched,
    })
}

pub(super) async fn save(
    ctx: &PluginCtx,
    session: &Value,
    models: &[Value],
    complete: bool,
) -> Result<(), GatewayError> {
    let id = session.get("id").and_then(Value::as_str).unwrap_or("");
    if id.is_empty() {
        return Ok(());
    }
    ctx.ledger_put(id, LEDGER, &json!({
        "authority": authority(session), "fetchedAt": crate::logging::now_ms(), "models": models, "complete": complete,
    })).await.map_err(|_| GatewayError::with_status(502, "企业模型目录未能持久保存"))
}

pub(super) async fn check(
    ctx: &PluginCtx,
    region: Region,
    session: &Value,
    model: &str,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
) -> Result<Option<Value>, GatewayError> {
    catalog_state::current(ctx, region).await;
    if !enterprise(session) {
        if catalog_state::model(region, model).is_some_and(|model| {
            model.get("workbuddyAccountScoped").and_then(Value::as_bool) == Some(true)
        }) {
            return Err(unavailable(
                "该模型仅在企业账号目录中声明，当前个人账号没有授权",
            ));
        }
        return Ok(catalog_state::model(region, model));
    }
    let own = match load(ctx, session).await {
        Some(own) => own,
        None => {
            let outcome = catalog::refresh_session(ctx, region, session, proxy).await;
            if !outcome.refreshed {
                return Err(GatewayError::with_status(
                    503,
                    "企业账号模型权限尚未确认，未发送对话请求",
                )
                .with_code("account_model_permissions_unknown"));
            }
            load(ctx, session)
                .await
                .ok_or_else(|| unavailable("企业账号权限目录未能恢复"))?
        }
    };
    if let Some(model) = own.models.into_iter().find(|entry| {
        entry
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| id.eq_ignore_ascii_case(model))
    }) {
        catalog_state::merge_account(region, std::slice::from_ref(&model));
        return Ok(Some(model));
    }
    Err(if own.complete {
        unavailable("当前企业账号目录没有这个模型，未将请求发给无授权账号")
    } else {
        GatewayError::with_status(503, "企业目录只取得部分条目，目标模型权限未知")
            .with_code("account_model_permissions_unknown")
    })
}

fn unavailable(message: &str) -> GatewayError {
    GatewayError::with_status(403, message).with_code("account_model_unavailable")
}
