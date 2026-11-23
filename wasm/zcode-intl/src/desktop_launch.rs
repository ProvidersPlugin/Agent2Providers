use agent2api_wasm_sdk::{
    self as sdk,
    desktop_launch::{self as launch, Change},
    json, Error, Result, Value,
};
use base64::Engine;

use super::{account, network::text};

const FAMILY: &str = "zai";
const INACTIVE_FAMILY: &str = "bigmodel";
const ZAI_ANTHROPIC_BASE: &str = "https://api.z.ai/api/anthropic";
const START_PLAN_ANTHROPIC_BASE: &str = "https://zcode.z.ai/api/v1/zcode-plan/anthropic";

pub fn summary() -> Value {
    json!({
        "version": 1,
        "clients": [
            { "id": "zcode", "label": "ZCode", "platform": "windows" }
        ]
    })
}

pub fn jwt_user_id(token: &str) -> String {
    let Some(payload) = token.split('.').nth(1) else {
        return String::new();
    };
    let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
    else {
        return String::new();
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return String::new();
    };
    ["user_id", "userId", "sub", "id"]
        .iter()
        .find_map(|key| {
            let item = &value[*key];
            item.as_str()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .or_else(|| item.as_u64().map(|id| id.to_string()))
        })
        .unwrap_or_default()
}

pub fn resolve_user_id(secret: &Value, jwt: &str) -> String {
    let uid = account::user(secret);
    if !uid.is_empty() {
        return uid;
    }
    jwt_user_id(jwt)
}

fn available(secret: &Value) -> (bool, &'static str) {
    let jwt = account::jwt(secret);
    if jwt.is_empty() {
        return (
            false,
            "该账号缺少 ZCode 登录令牌（JWT），请通过网页登录或导入完整凭证",
        );
    }
    if account::token_expired(&jwt) {
        return (false, "该账号的 ZCode 登录态已过期，请重新网页登录");
    }
    if resolve_user_id(secret, &jwt).is_empty() {
        return (
            false,
            "该账号缺少用户 ID，无法准备客户端身份，请重新网页登录",
        );
    }
    (true, "")
}

pub fn is_available(secret: &Value) -> bool {
    available(secret).0
}

pub fn check() -> Result<Value> {
    let context = launch::context()?;
    if context["client"].as_str() != Some("zcode") {
        return Err(Error::unsupported());
    }
    let record = account::record()?;
    let (ok, reason) = available(&record["secret"]);
    Ok(launch::ready(ok, reason))
}

fn patch_builtin_provider(
    providers: &mut serde_json::Map<String, Value>,
    id: &str,
    default_name: &str,
    base_url: &str,
    api_key: &str,
) {
    let key = api_key.trim();
    let mut entry = providers
        .get(id)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if !entry.get("name").is_some_and(Value::is_string) {
        entry.insert("name".into(), json!(default_name));
    }
    entry.insert("kind".into(), json!("anthropic"));
    entry.insert("source".into(), json!("custom"));
    let mut options = entry
        .get("options")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    options.insert("apiKey".into(), json!(key));
    options.insert("baseURL".into(), json!(base_url));
    if key.is_empty() {
        options.insert("apiKeyRequired".into(), json!(true));
        entry.insert("enabled".into(), json!(false));
    } else {
        options.remove("apiKeyRequired");
        entry.insert("enabled".into(), json!(true));
        entry.remove("systemDisabledReason");
    }
    entry.insert("options".into(), Value::Object(options));
    providers.insert(id.into(), Value::Object(entry));
}

fn deactivate_family_builtins(providers: &mut serde_json::Map<String, Value>, inactive: &str) {
    let prefix = format!("builtin:{inactive}");
    for (id, value) in providers.iter_mut() {
        if (id == &prefix || id.starts_with(&format!("{prefix}-"))) && value.is_object() {
            if let Some(obj) = value.as_object_mut() {
                obj.insert("enabled".into(), json!(false));
                obj.insert("systemDisabledReason".into(), json!("oauth_provider_inactive"));
            }
        }
    }
}

