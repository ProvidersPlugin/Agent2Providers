//! 只刷新当前池当前账号。即使两个池引用同一令牌，也不越权读写另一池。
//! 跨池授权凭据组的原子同步必须由宿主另行提供；本文件不实现跨池续期。

use super::{account, client, desktop, endpoints};
use agent2api_wasm_sdk::{self as sdk, json, state::Scope, AccountLock, Error, Result, Value};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

const OVERLAY: &str = "desktop-credential-overlay";
const KEY: &str = "credentials";
const TTL: i64 = 7_200_000;
const CONTROL: &str = "cline-refresh-control";
const IN_FLIGHT_TTL_MS: i64 = 60_000;
const BACKOFF_TTL_MS: i64 = 60_000;

fn unresolved() -> Error {
    Error::http(409,
        "Cline 上次续期结果尚未确认，不会重复消费同一续期令牌；仍有效的访问令牌可继续使用，到期后请重新登录")
        .with_code("cline_refresh_result_unconfirmed")
}

fn backoff_key(current: &Snapshot) -> String {
    format!(
        "refresh-backoff:{}",
        account::fingerprint(&account::text(&current.record, &["id"]))
    )
}

fn control_active(value: &Value, stamp: &str, now: i64) -> bool {
    value["stamp"] == stamp
        && value["at"]
            .as_i64()
            .is_some_and(|at| at > 0 && now >= at && now.saturating_sub(at) < IN_FLIGHT_TTL_MS)
}

fn clear_control(stamp: &str) {
    let Ok(current) = sdk::state::get(Scope::Account, CONTROL, 1) else {
        return;
    };
    if current.value.is_null() {
        return;
    }
    if current.compatible && current.value["stamp"] != stamp {
        return;
    }
    let _ = sdk::state::compare_exchange(
        Scope::Account,
        CONTROL,
        1,
        current.revision,
        Value::Null,
        0,
    );
}

fn refresh_stamp(current: &Snapshot) -> String {
    // 按刷新材料而非可轮换的访问令牌认这一轮；持久账号状态不会因实例/重启丢失。
    account::fingerprint(
        &json!({ "provider": current.record["provider"],
        "identity": current.record["identity"],
        "refresh": account::text(&current.secret, &["refreshToken", "refresh_token"]) })
        .to_string(),
    )
}

fn usable(current: &Snapshot) -> bool {
    account::expiry(&current.secret).is_some_and(|at| at > sdk::now())
}

fn retain_valid(current: &Snapshot) -> Result<Value> {
    let latest = read()?;
    check_record(&current.record, &latest.record)?;
    if current.secret != latest.secret
        || current.stamp != latest.stamp
        || current.overlay_revision != latest.overlay_revision
        || !usable(&latest)
    {
        return Err(source_changed());
    }
    Ok(latest.secret)
}

struct Snapshot {
    record: Value,
    secret: Value,
    stamp: String,
    overlay_revision: u64,
}

fn identity_changed() -> Error {
    Error::http(
        409,
        "Cline 续期发现账号身份冲突，未交付本次新凭据；请先确认当前账号",
    )
    .with_code("identity_changed")
}

fn source_changed() -> Error {
    Error::http(
        409,
        "Cline 续期期间原凭据来源或临时覆盖已变化，未交付本次新凭据；本次不会自动重发",
    )
    .with_code("cline_refresh_source_changed")
}

fn source_unconfirmed() -> Error {
    Error::http(
        502,
        "Cline 原凭据来源未能确认，未交付本次新凭据；本次不会自动重发",
    )
    .with_code("cline_refresh_source_unconfirmed")
}

fn save_unconfirmed() -> Error {
    Error::http(
        502,
        "Cline 本次续期结果的保存未确认，未交付新凭据；本次不会自动重发",
    )
    .with_code("cline_refresh_save_unconfirmed")
}

