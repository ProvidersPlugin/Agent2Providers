//! 账号身份提示不是宿主数据库主键；accounts.identity.v1 由宿主保留旧主键。
//! 2026-11-10：去掉以 AK 前缀作昵称及尾号判重，不联网、不接本机/文件/浏览器。

use agent2api_wasm_sdk::{json, Result, Value};

use crate::{
    credentials::{self, Credential},
    errors, gates, refresh, util,
};

pub fn identity(secret: &Value) -> Result<String> {
    Ok(Credential::parse(secret)?.identity())
}

pub fn normalize(params: &Value) -> Result<Value> {
    credentials::own_provider(params)?;
    let source = &params["source"];
    if source["kind"].as_str() != Some("manual") {
        return Err(errors::invalid(
            "手工导入请粘贴完整凭据 JSON；网页登录应从官方授权入口发起，不接受任意本机文件",
        ));
    }
    let (input, form_name) = manual(&source["payload"])?;
    let c = Credential::parse(&input)?;
    let identity = c.identity();
    let mut secret = c.secret()?;
    let name = params["name"]
        .as_str()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .or(form_name)
        .unwrap_or_else(|| util::text(&input, &["name"]));
    if !name.is_empty() {
        secret["name"] = json!(errors::public_text(&name, &c.secrets(), 100));
    }
    secret["source"] = json!("manual");
    secret["desktop"] = json!(false);
    // 每次显式导入是一代新的快照。只从宿主随机数生成，不能让输入自行指定绑定代次。
    secret["codeartsImportGeneration"] = json!(util::hex(&util::random::<32>()?));
    secret["codeartsIdentityMode"] = json!(if c.stable_identity().is_some() {
        "domain_user"
    } else {
        "credential_snapshot"
    });
    let mut public = view(&secret)?;
    // 手工材料存在不代表已完成本次宿主登记；登记成功后的公开字段由真实保存事务更新。
    public["canRefresh"] = json!(false);
    Ok(json!({ "identity": identity, "secret": secret, "public": public, "desktop": false }))
}

fn manual(input: &Value) -> Result<(Value, Option<String>)> {
    credentials::own_provider(input)?;
    if input.get("methodIndex").is_some() {
        if input["methodIndex"].as_u64() != Some(0) {
            return Err(errors::invalid(
                "网页登录必须从授权入口发起；手工添加请选择凭据 JSON",
            ));
        }
        let fields = input["inputs"]
            .as_object()
            .ok_or_else(|| errors::invalid("CodeArts 手工表单字段无效"))?;
        if fields.iter().any(|(key, value)| {
            !["credentialJson", "name"].contains(&key.as_str()) || !value.is_string()
        }) {
            return Err(errors::invalid("CodeArts 手工表单含有未申报字段"));
        }
        let value = util::object(
            fields
                .get("credentialJson")
                .and_then(Value::as_str)
                .unwrap_or(""),
        )?;
        let name = fields
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string);
        return Ok((value, name));
    }
    if let Some(text) = input.as_str() {
        return Ok((util::object(text)?, None));
    }
    // 旧面板在 jsonExpand 前后的两种手工形态；仍然不是文件导入。
    if input.get("credentialJson").is_some() {
        let value = util::object(
            input["credentialJson"]
                .as_str()
                .ok_or_else(|| errors::invalid("凭据 JSON 必须是文本"))?,
        )?;
        let name = input["name"]
            .as_str()
            .filter(|name| !name.trim().is_empty())
            .map(str::to_string);
        return Ok((value, name));
    }
    if !input.is_object() {
        return Err(errors::invalid("请粘贴完整 CodeArts 凭据 JSON"));
    }
    Ok((input.clone(), None))
}

pub fn view(secret: &Value) -> Result<Value> {
    let c = Credential::parse(secret)?;
    let secrets = c.secrets();
    let now = util::now()?;
    let explicit = util::text(secret, &["name"]);
    let name = if !explicit.is_empty() {
        explicit.as_str()
    } else if !c.user_name.is_empty() {
        c.user_name.as_str()
    } else {
        "CodeArts 账号"
    };
    let expiry = c.expiry();
    let available = !c.security_token.is_empty() && expiry.is_some_and(|at| at > now);
    let mut value = json!({
        "name": errors::public_text(name, &secrets, 100),
        "uid": errors::public_text(&c.user_id, &secrets, 256),
        "userId": errors::public_text(&c.user_id, &secrets, 256),
        "domainId": errors::public_text(&c.domain_id, &secrets, 256),
        "userName": errors::public_text(&c.user_name, &secrets, 100),
        "loginType": errors::public_text(&c.login_type, &secrets, 32),
        "tokenTail": tail(&c.access_key_id, &secrets),
        "expiresAt": expiry, "accessTokenExpiresAt": expiry,
        "hasAccessToken": !c.security_token.is_empty(), "hasRefreshToken": !c.refresh_token.is_empty(),
        "identityStatus": if c.stable_identity().is_some() { "domain_user" } else { "snapshot_only" },
        "validation": "format_only", "available": available, "desktop": false,
        "edition": null, "editionLabel": null, "longLived": false,
        "migrationStage": "complete", "availabilityMessage": gates::SUMMARY,
        "importMessage": gates::IMPORT
    });
    if c.stable_identity().is_none() {
        value["identityWarning"] = json!(gates::IDENTITY);
    }
    if c.security_token.is_empty() {
        value["credentialWarning"] = json!("尚缺 security_token；保存不代表可查询上游");
    } else if expiry.is_none() {
        value["credentialWarning"] =
            json!("到期时间未知或格式无效；只保存快照，不猜测期限或发送请求");
    } else if !available {
        value["credentialWarning"] = json!("临时凭据已到期，请完整导入更新后的材料");
    }
    let renewal = refresh::projection(&c, secret["needsRelogin"] == true, now);
    if let (Some(target), Some(fields)) = (value.as_object_mut(), renewal.as_object()) {
        target.extend(fields.clone());
    }
    Ok(value)
}

fn tail(value: &str, secrets: &[String]) -> String {
    if value.chars().count() <= 6 {
        return String::new();
    }
    let tail: String = value
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if errors::contains_secret(&tail, secrets) {
        String::new()
    } else {
        tail
    }
}
