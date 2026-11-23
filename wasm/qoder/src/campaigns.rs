//! 国内活动：只读实时 campaigns，领取前确认当前账号与上游下发的活动编号。
//! sash 使用普通 Bearer + cosy-clienttype:10，不带 COSY 签名或国际 UMID 风控头。
use agent2api_wasm_sdk::{json, Error, Result, Value};
use super::{account::{self, Snapshot}, client::{self, Reply}, endpoints, refresh, util};

pub struct Session { pub snapshot: Snapshot, pub list: Value }
pub fn load() -> Result<Session> {
    let mut snapshot = refresh::ensure(false)?;
    let original = snapshot.credentials.binding();
    let list = match fetch(&snapshot) {
        Ok(list) => list,
        Err(error) if error.status == Some(401) && snapshot.credentials.can_refresh() => {
            account::unchanged(&snapshot)?;
            snapshot = refresh::ensure(true)?;
            if snapshot.credentials.binding() != original { return Err(account::changed()); }
            fetch(&snapshot)?
        }
        Err(error) => return Err(error),
    };
    Ok(Session { snapshot, list })
}
pub fn unwrap_data(value: Value) -> Value {
    match value.get("data") { Some(data) if data.is_object() => data.clone(), Some(data) if data.is_array() => json!({ "campaigns": data }), _ => value }
}
pub fn fetch(snapshot: &Snapshot) -> Result<Value> {
    let reply = client::request("GET", endpoints::OPEN_API, &format!("{}?forceRefresh=true", endpoints::CAMPAIGNS),
        client::sash_headers(&snapshot.credentials.access), None, "活动查询")?;
    let value = unwrap_data(client::accepted(reply, "活动查询")?);
    let rows = value["campaigns"].as_array().filter(|rows| rows.len() <= 512).ok_or_else(|| Error::http(502, "Qoder 活动清单缺少有效 campaigns"))?;
    let uid = util::text(&value, &["uid"]);
    if !uid.is_empty() && uid != snapshot.credentials.user { return Err(account::changed()); }
    let mut ids = Vec::new();
    for row in rows {
        let id = row["campaignId"].as_str().filter(|id| id_valid(id)).ok_or_else(|| Error::http(502, "Qoder 活动清单缺少有效活动编号"))?;
        if ids.contains(&id) { return Err(Error::http(502, "Qoder 活动清单包含重复活动编号")); }
        ids.push(id);
    }
    account::unchanged(snapshot)?;
    Ok(value)
}
pub fn submit(snapshot: &Snapshot, id: &str) -> Result<Reply> {
    if !id_valid(id) { return Err(Error::http(400, "请选择有效的 Qoder 活动")); }
    account::unchanged(snapshot)?;
    let mut url = url::Url::parse(endpoints::OPEN_API).map_err(|_| Error::new("Qoder 活动接口配置无效"))?;
    url.set_path(endpoints::CAMPAIGNS);
    url.path_segments_mut().map_err(|_| Error::new("Qoder 活动路径配置无效"))?.push(id).push("claim");
    let mut headers = client::sash_headers(&snapshot.credentials.access);
    headers["origin"] = json!(endpoints::OPEN_API);
    client::request("POST", endpoints::OPEN_API, url.path(), headers, Some(&json!({})), "活动领取")
}
pub fn id_valid(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && !matches!(id, "." | "..") && util::printable(id, 256)
        && !id.contains(['/', '\\', '?', '#', '%']) && !id.bytes().any(|byte| byte.is_ascii_whitespace())
}
pub fn text(row: &Value, locale: &str, field: &str) -> String {
    ["POPUP", "USAGE"].iter().find_map(|kind| row["placements"].as_array().into_iter().flatten()
        .filter(|placement| placement["type"] == *kind).find_map(|placement| placement["content"][locale][field].as_str().filter(|value| !value.trim().is_empty())))
        .or_else(|| row[field].as_str()).unwrap_or("").trim().to_string()
}
pub fn credit(row: &Value) -> bool { row["actionType"] == "CLAIM_BENEFIT" && row["benefit"]["kind"] == "CREDITS" }
pub fn daily(row: &Value) -> bool {
    if !credit(row) { return false; }
    let title = format!("{} {}", text(row, "zh", "title"), text(row, "en", "title")).to_lowercase();
    let scope = &row["benefit"]["modelScope"];
    let scoped = scope["modelSeries"]["key"] != "ALL_MODELS" && !scope.is_null() && scope.as_object().is_none_or(|map| !map.is_empty());
    if scoped || title.contains("sonus") || title.contains("sota") { return false; }
    if ["每日", "每天", "daily", "every day"].iter().any(|part| title.contains(*part)) { return true; }
    let one_day = match (util::timestamp(&row["startAt"]), util::timestamp(&row["endAt"])) {
        (Some(start), Some(end)) => end > start && end - start <= 86_400_000, _ => false,
    };
    one_day && util::number(&row["benefit"]["amount"]) == Some(100.0) && util::number(&row["benefit"]["validity"]["days"]) == Some(30.0)
}
pub fn status(row: &Value, now: i64) -> &'static str {
    if util::timestamp(&row["startAt"]).is_some_and(|start| start > now) || util::timestamp(&row["endAt"]).is_some_and(|end| end <= now) { return "inactive"; }
    match row["claimStatus"].as_str() { Some("CLAIMED") => "claimed", Some("CLAIMABLE") => "claimable", _ => "ineligible" }
}
pub fn view(row: &Value, now: i64) -> Value {
    let title = text(row, "zh", "title"); let en = text(row, "en", "title");
    json!({ "campaignId": row["campaignId"], "title": if title.is_empty() { if en.is_empty() { "Qoder 活动" } else { &en } } else { &title },
        "titleEn": en, "description": text(row, "zh", "description"), "descriptionEn": text(row, "en", "description"),
        "amount": util::number(&row["benefit"]["amount"]).filter(|amount| *amount >= 0.0), "status": status(row, now),
        "startsAt": util::timestamp(&row["startAt"]), "endsAt": util::timestamp(&row["endAt"]),
        "claimedAt": util::timestamp(&row["claimedAt"]),
        "expiresAt": util::timestamp(&row["expiresAt"]).or_else(|| util::timestamp(&row["benefit"]["validity"]["fixedEnd"])),
        "validDays": util::number(&row["benefit"]["validity"]["days"]).filter(|days| *days > 0.0) })
}