fn invalid_response() -> Error {
    Error::http(
        502,
        "Cline 续期响应的结构或凭据字段无效，未保存或交付本次结果；本次不会自动重发",
    )
    .with_code("cline_refresh_invalid_response")
}

fn missing_field() -> Error {
    Error::http(
        502,
        "Cline 续期响应缺少必要凭据，无法确认完整结果，未保存或交付；本次不会自动重发",
    )
    .with_code("cline_refresh_missing_field")
}

fn invalid_field_type() -> Error {
    Error::http(
        502,
        "Cline 续期响应的凭据或身份字段类型异常，未保存或交付；本次不会自动重发",
    )
    .with_code("cline_refresh_field_type")
}

fn empty_field() -> Error {
    Error::http(
        502,
        "Cline 续期响应的凭据或身份字段为空，未保存或交付；本次不会自动重发",
    )
    .with_code("cline_refresh_empty_field")
}

fn conflicting_aliases() -> Error {
    Error::http(
        502,
        "Cline 续期响应的凭据别名互相冲突，未保存或交付；本次不会自动重发",
    )
    .with_code("cline_refresh_alias_conflict")
}

fn identity_unconfirmed() -> Error {
    Error::http(
        409,
        "Cline 续期凭据缺少可核对的账号身份，未保存或交付本次结果；请重新登录",
    )
    .with_code("cline_refresh_identity_unconfirmed")
}

fn same_identity(previous: &Value, current: &Value) -> bool {
    ["id", "provider", "identity"]
        .iter()
        .all(|key| previous[*key] == current[*key])
}

fn check_record(previous: &Value, current: &Value) -> Result<()> {
    if !same_identity(previous, current)
        || previous["desktop"] != current["desktop"]
        || sdk::account_disabled(current)
    {
        return Err(identity_changed());
    }
    if previous["secret"] != current["secret"] {
        return Err(source_changed());
    }
    Ok(())
}

fn read() -> Result<Snapshot> {
    // 这里只重读发送前的快照；续期请求一旦发出，不再回退到 read()/snapshot()。
    for _ in 0..3 {
        if let Some(snapshot) = read_once()? {
            return Ok(snapshot);
        }
    }
    Err(source_changed())
}

fn read_once() -> Result<Option<Snapshot>> {
    let record = sdk::account().map_err(|_| source_unconfirmed())?;
    if !account::local(&record["secret"]) {
        let secret = account::parse(&record["secret"])?;
        let latest = sdk::account().map_err(|_| source_unconfirmed())?;
        check_record(&record, &latest)?;
        return Ok(Some(Snapshot {
            secret,
            record,
            stamp: String::new(),
            overlay_revision: 0,
        }));
    }
    if record["secret"]["desktopReadOnly"] == true {
        // 只读关联不依赖旧的续期覆盖槽；槽损坏不能挡住当前官方仍有效的登录态。
        let source = desktop::read(&record["secret"])?;
        let latest_record = sdk::account().map_err(|_| source_unconfirmed())?;
        check_record(&record, &latest_record)?;
        return Ok(Some(Snapshot {
            record,
            secret: source.secret,
            stamp: source.stamp,
            overlay_revision: 0,
        }));
    }
    let overlay =
        sdk::state::conversation_get(OVERLAY, KEY, 1).map_err(|_| source_unconfirmed())?;
    // 读取失败不是来源已失效的证据，不能顺手清除已经保存的临时凭据。
    let source = desktop::read(&record["secret"])?;
    let latest = sdk::state::conversation_get(OVERLAY, KEY, 1).map_err(|_| source_unconfirmed())?;
    let latest_record = sdk::account().map_err(|_| source_unconfirmed())?;
    if !same_identity(&record, &latest_record) {
        return Err(identity_changed());
    }
    if latest.revision != overlay.revision || latest_record["secret"] != record["secret"] {
        return Ok(None);
    }
    if !overlay.compatible {
        return Err(source_changed());
    }
    let mut overlay_revision = overlay.revision;
    let secret = if overlay.value["stamp"] == source.stamp
        && overlay.value["secret"].is_object()
        && overlay.value["until"]
            .as_i64()
            .is_some_and(|until| until > sdk::now())
    {
        let secret = account::parse(&overlay.value["secret"])?;
        if account::identity(&secret) != account::identity(&source.secret) {
            return Err(identity_changed());
        }
        secret
    } else {
        if !overlay.value.is_null() {
            if !sdk::state::conversation_compare_exchange(
                OVERLAY,
                KEY,
                1,
                overlay.revision,
                Value::Null,
                TTL,
            )
            .map_err(|_| source_unconfirmed())?
            {
                return Ok(None);
            }
            let cleared =
                sdk::state::conversation_get(OVERLAY, KEY, 1).map_err(|_| source_unconfirmed())?;
            if !cleared.compatible || !cleared.value.is_null() {
                return Ok(None);
            }
            overlay_revision = cleared.revision;
        }
        source.secret
    };
    Ok(Some(Snapshot {
        record,
        secret,
        stamp: source.stamp,
        overlay_revision,
    }))
}

