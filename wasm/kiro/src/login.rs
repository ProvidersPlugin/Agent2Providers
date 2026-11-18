//! Kiro双阶段托管授权；所有端口/state/回调与步骤来自原宿主Call，不自建监听。
//! portal选择AWS仅消费一次可信输入、先撤销旧OAuth材料，再advance同端口下一阶段。
//! 换码前CAS占用一次，失败不恢复等待、不重发code、不延长原流程期限。

use crate::{
    credentials, endpoints, errors,
    login_callback::{self, Kind},
    login_methods, login_oauth as oauth,
    login_stage::{self as stage, Stage},
    util,
};
use agent2api_wasm_sdk::{
    self as sdk, json,
    login::managed,
    state::{self, Scope, Snapshot},
    Error, Result, Value,
};

const KEY: &str = "kiro.oauth";

fn invalid() -> Error {
    stage::invalid()
}
fn flow(params: &Value) -> Result<&str> {
    params["flow"]
        .as_str()
        .filter(|flow| {
            flow.starts_with("managed_")
                && flow.len() <= 256
                && flow
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        })
        .ok_or_else(invalid)
}
fn ttl(context: &managed::Context) -> Result<i64> {
    let left = context
        .expires_at
        .checked_sub(util::now()?)
        .filter(|left| *left >= 1000)
        .ok_or_else(invalid)?;
    let left = left
        .min(i64::try_from(context.remaining_ms).map_err(|_| invalid())?)
        .min(endpoints::LOGIN_TTL);
    if left < 1000 {
        return Err(invalid());
    }
    Ok(left)
}
fn save(before: &Snapshot, value: Value, context: &managed::Context) -> Result<Snapshot> {
    let context = stage::fresh(context)?;
    if !before.compatible
        || !state::compare_exchange(
            Scope::Login,
            KEY,
            1,
            before.revision,
            value.clone(),
            ttl(&context)?,
        )?
    {
        return Err(invalid());
    }
    let after = state::get(Scope::Login, KEY, 1)?;
    if !after.compatible || after.value != value {
        return Err(invalid());
    }
    Ok(after)
}
fn clear(before: &Snapshot) -> Result<Snapshot> {
    if !before.compatible
        || !state::compare_exchange(Scope::Login, KEY, 1, before.revision, Value::Null, 1000)?
    {
        return Err(invalid());
    }
    let after = state::get(Scope::Login, KEY, 1)?;
    if !after.compatible || !after.value.is_null() {
        return Err(invalid());
    }
    Ok(after)
}
fn current(context: &managed::Context) -> Result<Snapshot> {
    let snapshot = state::get(Scope::Login, KEY, 1)?;
    if !snapshot.compatible
        || snapshot.value["flow"] != context.flow
        || snapshot.value["expiresAt"].as_i64() != Some(context.expires_at)
        || snapshot.value["interaction"] != context.interaction
        || !matches!(snapshot.value["phase"].as_str(), Some("portal" | "aws"))
    {
        return Err(invalid());
    }
    ttl(context)?;
    Ok(snapshot)
}
fn reserve(before: &Snapshot, phase: &str, context: &managed::Context) -> Result<String> {
    let attempt = util::nonce(24)?;
    // 撤销共享code/verifier/clientSecret材料；本次调用栈独占它们，不能再由poll/恢复换码。
    save(
        before,
        json!({ "phase":phase, "flow":context.flow, "interaction":context.interaction,
        "expiresAt":context.expires_at, "attempt":attempt }),
        context,
    )?;
    Ok(attempt)
}
fn owned(attempt: &str, context: &managed::Context) -> Result<Snapshot> {
    let snapshot = state::get(Scope::Login, KEY, 1)?;
    if !snapshot.compatible
        || snapshot.value["attempt"] != attempt
        || snapshot.value["flow"] != context.flow
        || snapshot.value["interaction"] != context.interaction
        || snapshot.value["expiresAt"].as_i64() != Some(context.expires_at)
    {
        return Err(invalid());
    }
    ttl(context)?;
    Ok(snapshot)
}
fn failed() -> Value {
    json!({ "state":"failed", "message":"Kiro授权未确认完成，已终止本次流程；不会重发授权码，请重新发起" })
}
fn inputs(params: &Value, context: &managed::Context) -> Result<Value> {
    let payload = &params["payload"];
    if payload.as_object().is_none_or(|fields| {
        fields.keys().any(|key| {
            !matches!(
                key.as_str(),
                "methodIndex" | "inputs" | "locale" | "mode" | "proxy"
            )
        })
    }) || payload["methodIndex"].as_u64() != Some(context.method_index)
        || payload
            .get("mode")
            .is_some_and(|mode| !matches!(mode.as_str(), Some("external" | "embedded" | "auto")))
    {
        return Err(invalid());
    }
    // proxy是宿主传输信封；出口始终由可信Call保留，不读取、不改写、不交给上游。
    let input = payload.get("inputs").cloned().unwrap_or_else(|| json!({}));
    let fields = input
        .as_object()
        .ok_or_else(|| Error::http(400, "Kiro登录字段必须为对象"))?;
    if fields.iter().any(|(key, value)| {
        !matches!(key.as_str(), "loginMethod" | "startUrl" | "region")
            || value
                .as_str()
                .is_none_or(|text| text.len() > 8192 || text.chars().any(char::is_control))
    }) {
        return Err(Error::http(400, "Kiro登录包含未申报字段"));
    }
    Ok(input)
}