pub fn prepare() -> Result<Value> {
    let context = launch::context()?;
    let client = context["client"].as_str().ok_or_else(Error::unsupported)?;
    if client != "zcode" || context["preparing"] != true {
        return Err(Error::unsupported());
    }
    let raw = sdk::account()?;
    let secret = account::decode(&raw["secret"])?;
    let (ok, reason) = available(&secret);
    if !ok {
        return Err(Error::http(409, reason));
    }
    let jwt = account::jwt(&secret);
    let uid = resolve_user_id(&secret, &jwt);
    let access = account::access(&secret);
    let coding_key = if !access.is_empty() && access != jwt {
        access.as_str()
    } else {
        ""
    };
    let oauth_access = {
        let token = account::strip_bearer(&text(
            &secret,
            &["oauthAccessToken", "oauth_access_token", "bizToken", "biz_token"],
        ));
        if token.is_empty() {
            jwt.clone()
        } else {
            token
        }
    };
    let oauth_refresh = account::strip_bearer(&text(
        &secret,
        &[
            "oauthRefreshToken",
            "oauth_refresh_token",
            "refreshToken",
            "refresh_token",
        ],
    ));
    let email = text(&secret, &["email"]);
    let raw_name = text(&secret, &["name"]);
    let display_name = if !raw_name.is_empty() {
        raw_name
    } else if !email.is_empty() {
        email.clone()
    } else {
        uid.clone()
    };
    let user_info = json!({
        "id": uid,
        "username": display_name,
        "displayName": display_name,
        "email": email,
        "rawProfile": {
            "user_id": uid,
            "name": display_name,
            "email": email,
            "zcodeProfileSchemaVersion": 2
        }
    })
    .to_string();

    let prev_creds = launch::existing("credentials")?;
    let prev_config = launch::existing("config")?;
    let prev_settings = launch::existing("settings")?;

    let mut creds_map = serde_json::Map::new();
    creds_map.insert("zcodejwttoken".into(), json!(jwt));
    creds_map.insert("oauth:active_provider".into(), json!(FAMILY));
    creds_map.insert(
        "oauth:login_attribution".into(),
        prev_creds
            .get("oauth:login_attribution")
            .cloned()
            .unwrap_or(Value::Null),
    );
    creds_map.insert(format!("oauth:{FAMILY}:access_token"), json!(oauth_access));
    creds_map.insert(
        format!("oauth:{FAMILY}:refresh_token"),
        if oauth_refresh.is_empty() {
            Value::Null
        } else {
            json!(oauth_refresh)
        },
    );
    creds_map.insert(format!("oauth:{FAMILY}:user_info"), json!(user_info));
    creds_map.insert(
        format!("oauth:{INACTIVE_FAMILY}:access_token"),
        Value::Null,
    );
    creds_map.insert(
        format!("oauth:{INACTIVE_FAMILY}:refresh_token"),
        Value::Null,
    );
    creds_map.insert(format!("oauth:{INACTIVE_FAMILY}:user_info"), Value::Null);
    if !coding_key.is_empty() {
        creds_map.insert(
            format!("account-provider:coding-plan:account:{FAMILY}-individual-coding-plan:account:{uid}:api-key"),
            json!(coding_key),
        );
    }

    let mut providers = prev_config
        .get("provider")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    patch_builtin_provider(
        &mut providers,
        "builtin:zai",
        "Z.ai - API Key",
        ZAI_ANTHROPIC_BASE,
        "",
    );
    patch_builtin_provider(
        &mut providers,
        "builtin:zai-coding-plan",
        "Z.ai - Coding Plan",
        ZAI_ANTHROPIC_BASE,
        coding_key,
    );
    patch_builtin_provider(
        &mut providers,
        "builtin:zai-start-plan",
        "Z.ai - Coding Plan",
        START_PLAN_ANTHROPIC_BASE,
        &jwt,
    );
    deactivate_family_builtins(&mut providers, INACTIVE_FAMILY);
    let config_value = json!({ "provider": Value::Object(providers) });

    let use_start = account::plan(&secret) == account::START || coding_key.is_empty();
    let selection_kind = if use_start {
        "start-plan"
    } else {
        "individual-coding-plan"
    };
    let mut selections = prev_settings
        .get("providerFamilyConnectionSelections")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    selections.insert(FAMILY.into(), json!({ "kind": selection_kind }));
    let modes_value = prev_settings
        .get("modelProviderFamilyModes")
        .and_then(Value::as_object)
        .cloned()
        .map(|mut modes| {
            modes.insert(FAMILY.into(), json!("oauth"));
            Value::Object(modes)
        })
        .unwrap_or(Value::Null);
    let selected_keys_value = prev_settings
        .get("modelProviderFamilySelectedKeys")
        .and_then(Value::as_object)
        .cloned()
        .map(|mut keys| {
            let suffix = if use_start { "start-plan" } else { "coding-plan" };
            keys.insert(
                FAMILY.into(),
                json!(format!("coding-plan:builtin:{FAMILY}-{suffix}")),
            );
            Value::Object(keys)
        })
        .unwrap_or(Value::Null);
    let settings_value = json!({
        "providerFamilyDomain": FAMILY,
        "providerFamilyDomainUpdatedAt": sdk::now(),
        "providerFamilyConnectionSelections": Value::Object(selections),
        "modelProviderFamilyModes": modes_value,
        "modelProviderFamilySelectedKeys": selected_keys_value
    });

    if sdk::account()?["secret"] != raw["secret"] {
        return Err(Error::http(409, "ZCode 国际版账号已变化，请重新操作"));
    }
    launch::plan(
        client,
        raw["secret"].clone(),
        vec![
            Change {
                resource: "credentials".into(),
                value: Value::Object(creds_map),
            },
            Change {
                resource: "config".into(),
                value: config_value,
            },
            Change {
                resource: "settings".into(),
                value: settings_value,
            },
        ],
    )
}