pub fn snapshot() -> Result<Value> {
    Ok(read()?.secret)
}

pub fn confirm(expected: &Value) -> Result<()> {
    let current = read()?;
    if account::access(&current.secret)? != account::access(expected)?
        || account::identity(&current.secret) != account::identity(expected)
    {
        return Err(identity_changed());
    }
    Ok(())
}

pub fn expiring_within(secret: &Value, min_lead_ms: i64) -> bool {
    let margin = endpoints::REFRESH_MARGIN.max(min_lead_ms.max(0));
    account::expiry(secret)
        .is_some_and(|at| at.saturating_sub(margin) <= sdk::now())
}

pub fn expiring(secret: &Value) -> bool {
    expiring_within(secret, 0)
}

pub fn renewable(secret: &Value) -> bool {
    !account::text(secret, &["refreshToken", "refresh_token"]).is_empty()
}

pub fn due(min_lead_ms: i64) -> Result<bool> {
    let snapshot = read()?;
    if account::local(&snapshot.record["secret"]) {
        let status = sdk::desktop_refresh::status(crate::PROVIDER_ID)?;
        return Ok(status["authorized"] == true && status["refreshable"] == true && expiring_within(&snapshot.secret, min_lead_ms));
    }
    if !renewable(&snapshot.secret) { return Ok(false); }
    let stamp = refresh_stamp(&snapshot);
    let control = sdk::state::get(Scope::Account, CONTROL, 1)?;
    if control.compatible && control_active(&control.value, &stamp, sdk::now()) {
        return Ok(false);
    }
    if usable(&snapshot) && sdk::session_get(&backoff_key(&snapshot)).is_ok_and(|entry| entry["stamp"] == stamp) {
        return Ok(false);
    }
    Ok(expiring_within(&snapshot.secret, min_lead_ms))
}

fn completion_stamp(current: &Snapshot, secret: &Value, overlay_revision: u64) -> String {
    // 显式 null 用于清除旧到期时间；read() 会把未知到期时间归一为缺字段。
    let mut credentials = secret.clone();
    if credentials["expiresAt"].is_null() {
        if let Some(object) = credentials.as_object_mut() {
            object.remove("expiresAt");
        }
    }
    account::fingerprint(
        &json!({ "id": current.record["id"], "provider": current.record["provider"],
            "identity": current.record["identity"], "source": current.stamp,
            "overlayRevision": overlay_revision, "secret": credentials })
        .to_string(),
    )
}

// updated=false 只允许补读本账号来区分身份冲突，绝不交付这次补读的凭据或重试写入。
fn save_conflict(current: &Snapshot) -> Error {
    match sdk::account() {
        Ok(record) if !same_identity(&current.record, &record) => identity_changed(),
        _ => source_changed(),
    }
}