pub fn start(params: &Value) -> Result<Value> {
    if params["payload"]["methodIndex"] == 1 {
        return Err(credentials::local_unavailable());
    }
    let context = stage::context(params)?;
    if params["method"].as_str() != Some("browser") {
        return Err(invalid());
    }
    let input = inputs(params, &context)?;
    let choice = input["loginMethod"]
        .as_str()
        .filter(|choice| !choice.is_empty())
        .unwrap_or("portal");
    if !matches!(choice, "portal" | "builderid" | "enterprise") {
        return Err(Error::http(400, "Kiro登录方式无效"));
    }
    if choice != "enterprise" && (input.get("startUrl").is_some() || input.get("region").is_some())
    {
        return Err(Error::http(400, "Kiro登录包含当前方式未启用的字段"));
    }
    let region = input["region"]
        .as_str()
        .map(str::trim)
        .filter(|region| !region.is_empty())
        .unwrap_or("us-east-1");
    let issuer = if choice == "enterprise" {
        input["startUrl"].as_str().unwrap_or("").trim()
    } else {
        endpoints::BUILDER_START
    };
    if choice != "portal" {
        endpoints::auth_region(region)?;
        if !endpoints::https(issuer) {
            return Err(Error::http(400, "AWS组织登录地址须为有效HTTPS地址"));
        }
    }
    let before = state::get(Scope::Login, KEY, 1)?;
    if !before.compatible || !before.value.is_null() {
        return Err(invalid());
    }
    let attempt = reserve(&before, "preparing", &context)?;
    let result = (|| {
        let lease = stage::open(
            if choice == "portal" {
                stage::PORTAL_PROFILE
            } else {
                stage::AWS_PROFILE
            },
            &context,
        )?;
        let mut value =
            json!({ "flow":context.flow, "expiresAt":context.expires_at, "stage":lease.value() });
        let url = if choice == "portal" {
            let verifier = util::nonce(48)?;
            let url = oauth::portal(&lease.oauth_state, &verifier, &lease.redirect_uri);
            value["phase"] = json!("portal");
            value["verifier"] = json!(verifier);
            url
        } else {
            let aws = oauth::start_aws(
                if choice == "enterprise" {
                    "awsidc"
                } else {
                    "builderid"
                },
                issuer,
                region,
                &lease.oauth_state,
                &lease.redirect_uri,
            )?;
            let url = aws["authorize_url"]
                .as_str()
                .filter(|url| !url.is_empty())
                .ok_or_else(invalid)?
                .to_string();
            value["phase"] = json!("aws");
            value["aws"] = aws;
            url
        };
        stage::recheck(params, &context)?;
        owned(&attempt, &context)?;
        let interaction =
            stage::publish(&context, &lease, "Kiro 网页登录", login_methods::WEB, &url)?;
        value["interaction"] = json!(interaction);
        let reserved = owned(&attempt, &context)?;
        save(&reserved, value, &context)?;
        Ok(
            json!({ "flow":context.flow, "url":"", "poll_interval_ms":1000, "external_browser":true,
            "allowed_hosts":[], "instructions":login_methods::WEB }),
        )
    })();
    match result {
        Ok(value) => Ok(value),
        Err(error) => {
            if let Ok(before) = owned(&attempt, &context) {
                clear(&before)?;
            }
            Err(error)
        }
    }
}

