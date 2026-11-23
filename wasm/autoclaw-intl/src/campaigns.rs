//! 活动福利只读预览、手动按项领取与兑换码兑换；查询不触发 user-info 或任务完成，在途不明不重放。
use super::{account, client, endpoints, errors, promotion, refresh, util};
use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};

const CLAIM_LEDGER: &str = "autoclawCampaignClaims";
const REDEEM_LEDGER: &str = "autoclawRedemption";
const TASK_LIST: &str = "/autoclaw-proxy/proxy/autoclaw-task-list?lang=zh-CN";
const TASK_COMPLETE: &str = "/autoclaw-proxy/proxy/autoclaw-task-complete";
const IDENTITY_TASKS: &str = "/agent-assetmgr/api/v1/identity-tasks?biz_app_id=autoclaw&client_version=2.2.0&os=win";
const IDENTITY_COMPLETE: &str = "/agent-assetmgr/api/v1/identity-tasks/complete";
const REDEEM_PATH: &str = "/agent-assetmgr/api/v1/autoglm-codes/redeem";
const CLIENT_VERSION: &str = "2.2.0";

enum Target {
    LoginActive(String),
    ClientTask { task_id: String, day: Option<String> },
    Newbie(String),
}

impl Target {
    fn parse(raw: &str) -> Option<Self> {
        let id = raw.trim();
        if let Some(day) = id.strip_prefix("login-active:") {
            return valid_day(day).then(|| Self::LoginActive(day.to_string()));
        }
        if let Some(rest) = id.strip_prefix("task:") {
            if let Some((task_id, day)) = rest.split_once(':') {
                if valid_id(task_id) && task_id != "daily_signin" && valid_day(day) {
                    return Some(Self::ClientTask { task_id: task_id.to_string(), day: Some(day.to_string()) });
                }
                return None;
            }
            if valid_id(rest) && rest != "daily_signin" {
                return Some(Self::ClientTask { task_id: rest.to_string(), day: None });
            }
            return None;
        }
        if let Some(category) = id.strip_prefix("newbie:") {
            return valid_id(category).then(|| Self::Newbie(category.to_string()));
        }
        None
    }
}