fn save(current: &Snapshot, secret: &Value) -> Result<u64> {
    if account::local(&current.record["secret"]) {
        let overlay =
            sdk::state::conversation_get(OVERLAY, KEY, 1).map_err(|_| source_unconfirmed())?;
        if !overlay.compatible || overlay.revision != current.overlay_revision {
            return Err(source_changed());
        }
        let source = desktop::read(&current.record["secret"]).map_err(|error| {
            if error.code.as_deref() == Some("identity_changed") {
                identity_changed()
            } else {
                source_unconfirmed()
            }
        })?;
        if source.stamp != current.stamp {
            return Err(source_changed());
        }
        let record = sdk::account().map_err(|_| source_unconfirmed())?;
        check_record(&current.record, &record)?;
        let now = sdk::now();
        if now <= 0 {
            return Err(source_unconfirmed());
        }
        // 临时覆盖不是桌面文件落盘。只信本次 CAS 的明确回执，不补读猜测是否保存成功。
        // helper 来源、账号与覆盖仍没有宿主同锁原子核对，不能据此宣称已解决来源 ABA。
        let updated = sdk::host(
            "state.compare_exchange",
            json!({ "scope": "conversation", "conversation": OVERLAY, "key": KEY,
                "schema": 1, "revision": overlay.revision,
                "value": { "stamp": source.stamp, "secret": secret, "until": now.saturating_add(TTL) },
                "ttl_ms": TTL }),
        )
        .map_err(|_| save_unconfirmed())?;
        match updated.get("updated").and_then(Value::as_bool) {
            Some(true) => updated["revision"]
                .as_u64()
                .filter(|revision| *revision > overlay.revision)
                .ok_or_else(save_unconfirmed),
            Some(false) => Err(save_conflict(current)),
            None => Err(save_unconfirmed()),
        }
    } else {
        let record = sdk::account().map_err(|_| source_unconfirmed())?;
        check_record(&current.record, &record)?;
        let mut public = account::view(secret);
        public["needsRelogin"] = json!(false);
        public["needsReloginReason"] = json!("");
        let updated = sdk::host(
            "account.update",
            json!({ "expect": current.record["secret"], "secret": secret, "public": public }),
        )
        .map_err(|_| save_unconfirmed())?;
        match updated.get("updated").and_then(Value::as_bool) {
            Some(true) => Ok(0),
            Some(false) => Err(save_conflict(current)),
            None => Err(save_unconfirmed()),
        }
    }
}

fn mark_relogin(current: &Snapshot) {
    let Ok(record) = sdk::account() else { return };
    if !same_identity(&current.record, &record) || record["secret"] != current.record["secret"] {
        return;
    }
    let mut public = account::view(&current.secret);
    public["needsRelogin"] = json!(true);
    public["needsReloginReason"] = json!("Cline 登录授权已失效，请重新登录");
    let _ = sdk::host(
        "account.update",
        json!({ "expect": current.record["secret"], "secret": current.record["secret"], "public": public }),
    );
}

fn refresh_denied(reply: &client::Reply) -> bool {
    if matches!(reply.status, 401 | 403) { return true; }
    if reply.status != 400 { return false; }
    let text = [&reply.body["error"]["code"], &reply.body["error"]["message"],
        &reply.body["error"], &reply.body["code"], &reply.body["message"]]
        .into_iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ").to_ascii_lowercase();
    ["invalid_grant", "invalid refresh", "refresh token", "revoked", "expired", "session not found", "token not found"]
        .iter().any(|marker| text.contains(marker))
}

