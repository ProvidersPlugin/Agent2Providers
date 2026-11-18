//! 国际活动普通清单与受风控增强清单分层；首个 GET 必须先取得活动 uid。
//! 普通 Bearer + cosy-clienttype:10；不把普通 COSY 的 machineId 冒充活动 UMID。
//! 未接通增强步骤时保留普通清单与其明确下发的免费非每日活动，不推断所有促销都需 UMID。
use super::{
    account::{self, Snapshot},
    client::{self, Reply},
    daily, endpoints, refresh, risk, util,
};
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub struct Session {
    pub snapshot: Snapshot,
    pub list: Value,
    pub risk_ready: bool,
    pub risk_note: Option<String>,
    headers: Value,
    uid: String,
}
impl Session {
    pub fn binding(&self) -> String { util::hash(&json!([self.snapshot.credentials.binding(), self.uid]).to_string()) }
}
pub fn load() -> Result<Session> {
    let snapshot = refresh::ensure(false)?;
    match with_snapshot(snapshot.clone()) {
        Err(error) if error.status == Some(401) && snapshot.credentials.can_refresh() => {
            account::unchanged(&snapshot)?;
            let fresh = refresh::ensure(true)?;
            if fresh.credentials.binding() != snapshot.credentials.binding() {
                return Err(account::changed());
            }
            with_snapshot(fresh)
        }
        result => result,
    }
}
pub fn with_snapshot(snapshot: Snapshot) -> Result<Session> {
    let mut headers = client::sash_headers(&snapshot.credentials.access);
    let mut list = fetch(&snapshot, &headers, None)?;
    let uid = activity_uid(&list)?;
    let mut risk_ready = false;
    let risk_note = match risk::augment(&snapshot, &list) {
        Ok(augmented) => {
            // 保留原生顺序：以活动 uid 获取真实头后，再读增强目录；当前宿主门禁不会走到这里。
            headers = augmented;
            list = fetch(&snapshot, &headers, Some(&uid))?;
            risk_ready = true;
            None
        }
        Err(error) if risk::blocked(&error) => Some(error.message),
        Err(error) => return Err(error),
    };
    Ok(Session {
        snapshot,
        list,
        headers,
        uid,
        risk_ready,
        risk_note,
    })
}
pub fn unwrap_data(value: Value) -> Value {
    let mut data = match value.get("data") {
        Some(data) if data.is_object() => data.clone(),
        Some(data) if data.is_array() => json!({ "campaigns": data }),
        _ => return value,
    };
    // uid 在顶层的旧信封也必须保留，不能被 data 拆包丢掉后拿 userId 代替。
    if data.get("uid").is_none() {
        if let Some(uid) = value.get("uid") {
            data["uid"] = uid.clone();
        }
    }
    data
}
fn activity_uid(value: &Value) -> Result<String> {
    let Some(raw) = value.get("uid").filter(|value| !value.is_null()) else {
        return Ok(String::new());
    };
    let uid = raw
        .as_str()
        .ok_or_else(|| Error::http(502, "Qoder 活动用户标识格式无效"))?
        .trim();
    util::identity(uid).map_err(|_| Error::http(502, "Qoder 活动用户标识格式无效"))?;
    Ok(uid.into())
}
fn fetch(snapshot: &Snapshot, headers: &Value, expected_uid: Option<&str>) -> Result<Value> {
    account::unchanged(snapshot)?;
    let reply = client::request(
        "GET",
        endpoints::OPEN_API,
        &format!("{}?forceRefresh=true", endpoints::CAMPAIGNS),
        headers.clone(),
        None,
        "活动查询",
    )?;
    let value = unwrap_data(client::accepted(reply, "活动查询")?);
    if super::errors::has_error(&value) {
        return Err(super::errors::failure(502, &value));
    }
    let rows = value["campaigns"]
        .as_array()
        .filter(|rows| rows.len() <= 512)
        .ok_or_else(|| Error::http(502, "Qoder 活动清单缺少有效 campaigns"))?;
    let uid = activity_uid(&value)?;
    if expected_uid.is_some_and(|expected| expected != uid) {
        return Err(account::changed());
    }
    let mut ids = Vec::new();
    for row in rows {
        let id = row["campaignId"]
            .as_str()
            .filter(|id| id_valid(id))
            .ok_or_else(|| Error::http(502, "Qoder 活动清单缺少有效活动编号"))?;
        if ids.contains(&id) {
            return Err(Error::http(502, "Qoder 活动清单包含重复活动编号"));
        }
        ids.push(id);
    }
    account::unchanged(snapshot)?;
    Ok(value)
}
pub fn reload(session: &Session) -> Result<Value> { fetch(&session.snapshot, &session.headers, Some(&session.uid)) }
pub fn permit(session: &Session, row: &Value, is_daily: bool) -> Result<()> {
    if !credit(row) || daily(row) != is_daily {
        return Err(Error::http(400, "该活动不是本次指定的免费 Credits 权益"));
    }
    let now = util::now()?;
    if status(row, now) != "claimable" {
        return Err(Error::http(409, "Qoder 当前活动已不能领取，请重新查询"));
    }
    if is_daily {
        if !daily::current(row, now) {
            return Err(Error::http(409, "Qoder 当日活动窗口未确认，未领取旧轮次"));
        }
        if !session.risk_ready {
            return Err(risk::unavailable());
        }
    }
    account::unchanged(&session.snapshot)
}
pub fn submit(session: &Session, id: &str, is_daily: bool) -> Result<Reply> {
    if !id_valid(id) {
        return Err(Error::http(400, "请选择有效的 Qoder 活动"));
    }
    let row = session.list["campaigns"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|row| row["campaignId"] == id)
        .ok_or_else(|| Error::http(409, "Qoder 当前清单中没有所选活动，未提交领取"))?;
    permit(session, row, is_daily)?;
    let mut url = url::Url::parse(endpoints::OPEN_API).map_err(|_| Error::new("Qoder 活动接口配置无效"))?;
    url.set_path(endpoints::CAMPAIGNS);
    url.path_segments_mut()
        .map_err(|_| Error::new("Qoder 活动路径配置无效"))?
        .push(id)
        .push("claim");
    let mut headers = session.headers.clone();
    headers["origin"] = json!(endpoints::OPEN_API);
    client::request(
        "POST",
        endpoints::OPEN_API,
        url.path(),
        headers,
        Some(&json!({})),
        "活动领取",
    )
}
pub fn id_valid(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && !matches!(id, "." | "..")
        && util::printable(id, 256)
        && !id.contains(['/', '\\', '?', '#', '%'])
        && !id.bytes().any(|byte| byte.is_ascii_whitespace())
}
pub fn text(row: &Value, locale: &str, field: &str) -> String {
    ["POPUP", "USAGE"]
        .iter()
        .find_map(|kind| {
            row["placements"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|placement| placement["type"] == *kind)
                .find_map(|placement| {
                    placement["content"][locale][field]
                        .as_str()
                        .filter(|value| !value.trim().is_empty())
                })
        })
        .or_else(|| row[field].as_str())
        .unwrap_or("")
        .trim()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(4096)
        .collect()
}
pub fn credit(row: &Value) -> bool { row["actionType"] == "CLAIM_BENEFIT" && row["benefit"]["kind"] == "CREDITS" }
pub fn daily(row: &Value) -> bool {
    if !credit(row) {
        return false;
    }
    let title = format!("{} {}", text(row, "zh", "title"), text(row, "en", "title")).to_lowercase();
    let scope = &row["benefit"]["modelScope"];
    let scoped = scope["modelSeries"]["key"] != "ALL_MODELS"
        && !scope.is_null()
        && scope.as_object().is_none_or(|map| !map.is_empty());
    if scoped || title.contains("sonus") || title.contains("sota") {
        return false;
    }
    if ["每日", "每天", "daily", "every day"]
        .iter()
        .any(|part| title.contains(*part))
    {
        return true;
    }
    let one_day = match (util::timestamp(&row["startAt"]), util::timestamp(&row["endAt"])) {
        (Some(start), Some(end)) => end > start && end - start <= 86_400_000,
        _ => false,
    };
    one_day
        && util::number(&row["benefit"]["amount"]) == Some(100.0)
        && util::number(&row["benefit"]["validity"]["days"]) == Some(30.0)
}
pub fn status(row: &Value, now: i64) -> &'static str {
    if util::timestamp(&row["startAt"]).is_some_and(|start| start > now)
        || util::timestamp(&row["endAt"]).is_some_and(|end| end <= now)
    {
        return "inactive";
    }
    match row["claimStatus"].as_str() {
        Some("CLAIMED") => "claimed",
        Some("CLAIMABLE") => "claimable",
        _ => "ineligible",
    }
}
pub fn view(row: &Value, now: i64) -> Value {
    let title = text(row, "zh", "title");
    let en = text(row, "en", "title");
    json!({ "campaignId": row["campaignId"], "title": if title.is_empty() { if en.is_empty() { "Qoder 活动" } else { &en } } else { &title },
        "titleEn": en, "description": text(row, "zh", "description"), "descriptionEn": text(row, "en", "description"),
        "amount": util::number(&row["benefit"]["amount"]).filter(|amount| *amount >= 0.0), "status": status(row, now),
        "startsAt": util::timestamp(&row["startAt"]), "endsAt": util::timestamp(&row["endAt"]),
        "claimedAt": util::timestamp(&row["claimedAt"]),
        "expiresAt": util::timestamp(&row["expiresAt"]).or_else(|| util::timestamp(&row["benefit"]["validity"]["fixedEnd"])),
        "validDays": util::number(&row["benefit"]["validity"]["days"]).filter(|days| *days > 0.0) })
}