fn valid_id(text: &str) -> bool {
    !text.is_empty() && text.len() <= 96 && text.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

fn valid_day(text: &str) -> bool {
    text.len() == 10 && text.as_bytes().get(4) == Some(&b'-') && text.as_bytes().get(7) == Some(&b'-')
        && text.bytes().enumerate().all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
}

fn ledger_identity(snapshot: &account::Snapshot) -> String {
    let user = account::user(&snapshot.secret);
    let key = if user.is_empty() { util::hash(&account::access(&snapshot.secret)) } else { user };
    util::hash(&json!([endpoints::PROVIDER, snapshot.record["id"], key]).to_string())
}

fn load_ledger(kind: &str, bucket: &str, snapshot: &account::Snapshot, max_age_ms: i64) -> Result<Value> {
    let value = sdk::host("ledger.get", json!({ "kind": kind }))?;
    if !value.is_null() && !value.is_object() {
        return Err(Error::http(409, "AutoClaw 活动台账格式异常"));
    }
    let id = ledger_identity(snapshot);
    let mut root = if value["identity"] == id { value } else { json!({ "identity": id, bucket: {} }) };
    if !root[bucket].is_object() {
        root[bucket] = json!({});
    }
    if let Some(map) = root[bucket].as_object_mut() {
        let now = sdk::now();
        map.retain(|_, row| row["at"].as_i64().is_some_and(|at| now.saturating_sub(at) < max_age_ms));
    }
    Ok(root)
}

fn save_ledger(kind: &str, snapshot: &account::Snapshot, value: &Value) -> Result<()> {
    if !account::same(snapshot, &account::read()?) {
        return Err(Error::http(409, "AutoClaw 账号已变化，未写入活动台账"));
    }
    if value.to_string().len() > 60 * 1024 {
        return Err(Error::http(409, "AutoClaw 活动台账容量已满"));
    }
    sdk::host("ledger.put", json!({ "kind": kind, "value": value })).map(|_| ())
}

fn ensure_claim_capacity(ledger: &Value, id: &str) -> Result<()> {
    if ledger["claims"]
        .as_object()
        .is_some_and(|map| map.len() >= 128 && !map.contains_key(id))
    {
        return Err(Error::http(409, "AutoClaw 活动记录过多，请稍后重试"));
    }
    Ok(())
}

fn set_claim_state(
    snapshot: &account::Snapshot,
    ledger: &mut Value,
    id: &str,
    status: &str,
    day: Option<&str>,
) -> Result<()> {
    ensure_claim_capacity(ledger, id)?;
    let mut entry = json!({ "status": status, "at": sdk::now() });
    if let Some(day) = day {
        entry["day"] = json!(day);
    }
    ledger["claims"][id] = entry;
    save_ledger(CLAIM_LEDGER, snapshot, ledger)
}

fn clear_claim_state(snapshot: &account::Snapshot, ledger: &mut Value, id: &str) -> Result<()> {
    if let Some(claims) = ledger["claims"].as_object_mut() {
        if claims.remove(id).is_some() {
            save_ledger(CLAIM_LEDGER, snapshot, ledger)?;
        }
    }
    Ok(())
}

fn is_auth_error(error: &Error) -> bool {
    error.status == Some(401) || error.upstream_code.is_some_and(client::auth_code)
}

fn is_fatal_source(error: &Error) -> bool {
    is_auth_error(error) || error.code.as_deref() == Some("forbidden")
}

fn with_refresh<T, F>(mut op: F) -> Result<T>
where
    F: FnMut(&account::Snapshot) -> Result<T>,
{
    let _ = refresh::ensure()?;
    let initial = account::read()?;
    match op(&initial) {
        Err(error) if is_auth_error(&error) && !initial.local => {
            if !account::same(&initial, &account::read()?) {
                return Err(Error::http(409, "AutoClaw 账号已变化，请重新操作"));
            }
            refresh::force()?;
            let fresh = account::read()?;
            if ledger_identity(&fresh) != ledger_identity(&initial) {
                return Err(Error::http(409, "AutoClaw 续期后账号身份不一致，已中止操作"));
            }
            op(&fresh)
        }
        other => other,
    }
}

fn campaign_row(
    id: String,
    title: String,
    title_en: String,
    description: String,
    description_en: String,
    amount: Option<i64>,
    status: &str,
) -> Value {
    json!({
        "campaignId": id,
        "title": title,
        "titleEn": title_en,
        "description": description,
        "descriptionEn": description_en,
        "amount": amount,
        "unit": "积分",
        "status": status,
        "startsAt": null,
        "endsAt": null,
        "claimedAt": null,
        "expiresAt": null,
        "validDays": null
    })
}

fn build_login_campaign(report: &Value, today: &str, saved: &Value) -> Value {
    let granted = report["granted"] == true;
    let confirming = saved["status"] == "confirming";
    let status = if granted {
        "claimed"
    } else if confirming {
        "confirming"
    } else if report["incomplete"] == true {
        "ineligible"
    } else {
        "claimable"
    };
    let latest_hint = if !granted {
        let latest = &report["latest"];
        let gdate = util::text(latest, &["grantDate"]);
        let pts = util::integer(&latest["points"]);
        if !gdate.is_empty() {
            match pts {
                Some(p) if p > 0 => format!(" 最近活动入账：{gdate} +{p} 积分。"),
                _ => format!(" 最近活动入账日期：{gdate}。"),
            }
        } else {
            String::new()
        }
    } else {
        String::new()
    };
    let desc = if granted {
        let detail = util::text(report, &["description"]);
        if detail.is_empty() {
            format!("今日（{today}）登录活跃 / Weekly Perks 福利已记录入账。")
        } else {
            format!("今日已入账：{detail}")
        }
    } else if report["incomplete"] == true {
        "今日积分收入记录超过单页上限，暂无法确认是否已到账。".to_string()
    } else {
        format!(
            "手动触发桌面活跃上报以核对当前周期登录福利或 Weekly Perks 奖励（实际是否发放以官方账户流水为准）。{latest_hint}"
        )
    };
    campaign_row(
        format!("login-active:{today}"),
        "登录活跃 / Weekly Perks 福利".to_string(),
        "Login Active & Weekly Perks".to_string(),
        desc,
        "Report desktop client activity to check and claim active login or Weekly Perks credits based on upstream ledger.".to_string(),
        if granted { util::integer(&report["points"]) } else { None },
        status,
    )
}

fn fetch_client_tasks(secret: &Value) -> Result<Vec<Value>> {
    let payload = client::get(TASK_LIST, &account::access(secret), "任务列表查询")?;
    client::business(&payload, "查询任务列表")?;
    Ok(payload["data"].as_array().cloned().unwrap_or_default())
}

fn client_task_id(task: &Value) -> String {
    util::text(task, &["task_id"])
}

fn client_task_done(task: &Value) -> bool {
    task["completed"] == true
        || matches!(
            util::text(task, &["status"]).to_ascii_lowercase().as_str(),
            "completed" | "claimed" | "done"
        )
}

fn client_task_campaign_id(task_id: &str, today: &str) -> String {
    if task_id.starts_with("daily_") {
        format!("task:{task_id}:{today}")
    } else {
        format!("task:{task_id}")
    }
}

fn map_client_task(task: &Value, today: &str, claims: &Value) -> Option<Value> {
    if task["display"] == false {
        return None;
    }
    let task_id = client_task_id(task);
    if !valid_id(&task_id) || task_id == "daily_signin" {
        return None;
    }
    let campaign_id = client_task_campaign_id(&task_id, today);
    let done = client_task_done(task);
    let triggerable = task["client_triggered"] == true || task_id == "daily_inspiration_center";
    let status = if done {
        "claimed"
    } else if claims[&campaign_id]["status"] == "confirming" {
        "confirming"
    } else if triggerable {
        "claimable"
    } else {
        "ineligible"
    };
    let title = {
        let raw = util::text(task, &["title", "name"]);
        if raw.is_empty() { task_id.clone() } else { raw }
    };
    let desc = {
        let raw = util::text(task, &["status_description", "point_description", "description"]);
        if status == "ineligible" {
            let hint = if task_id == "upgrade_pc_app" {
                "需在官方 AutoClaw 桌面客户端升级至最新版本，无法由网关代领。"
            } else {
                "需在官方客户端完成对应动作后才可领取，插件不伪造上报。"
            };
            if raw.is_empty() { hint.to_string() } else { format!("{raw}（{hint}）") }
        } else if raw.is_empty() {
            "官方客户端积分任务。".to_string()
        } else {
            raw
        }
    };
    Some(campaign_row(
        campaign_id,
        title.clone(),
        title,
        desc,
        "Official AutoClaw client credit task.".to_string(),
        util::integer(&task["reward_points"]),
        status,
    ))
}

fn fetch_newbie_tasks(secret: &Value) -> Result<(bool, Vec<Value>)> {
    let payload = client::get(IDENTITY_TASKS, &account::access(secret), "新手任务查询")?;
    client::business(&payload, "查询新手任务")?;
    let data = &payload["data"];
    let all_done = data["all_tasks_completed"] == true;
    let tasks = data["tasks"].as_array().cloned().unwrap_or_default();
    Ok((all_done, tasks))
}

fn newbie_category(task: &Value) -> String {
    util::text(task, &["task_category", "category", "id"])
}

fn newbie_status(task: &Value) -> String {
    util::text(task, &["status"]).to_ascii_lowercase()
}

fn map_newbie_task(task: &Value, claims: &Value) -> Option<Value> {
    let category = newbie_category(task);
    if !valid_id(&category) {
        return None;
    }
    let campaign_id = format!("newbie:{category}");
    let raw_status = newbie_status(task);
    let status = if matches!(raw_status.as_str(), "completed" | "claimed" | "done") {
        "claimed"
    } else if claims[&campaign_id]["status"] == "confirming" {
        "confirming"
    } else if raw_status == "claim" {
        "claimable"
    } else {
        "ineligible"
    };
    let title = {
        let raw = util::text(task, &["title", "task_name", "name"]);
        if raw.is_empty() { format!("新手任务 · {category}") } else { raw }
    };
    let desc = {
        let raw = util::text(task, &["description", "desc", "status_description"]);
        if status == "ineligible" {
            if raw.is_empty() {
                "需先完成对应新手引导任务条件后才可领取奖励，插件不代做任务。".to_string()
            } else {
                format!("{raw}（需先满足条件后才可领取）")
            }
        } else if raw.is_empty() {
            "新手任务条件已达成，可点击领取积分。".to_string()
        } else {
            raw
        }
    };
    Some(campaign_row(
        campaign_id,
        title.clone(),
        title,
        desc,
        "Official AutoClaw newbie reward task.".to_string(),
        util::integer(&task["reward_points"]),
        status,
    ))
}

pub fn preview() -> Result<Value> {
    with_refresh(preview_once)
}

fn preview_once(snapshot: &account::Snapshot) -> Result<Value> {
    let today = util::day()?;
    let ledger = load_ledger(CLAIM_LEDGER, "claims", snapshot, 30 * 86_400_000)?;
    let claims = &ledger["claims"];
    let mut campaigns = Vec::new();
    let mut notices = Vec::new();
    let mut first_error: Option<Error> = None;
    let mut any_ok = false;

    match promotion::inspect(&snapshot.secret) {
        Ok(report) => {
            any_ok = true;
            campaigns.push(build_login_campaign(&report, &today, &claims[format!("login-active:{today}")]));
        }
        Err(error) if is_fatal_source(&error) => return Err(error),
        Err(error) => {
            notices.push(format!("登录活跃福利查询失败：{}", error.message));
            first_error.get_or_insert(error);
        }
    }

    match fetch_client_tasks(&snapshot.secret) {
        Ok(tasks) => {
            any_ok = true;
            for task in &tasks {
                if let Some(row) = map_client_task(task, &today, claims) {
                    campaigns.push(row);
                }
            }
        }
        Err(error) if is_fatal_source(&error) => return Err(error),
        Err(error) => {
            notices.push(format!("客户端任务查询失败：{}", error.message));
            first_error.get_or_insert(error);
        }
    }

    match fetch_newbie_tasks(&snapshot.secret) {
        Ok((all_done, tasks)) => {
            any_ok = true;
            let has_claimable = tasks.iter().any(|t| newbie_status(t) == "claim");
            if !all_done || has_claimable {
                for task in &tasks {
                    if let Some(row) = map_newbie_task(task, claims) {
                        campaigns.push(row);
                    }
                }
            }
        }
        Err(error) if is_fatal_source(&error) => return Err(error),
        Err(error) => {
            notices.push(format!("新手任务查询提示：{}", error.message));
            first_error.get_or_insert(error);
        }
    }

    if !account::same(snapshot, &account::read()?) {
        return Err(Error::http(409, "AutoClaw 账号已变化，请重新查询活动"));
    }
    if !any_ok {
        return Err(first_error.unwrap_or_else(|| Error::http(502, "AutoClaw 活动列表查询失败")));
    }

    let notice = if notices.is_empty() { Value::Null } else { json!(notices.join("；")) };
    Ok(json!({
        "campaigns": campaigns,
        "riskNote": null,
        "notice": notice,
        "redemption": { "supported": true }
    }))
}

fn outcome_json(id: &str, outcome: &str, campaign: Option<Value>, message: &str) -> Value {
    json!({
        "campaignId": id,
        "outcome": outcome,
        "campaign": campaign,
        "message": message
    })
}

pub fn claim(id: &str) -> Result<Value> {
    let raw = id.trim();
    if Target::parse(raw).is_none() {
        return Ok(outcome_json(raw, "inactive", None, "不支持的 AutoClaw 活动编号"));
    }
    with_refresh(|initial| {
        let _lock = AccountLock::acquire()?;
        let snapshot = account::read()?;
        if !account::same(initial, &snapshot) {
            return Err(Error::http(409, "AutoClaw 账号已变化，未提交领取"));
        }
        claim_locked(raw, &snapshot)
    })
}

fn claim_locked(id: &str, snapshot: &account::Snapshot) -> Result<Value> {
    let today = util::day()?;
    let target = match Target::parse(id) {
        Some(target) => target,
        None => return Ok(outcome_json(id, "inactive", None, "活动标识无效")),
    };
    let mut ledger = load_ledger(CLAIM_LEDGER, "claims", snapshot, 30 * 86_400_000)?;
    match target {
        Target::LoginActive(day) => {
            if day != today {
                return Ok(outcome_json(id, "inactive", None, "该登录活动日期已过期，请刷新列表"));
            }
            claim_login_active(id, &today, snapshot, &mut ledger)
        }
        Target::ClientTask { task_id, day } => {
            if day.as_deref().is_some_and(|d| d != today) {
                return Ok(outcome_json(id, "inactive", None, "该每日任务日期已过期，请刷新列表"));
            }
            claim_client_task(id, &task_id, &today, snapshot, &mut ledger)
        }
        Target::Newbie(category) => claim_newbie_task(id, &category, snapshot, &mut ledger),
    }
}

fn claim_login_active(id: &str, today: &str, snapshot: &account::Snapshot, ledger: &mut Value) -> Result<Value> {
    let before = promotion::inspect(&snapshot.secret)?;
    if before["granted"] == true {
        set_claim_state(snapshot, ledger, id, "claimed", Some(today))?;
        let row = build_login_campaign(&before, today, &ledger["claims"][id]);
        return Ok(outcome_json(id, "already_claimed", Some(row), "今日登录活跃 / Weekly Perks 福利已到账"));
    }
    if before["incomplete"] == true {
        let row = build_login_campaign(&before, today, &ledger["claims"][id]);
        return Ok(outcome_json(id, "confirming", Some(row), "今日收入流水超过单页上限，暂无法确认是否已到账，已暂停重发上报"));
    }
    let recent_confirming = ledger["claims"][id]["status"] == "confirming"
        && sdk::now().saturating_sub(ledger["claims"][id]["at"].as_i64().unwrap_or(0)) < 120_000;
    if recent_confirming {
        let row = build_login_campaign(&before, today, &ledger["claims"][id]);
        return Ok(outcome_json(id, "confirming", Some(row), "上次活跃上报仍在确认冷却期，请稍后再试或先核对流水"));
    }
    set_claim_state(snapshot, ledger, id, "confirming", Some(today))?;

    match promotion::activate(&snapshot.secret) {
        Ok(()) => {
            sdk::sleep(1500)?;
            if !account::same(snapshot, &account::read()?) {
                return Err(Error::http(409, "AutoClaw 账号已变化，请重新查询流水"));
            }
            let after = promotion::inspect(&snapshot.secret)?;
            if after["granted"] == true {
                set_claim_state(snapshot, ledger, id, "claimed", Some(today))?;
                let row = build_login_campaign(&after, today, &ledger["claims"][id]);
                return Ok(outcome_json(id, "claimed", Some(row), "已触发客户端活跃并核对今日福利到账"));
            }
            if after["incomplete"] == true {
                let row = build_login_campaign(&after, today, &ledger["claims"][id]);
                return Ok(outcome_json(id, "confirming", Some(row), "已上报活跃状态，但今日收入流水未完整，请稍后核对"));
            }
            clear_claim_state(snapshot, ledger, id)?;
            let mut row = build_login_campaign(&after, today, &Value::Null);
            row["status"] = json!("ineligible");
            row["description"] = json!("已上报客户端活跃状态，官方今日暂未发放新奖励（可能本周已领过、不在活动期或暂不符合条件）。");
            Ok(outcome_json(id, "ineligible", Some(row), "已完成活跃上报，今日暂无可发放的新福利"))
        }
        Err(error) if is_auth_error(&error) => {
            clear_claim_state(snapshot, ledger, id)?;
            Err(error)
        }
        Err(error) => {
            let row = build_login_campaign(&before, today, &ledger["claims"][id]);
            Ok(outcome_json(id, "confirming", Some(row), &format!("活跃上报未确认完成（{}），请稍后刷新核对流水", error.message)))
        }
    }
}

fn claim_client_task(id: &str, task_id: &str, today: &str, snapshot: &account::Snapshot, ledger: &mut Value) -> Result<Value> {
    let tasks = fetch_client_tasks(&snapshot.secret)?;
    let Some(task) = tasks.iter().find(|t| client_task_id(t) == task_id && t["display"] != false) else {
        return Ok(outcome_json(id, "inactive", None, "当前客户端任务列表未开启此任务"));
    };
    if client_task_done(task) {
        set_claim_state(snapshot, ledger, id, "claimed", Some(today))?;
        let row = map_client_task(task, today, &ledger["claims"]);
        return Ok(outcome_json(id, "already_claimed", row, "该任务今日已完成领取"));
    }
    if task["client_triggered"] != true && task_id != "daily_inspiration_center" {
        let row = map_client_task(task, today, &ledger["claims"]);
        return Ok(outcome_json(id, "ineligible", row, "该任务需在官方客户端完成实际操作后才可领取"));
    }
    if ledger["claims"][id]["status"] == "confirming" {
        let row = map_client_task(task, today, &ledger["claims"]);
        return Ok(outcome_json(id, "confirming", row, "上次提交结果待确认，已重查任务列表仍未完成，暂不重复提交"));
    }
    set_claim_state(snapshot, ledger, id, "confirming", Some(today))?;

    let reply = client::request("POST", TASK_COMPLETE, &account::access(&snapshot.secret), Some(&json!({ "task_id": task_id })), "客户端任务领取");
    handle_task_reply(id, reply, snapshot, ledger, |s, claims| {
        let latest_tasks = fetch_client_tasks(&s.secret)?;
        let current = latest_tasks.iter().find(|t| client_task_id(t) == task_id);
        let done = current.is_some_and(client_task_done);
        let view = current.and_then(|t| map_client_task(t, today, claims));
        Ok((done, view))
    })
}

fn claim_newbie_task(id: &str, category: &str, snapshot: &account::Snapshot, ledger: &mut Value) -> Result<Value> {
    let (_, tasks) = fetch_newbie_tasks(&snapshot.secret)?;
    let Some(task) = tasks.iter().find(|t| newbie_category(t) == category) else {
        return Ok(outcome_json(id, "inactive", None, "当前账号新手任务列表未找到该项"));
    };
    let raw_status = newbie_status(task);
    if matches!(raw_status.as_str(), "completed" | "claimed" | "done") {
        set_claim_state(snapshot, ledger, id, "claimed", None)?;
        let row = map_newbie_task(task, &ledger["claims"]);
        return Ok(outcome_json(id, "already_claimed", row, "该新手任务奖励已领取"));
    }
    if raw_status != "claim" {
        let row = map_newbie_task(task, &ledger["claims"]);
        return Ok(outcome_json(id, "ineligible", row, "该新手任务尚不满足领取条件"));
    }
    if ledger["claims"][id]["status"] == "confirming" {
        let row = map_newbie_task(task, &ledger["claims"]);
        return Ok(outcome_json(id, "confirming", row, "上次领取结果待确认，已重查新手任务仍可领取，暂不重复提交"));
    }
    set_claim_state(snapshot, ledger, id, "confirming", None)?;

    let body = json!({
        "biz_app_id": "autoclaw",
        "client_version": CLIENT_VERSION,
        "os": "win",
        "source_id": "autoclaw",
        "device_id": account::device(&snapshot.secret),
        "task_category": category
    });
    let reply = client::request("POST", IDENTITY_COMPLETE, &account::access(&snapshot.secret), Some(&body), "新手任务领取");
    handle_task_reply(id, reply, snapshot, ledger, |s, claims| {
        let (_, latest_tasks) = fetch_newbie_tasks(&s.secret)?;
        let current = latest_tasks.iter().find(|t| newbie_category(t) == category);
        let done = current.is_some_and(|t| matches!(newbie_status(t).as_str(), "completed" | "claimed" | "done"));
        let view = current.and_then(|t| map_newbie_task(t, claims));
        Ok((done, view))
    })
}

fn handle_task_reply<F>(id: &str, reply: Result<client::Reply>, snapshot: &account::Snapshot, ledger: &mut Value, verify: F) -> Result<Value>
where
    F: Fn(&account::Snapshot, &Value) -> Result<(bool, Option<Value>)>,
{
    let mut rejected: Option<String> = None;
    let mut already = false;
    match reply {
        Ok(res) => {
            let biz = errors::code(&res.body);
            if res.status == 401 || biz.is_some_and(client::auth_code) {
                clear_claim_state(snapshot, ledger, id)?;
                return Err(errors::failure(401, &res.body));
            }
            if (200..300).contains(&res.status) && biz.is_none_or(|c| c == 0) && !errors::has_error(&res.body) {
                already = res.body["data"]["already_completed"] == true;
            } else if (400..500).contains(&res.status) && !matches!(res.status, 408 | 429) || biz.is_some_and(|c| c != 0) {
                rejected = Some(errors::append_detail("官方拒绝领取", &res.body));
            }
        }
        Err(error) if is_auth_error(&error) => {
            clear_claim_state(snapshot, ledger, id)?;
            return Err(error);
        }
        Err(_) => {}
    }

    if let Ok((mut done, mut view)) = verify(snapshot, &ledger["claims"]) {
        if !done && rejected.is_none() {
            sdk::sleep(800)?;
            if !account::same(snapshot, &account::read()?) {
                return Err(Error::http(409, "AutoClaw 账号已变化，请重新核对活动状态"));
            }
            if let Ok((recheck_done, recheck_view)) = verify(snapshot, &ledger["claims"]) {
                done = recheck_done;
                view = recheck_view;
            }
        }
        if done {
            set_claim_state(snapshot, ledger, id, "claimed", None)?;
            let outcome = if already { "already_claimed" } else { "claimed" };
            return Ok(outcome_json(id, outcome, view, if already { "该奖励此前已领取" } else { "奖励领取成功" }));
        }
        if let Some(reason) = rejected {
            clear_claim_state(snapshot, ledger, id)?;
            return Ok(outcome_json(id, "failed", view, &reason));
        }
        return Ok(outcome_json(id, "confirming", view, "领取请求结果待确认，请稍后刷新核对状态"));
    }

    if let Some(reason) = rejected {
        clear_claim_state(snapshot, ledger, id)?;
        return Ok(outcome_json(id, "failed", None, &reason));
    }
    Ok(outcome_json(id, "confirming", None, "领取请求未确认结果，为避免重复提交已暂停重放，请稍后查询核对"))
}

pub fn redeem(payload: &Value) -> Result<Value> {
    if payload["confirmed"] == false {
        return Err(Error::http(400, "请确认将兑换码积分兑换到当前 AutoClaw 账号"));
    }
    let code = util::text(payload, &["code"]);
    if code.is_empty() || code.len() > 64 || code.chars().any(char::is_control) {
        return Err(Error::http(400, "请填写有效的 AutoClaw 兑换码"));
    }
    let key = util::hash(&code);
    with_refresh(|initial| {
        let _lock = AccountLock::acquire()?;
        let snapshot = account::read()?;
        if !account::same(initial, &snapshot) {
            return Err(Error::http(409, "AutoClaw 账号已变化，未提交兑换"));
        }
        let mut ledger = load_ledger(REDEEM_LEDGER, "codes", &snapshot, 90 * 86_400_000)?;
        match ledger["codes"][&key]["state"].as_str() {
            Some("succeeded") => return Ok(json!({ "outcome": "already_claimed", "message": "该兑换码已在当前账号兑换过" })),
            Some("confirming") => return Ok(json!({ "outcome": "confirming", "message": "上次兑换结果待确认，请先核对官方积分明细；不会重复提交" })),
            _ => {}
        }
        if ledger["codes"].as_object().is_some_and(|map| map.len() >= 120) {
            return Err(Error::http(409, "AutoClaw 兑换记录过多，请稍后清理后再试"));
        }
        ledger["codes"][&key] = json!({ "state": "confirming", "at": sdk::now() });
        save_ledger(REDEEM_LEDGER, &snapshot, &ledger)?;

        let reply = client::request("POST", REDEEM_PATH, &account::access(&snapshot.secret),
            Some(&json!({ "biz_app_id": "autoclaw", "code": code })), "兑换码兑换");
        match reply {
            Ok(res) => {
                let biz = errors::code(&res.body);
                if res.status == 401 || biz.is_some_and(client::auth_code) {
                    if let Some(map) = ledger["codes"].as_object_mut() { map.remove(&key); }
                    save_ledger(REDEEM_LEDGER, &snapshot, &ledger)?;
                    return Err(errors::failure(401, &res.body));
                }
                if (200..300).contains(&res.status) && biz == Some(0) {
                    ledger["codes"][&key] = json!({ "state": "succeeded", "at": sdk::now() });
                    save_ledger(REDEEM_LEDGER, &snapshot, &ledger)?;
                    return Ok(json!({ "outcome": "claimed", "message": "AutoClaw 兑换码兑换成功" }));
                }
                if (400..500).contains(&res.status) && !matches!(res.status, 408 | 429) || biz.is_some_and(|c| c != 0) {
                    if let Some(map) = ledger["codes"].as_object_mut() { map.remove(&key); }
                    save_ledger(REDEEM_LEDGER, &snapshot, &ledger)?;
                    let msg = match biz {
                        Some(650_101) => "该兑换码已使用".to_string(),
                        Some(650_102) => "兑换码已过期".to_string(),
                        _ => errors::append_detail("AutoClaw 兑换码兑换失败", &res.body),
                    };
                    return Ok(json!({ "outcome": "failed", "message": msg }));
                }
                Ok(json!({ "outcome": "confirming", "message": "兑换结果待确认，请核对官方积分明细；不会重复提交" }))
            }
            Err(error) if is_auth_error(&error) => {
                if let Some(map) = ledger["codes"].as_object_mut() { map.remove(&key); }
                save_ledger(REDEEM_LEDGER, &snapshot, &ledger)?;
                Err(error)
            }
            Err(_) => Ok(json!({ "outcome": "confirming", "message": "兑换请求未获确认结果，请核对官方积分明细；不会重复提交" })),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_campaign_targets_and_rejects_daily_signin() {
        assert!(matches!(Target::parse("login-active:2026-11-23"), Some(Target::LoginActive(d)) if d == "2026-11-23"));
        assert!(Target::parse("login-active:bad-date").is_none());
        assert!(matches!(
            Target::parse("task:daily_inspiration_center:2026-11-23"),
            Some(Target::ClientTask { task_id, day: Some(day) }) if task_id == "daily_inspiration_center" && day == "2026-11-23"
        ));
        assert!(Target::parse("task:daily_signin:2026-11-23").is_none());
        assert!(Target::parse("task:daily_signin").is_none());
        assert!(matches!(Target::parse("newbie:bind_im"), Some(Target::Newbie(c)) if c == "bind_im"));
        assert!(Target::parse("newbie:bad:id").is_none());
    }

    #[test]
    fn maps_client_newbie_and_login_campaigns_honestly() {
        let claims = json!({ "task:upgrade_pc_app": { "status": "confirming" } });
        let insp = map_client_task(&json!({ "task_id": "daily_inspiration_center", "title": "每日浏览灵感中心", "reward_points": 50, "status": "pending" }), "2026-11-23", &claims).unwrap();
        assert_eq!(insp["status"], "claimable");
        assert_eq!(insp["amount"], 50);
        let newbie = map_newbie_task(&json!({ "task_category": "first_chat", "title": "首次对话", "reward_points": 200, "status": "claim" }), &claims).unwrap();
        assert_eq!(newbie["status"], "claimable");
        let todo = map_newbie_task(&json!({ "task_category": "bind_im", "title": "绑定微信", "reward_points": 200, "status": "todo" }), &claims).unwrap();
        assert_eq!(todo["status"], "ineligible");
        let login = build_login_campaign(&json!({ "granted": false, "incomplete": false, "latest": { "grantDate": "2026-11-16", "points": 4000 } }), "2026-11-23", &Value::Null);
        assert_eq!(login["status"], "claimable");
        assert!(login["description"].as_str().unwrap().contains("2026-11-16 +4000"));
    }
}