fn execute_refresh(current: &Snapshot, old_uid: &str, body: &Value, force: bool) -> Result<Value> {
    let latest = read()?;
    check_record(&current.record, &latest.record)?;
    if current.secret != latest.secret || current.stamp != latest.stamp || current.overlay_revision != latest.overlay_revision {
        return Err(source_changed());
    }
    let reply = match client::request("POST", "/auth/refresh", None, false, Some(body)) {
        Ok(reply) => reply,
        Err(error) => {
            if error.code.as_deref() != Some("aborted") && !force && usable(current) {
                let _ = sdk::session_put(&backoff_key(current), json!({ "stamp": refresh_stamp(current) }), BACKOFF_TTL_MS);
                return retain_valid(current);
            }
            return Err(error);
        }
    };
    let secret = match refreshed(&current.secret, old_uid, &reply) {
        Ok(secret) => secret,
        Err(error) => {
            if refresh_denied(&reply) {
                mark_relogin(current);
                return Err(Error::http(401, "Cline 续期令牌已被拒绝，请重新登录；本次不会自动重发").with_code("refresh_token_expired"));
            }
            if !force && matches!(reply.status, 429 | 500..=599) && usable(current) {
                let _ = sdk::session_put(&backoff_key(current), json!({ "stamp": refresh_stamp(current) }), BACKOFF_TTL_MS);
                return retain_valid(current);
            }
            return Err(error);
        }
    };
    let saved_revision = save(current, &secret)?;
    let _ = sdk::session_remove(&backoff_key(current));
    let recent_key = format!("refresh:{}", account::fingerprint(&account::text(&current.record, &["id"])));
    let _ = sdk::session_put(&recent_key, json!({ "stamp": completion_stamp(current, &secret, saved_revision) }), 5000);
    Ok(secret)
}

pub fn ensure(force: bool) -> Result<Value> {
    let before = read()?;
    if account::local(&before.record["secret"]) {
        let status = sdk::desktop_refresh::status(crate::PROVIDER_ID)?;
        if (force || expiring(&before.secret)) && status["authorized"] == true && status["refreshable"] == true {
            match sdk::desktop_refresh::refresh(crate::PROVIDER_ID) {
                Ok(_) => return desktop::synchronize(&before.record),
                Err(error)
                    if !force && usable(&before)
                        && !matches!(error.code.as_deref(), Some("desktop_identity_changed" | "desktop_refresh_material_missing" | "refresh_token_expired")) =>
                {
                    return Ok(before.secret);
                }
                Err(error) => return Err(error),
            }
        }
        if account::expiry(&before.secret).is_some_and(|at| at <= sdk::now()) {
            return Err(Error::http(401, "Cline本机登录态已过期，请授权本机续期或重新登录官方客户端"));
        }
        if force { return desktop::synchronize(&before.record); }
        return Ok(before.secret);
    }
    if before.record["secret"]["desktopReadOnly"] == true {
        let expiry = account::expiry(&before.secret);
        if expiry.is_some_and(|at| at <= sdk::now()) {
            return Err(Error::http(401, "Cline 本机登录态已过期，请在官方客户端重新登录；不会消耗客户端的一次性续期凭据")
                .with_code("cline_desktop_relogin_required"));
        }
        if force {
            return Err(Error::http(401, "Cline 本机登录态已被拒绝，请在官方客户端重新登录；不会消耗客户端的一次性续期凭据")
                .with_code("cline_token"));
        }
        return Ok(before.secret);
    }
    if !force && !expiring(&before.secret) { return Ok(before.secret); }
    let _lock = AccountLock::acquire().map_err(|_| source_unconfirmed())?;
    let current = read()?;
    if !same_identity(&before.record, &current.record) { return Err(identity_changed()); }
    if current.record["secret"] != before.record["secret"] || current.secret != before.secret
        || current.stamp != before.stamp || current.overlay_revision != before.overlay_revision
    {
        if !force && usable(&current) { return Ok(current.secret); }
        return Err(source_changed());
    }
    if !force && !expiring(&current.secret) { return Ok(current.secret); }
    if !force && !renewable(&current.secret) {
        if account::expiry(&current.secret).is_none_or(|expiry| expiry > sdk::now()) { return Ok(current.secret); }
        mark_relogin(&current);
        return Err(Error::http(401, "Cline 访问令牌已到期且没有续期材料，请重新登录").with_code("refresh_token_expired"));
    }
    if !renewable(&current.secret) {
        mark_relogin(&current);
        return Err(Error::http(400, "Cline 账号没有 refreshToken，无法续期，请重新登录").with_code("refresh_token_expired"));
    }
    let recent_key = format!("refresh:{}", account::fingerprint(&account::text(&current.record, &["id"])));
    let stamp = completion_stamp(&current, &current.secret, current.overlay_revision);
    if sdk::session_get(&recent_key).map_err(|_| source_unconfirmed())?["stamp"] == stamp {
        return Ok(current.secret);
    }
    let reserved_stamp = refresh_stamp(&current);
    if !force && usable(&current) && sdk::session_get(&backoff_key(&current)).is_ok_and(|entry| entry["stamp"] == reserved_stamp) {
        return Ok(current.secret);
    }
    let old_uid = original_uid(&current.secret)?;
    let refresh = token_field(&current.secret, &["refreshToken", "refresh_token"], false, false)
        .map_err(|_| source_unconfirmed())?
        .ok_or_else(source_unconfirmed)?;
    let body = json!({ "refreshToken": refresh, "grantType": "refresh_token" });
    let control = sdk::state::get(Scope::Account, CONTROL, 1).map_err(|_| source_unconfirmed())?;
    if !force && control.compatible && control_active(&control.value, &reserved_stamp, sdk::now()) {
        if usable(&current) { return Ok(current.secret); }
        return Err(unresolved());
    }
    if !sdk::state::compare_exchange(
        Scope::Account, CONTROL, 1, control.revision,
        json!({ "stamp": reserved_stamp, "at": sdk::now() }), 0,
    ).map_err(|_| source_unconfirmed())? {
        return Err(source_changed());
    }
    let reserved = sdk::state::get(Scope::Account, CONTROL, 1).map_err(|_| source_unconfirmed())?;
    if !reserved.compatible || reserved.value["stamp"] != reserved_stamp {
        return Err(source_changed());
    }
    let outcome = execute_refresh(&current, &old_uid, &body, force);
    // 请求已结束（含失败）时释放进行中预约，避免临时网络或代理问题把账号永久锁死。
    clear_control(&reserved_stamp);
    outcome
}