pub fn callback(params: &Value) -> Result<Value> {
    let context = stage::context(params)?;
    let before = current(&context)?;
    let phase = before.value["phase"].as_str().ok_or_else(invalid)?;
    let lease = Stage::from_saved(
        &before.value["stage"],
        if phase == "portal" {
            stage::PORTAL_PROFILE
        } else {
            stage::AWS_PROFILE
        },
        &context,
    )?;
    let input = managed::take_input()?;
    let callback = match login_callback::parse(&input, &lease, phase) {
        Ok(callback) => callback,
        Err(error) => {
            clear(&before)?;
            return Err(error);
        }
    };
    drop(input);
    if phase == "aws" {
        if let Err(error) = stage::exact(
            &before.value["aws"],
            &[
                "region",
                "client_id",
                "client_secret",
                "provider",
                "state",
                "verifier",
                "authorize_url",
            ],
        ) {
            clear(&before)?;
            return Err(error);
        }
        if before.value["aws"]["state"] != lease.oauth_state {
            clear(&before)?;
            return Err(invalid());
        }
    }
    if matches!(callback.kind, Kind::Denied) {
        clear(&before)?;
        return Ok(failed());
    }
    if matches!(callback.kind, Kind::AwsChoice) {
        // 必须先真正消费host输入，再删除旧门户材料；不能只把verifier设null后复用旧state。
        let empty = clear(&before)?;
        drop(before);
        stage::recheck(params, &context)?;
        let next = stage::advance(&lease, &context)?;
        // advance由宿主再次清空并推进此键的版本；旧空快照不能作为新AWS材料的CAS版本。
        let advanced = state::get(Scope::Login, KEY, 1)?;
        if !advanced.compatible
            || advanced.schema != 1
            || !advanced.value.is_null()
            || advanced.revision <= empty.revision
        {
            return Err(invalid());
        }
        let attempt = reserve(&advanced, "advancing", &context)?;
        let result = (|| {
            stage::recheck(params, &context)?;
            let aws = oauth::start_aws(
                callback.get("login_option"),
                callback.get("issuer_url"),
                callback.get("idc_region"),
                &next.oauth_state,
                &next.redirect_uri,
            )?;
            let url = aws["authorize_url"]
                .as_str()
                .filter(|url| !url.is_empty())
                .ok_or_else(invalid)?;
            stage::recheck(params, &context)?;
            owned(&attempt, &context)?;
            let interaction = stage::publish(
                &context,
                &next,
                "Kiro · AWS 授权",
                "请完成本次AWS授权；端口保持不变，旧门户回调已失效。",
                url,
            )?;
            let reserved = owned(&attempt, &context)?;
            save(
                &reserved,
                json!({"phase":"aws", "flow":context.flow, "interaction":interaction,
                "expiresAt":context.expires_at, "stage":next.value(), "aws":aws }),
                &context,
            )?;
            Ok(sdk::login::pending(None))
        })();
        return match result {
            Ok(value) => Ok(value),
            Err(error) => {
                if let Ok(before) = owned(&attempt, &context) {
                    clear(&before)?;
                }
                Err(error)
            }
        };
    }
    let attempt = reserve(&before, "exchanging", &context)?;
    stage::recheck(params, &context)?;
    owned(&attempt, &context)?;
    let exchanged = match callback.kind {
        Kind::AwsCode => oauth::aws(
            &before.value["aws"],
            callback.get("code"),
            &lease.redirect_uri,
        ),
        Kind::Social => oauth::social(
            callback.get("login_option"),
            callback.get("code"),
            before.value["verifier"].as_str().unwrap_or(""),
            &format!(
                "{}{}?login_option={}",
                lease.redirect_uri,
                callback.url.path(),
                callback.get("login_option")
            ),
        ),
        _ => return Err(invalid()),
    };
    drop(before);
    drop(callback);
    drop(lease);
    stage::recheck(params, &context)?;
    owned(&attempt, &context)?;
    let current = owned(&attempt, &context)?;
    clear(&current)?;
    stage::recheck(params, &context)?; // 迟到/过期结果不能变Done，也不能由状态重读触发第二次换码。
    match exchanged {
        Ok(account) => Ok(sdk::login::done(account)),
        Err(error) if errors::host_stop(&error) => Err(error),
        Err(_) => Ok(failed()),
    }
}

pub fn poll(params: &Value) -> Result<Value> {
    let context = stage::context(params)?;
    current(&context)?;
    // 只展示等待；不查询上游、不交换code、不恢复旧发布地址。
    Ok(sdk::login::pending(None))
}
pub fn cancel(params: &Value) -> Result<Value> {
    let flow = flow(params)?;
    let before = state::get(Scope::Login, KEY, 1)?;
    if !before.compatible || !before.value.is_null() && before.value["flow"] != flow {
        return Err(invalid());
    }
    clear(&before)?;
    // listener由宿主任务close清理；取消时不能重新bind，也不能因Call已关闭再请求读取权限。
    Ok(Value::Null)
}