fn response_data(reply: &client::Reply) -> Result<&Value> {
    if matches!(reply.status, 401 | 403) {
        // 这是续期本身被拒绝，不是让外层再次续期的普通访问令牌过期。
        return Err(Error::http(403, "Cline 续期令牌已被拒绝，请重新登录；本次不会自动重发").with_code("cline_refresh_denied"));
    }
    if !(200..300).contains(&reply.status) {
        return Err(Error::http(502, format!("Cline 上游未确认本次续期成功（HTTP {}），未交付新凭据", reply.status))
            .with_code("cline_refresh_rejected"));
    }
    check_envelope(&reply.body)?;
    let data = match reply.body.get("data") {
        Some(data) if data.is_object() => data,
        Some(_) => return Err(invalid_response()),
        None => &reply.body,
    };
    check_envelope(data)?;
    for key in ["tokenType", "token_type"] {
        if let Some(value) = data.get(key).filter(|value| !value.is_null()) {
            if !value.as_str().is_some_and(|value| value.eq_ignore_ascii_case("Bearer")) {
                return Err(invalid_response());
            }
        }
    }
    Ok(data)
}

fn check_envelope(value: &Value) -> Result<()> {
    if !value.is_object()
        || value["type"] == "error"
        || value.get("success").is_some_and(|value| value.as_bool() != Some(true))
        || value.get("error").is_some_and(|value| !value.is_null() && value.as_str() != Some(""))
    {
        return Err(invalid_response());
    }
    Ok(())
}

fn token_field(value: &Value, keys: &[&str], access: bool, allow_empty: bool) -> Result<Option<String>> {
    let mut token: Option<String> = None;
    for key in keys {
        let Some(value) = value.get(*key).filter(|value| !value.is_null()) else { continue; };
        let raw = value.as_str().ok_or_else(invalid_field_type)?;
        if raw.is_empty() {
            if allow_empty { continue; }
            return Err(empty_field());
        }
        // 先验原文，不能把换行、空白、错类型、空字段或冲突别名当成“未轮换”。
        if raw.len() > endpoints::MAX_TOKEN || !raw.is_ascii()
            || raw.bytes().any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        {
            return Err(invalid_response());
        }
        let next = if access {
            match raw.strip_prefix(endpoints::PREFIX) {
                Some("") => return Err(invalid_response()),
                Some(value) if value.starts_with(endpoints::PREFIX) => return Err(invalid_response()),
                Some(_) => raw.to_string(),
                None => format!("{}{raw}", endpoints::PREFIX),
            }
        } else {
            raw.to_string()
        };
        if next.len() > endpoints::MAX_TOKEN { return Err(invalid_response()); }
        if token.as_ref().is_some_and(|token| token != &next) { return Err(conflicting_aliases()); }
        token = Some(next);
    }
    Ok(token)
}

fn jwt_claims(token: &str) -> Result<Value> {
    let raw = token.strip_prefix(endpoints::PREFIX).unwrap_or(token);
    if !raw.contains('.') { return Ok(Value::Null); }
    let mut parts = raw.split('.');
    let header = parts.next().filter(|value| !value.is_empty()).ok_or_else(invalid_response)?;
    let payload = parts.next().filter(|value| !value.is_empty()).ok_or_else(invalid_response)?;
    let signature = parts.next().filter(|value| !value.is_empty()).ok_or_else(invalid_response)?;
    if parts.next().is_some() || header.len() + payload.len() + signature.len() > endpoints::MAX_TOKEN {
        return Err(invalid_response());
    }
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).map_err(|_| invalid_response())?;
    let claims: Value = serde_json::from_slice(&bytes).map_err(|_| invalid_response())?;
    if !claims.is_object() { return Err(invalid_response()); }
    // 这里只核对上游原文中的声明，不把解码当签名验证，也不新增用户查询请求。
    Ok(claims)
}

fn uid_fields(value: &Value, keys: &[&str], allow_empty: bool) -> Result<Option<String>> {
    let mut uid: Option<String> = None;
    for key in keys {
        let Some(value) = value.get(*key).filter(|value| !value.is_null()) else { continue; };
        let next = value.as_str().ok_or_else(invalid_field_type)?;
        if next.is_empty() {
            if allow_empty { continue; }
            return Err(empty_field());
        }
        if next.len() > 512 || next.trim() != next || next.chars().any(char::is_control) {
            return Err(invalid_response());
        }
        if uid.as_deref().is_some_and(|uid| uid != next) { return Err(identity_changed()); }
        uid = Some(next.to_string());
    }
    Ok(uid)
}

fn join_uid(left: Option<String>, right: Option<String>) -> Result<Option<String>> {
    match (left, right) {
        (Some(left), Some(right)) if left != right => Err(identity_changed()),
        (Some(left), _) => Ok(Some(left)),
        (_, right) => Ok(right),
    }
}

fn original_uid(secret: &Value) -> Result<String> {
    let token = account::access(secret).map_err(|_| source_unconfirmed())?;
    let claims = jwt_claims(&token).map_err(|_| source_unconfirmed())?;
    let token_uid = uid_fields(&claims, &["external_id"], false).map_err(|_| source_unconfirmed())?;
    // 只保留原来已支持的 usr- 标识作为兜底；邮箱、名字和令牌摘要不证明账号相同。
    let stored = account::identity(secret);
    let stored_uid = if stored.starts_with("usr-") && stored.len() > 4 {
        uid_fields(&json!({ "account": stored }), &["account"], false)?
    } else {
        None
    };
    join_uid(token_uid, stored_uid)?.ok_or_else(identity_unconfirmed)
}

fn expiry_field(value: Option<&Value>) -> Result<Option<i64>> {
    let Some(value) = value.filter(|value| !value.is_null()) else { return Ok(None); };
    if value.as_str().is_some_and(|text| text.trim().is_empty()) {
        return Ok(None);
    }
    let raw = match value {
        Value::Number(value) => value.as_f64().filter(|number| number.is_finite() && number.fract() == 0.0),
        Value::String(value) if value.len() <= 64 && value.trim() == value && !value.chars().any(char::is_control) => {
            if let Ok(number) = value.parse::<i64>() {
                Some(number as f64)
            } else {
                return chrono::DateTime::parse_from_rfc3339(value)
                    .ok().map(|value| value.timestamp_millis()).filter(|at| *at > 0)
                    .map(Some).ok_or_else(invalid_response);
            }
        }
        _ => None,
    };
    let Some(number) = raw.filter(|number| *number > 0.0) else { return Err(invalid_response()); };
    let millis = if number < 100_000_000_000.0 { number * 1000.0 } else { number };
    if millis >= i64::MAX as f64 { return Err(invalid_response()); }
    Ok(Some(millis as i64))
}

fn jwt_expiry(claims: &Value) -> Result<Option<i64>> {
    let Some(value) = claims.get("exp").filter(|value| !value.is_null()) else { return Ok(None); };
    value.as_i64()
        .or_else(|| value.as_f64().filter(|n| n.is_finite() && n.fract() == 0.0 && *n > 0.0 && *n < (i64::MAX / 1000) as f64).map(|n| n as i64))
        .filter(|seconds| *seconds > 0)
        .and_then(|seconds| seconds.checked_mul(1000))
        .map(Some)
        .ok_or_else(invalid_response)
}

fn refreshed(current: &Value, old_uid: &str, reply: &client::Reply) -> Result<Value> {
    let data = response_data(reply)?;
    let access = token_field(data, &["accessToken", "access_token", "token"], true, false)?
        .ok_or_else(missing_field)?;
    let refresh = match token_field(data, &["refreshToken", "refresh_token"], false, true)? {
        Some(token) => token,
        None => token_field(current, &["refreshToken", "refresh_token"], false, false)?
            .ok_or_else(source_unconfirmed)?,
    };
    let claims = jwt_claims(&access)?;
    let token_uid = uid_fields(&claims, &["external_id"], false)?;
    let info_uid = match data.get("userInfo").filter(|value| !value.is_null()) {
        Some(info) if info.is_object() => uid_fields(info, &["clineUserId", "external_id"], true)?,
        Some(_) => return Err(invalid_response()),
        None => None,
    };
    let new_uid = join_uid(token_uid, info_uid)?.ok_or_else(identity_unconfirmed)?;
    if new_uid != old_uid { return Err(identity_changed()); }
    let expiry = match (expiry_field(data.get("expiresAt"))?, jwt_expiry(&claims)?) {
        // 两个真实截止时间同时存在时取较早者，不把访问令牌的有效期延长。
        (Some(response), Some(jwt)) => Some(response.min(jwt)),
        (response, jwt) => response.or(jwt),
    };
    let mut secret = current.clone();
    secret["accessToken"] = json!(access);
    secret["refreshToken"] = json!(refresh);
    // 显式 null 会同步清除宿主已有的 expiresAt 镜像；省略键会留下旧令牌的到期时间。
    secret["expiresAt"] = json!(expiry);
    // 原 account / identity 不变；不能用新响应的邮箱、名字或 WorkOS sub 换掉 Cline uid。
    Ok(secret)
}

pub fn token(force: bool) -> Result<String> {
    account::access(&ensure(force)?)
}
