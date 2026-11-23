//! 账号绑定的目录快照，持久化交给 state.v1；十分钟自动刷新 TTL，无 WASM 静态缓存。
//! 失败不覆盖同账号旧清单，跨账号展示由宿主聚合，不当作完整任务授权。

use std::collections::BTreeSet;

use agent2api_wasm_sdk::{
    json,
    state::{self, Scope},
    AccountLock, Error, Result, Value,
};

use crate::{account, client, endpoints::ReadEndpoint, errors, models, subscription, util};

const KEY: &str = "monkeycode.catalog";
const TTL: i64 = 600_000;
const MAX_PAGES: usize = 32;
const MAX_CACHE: usize = 240 * 1024;

fn stale(list: &Value, now: i64) -> bool {
    let at = list["meta"]["refreshed_at"].as_i64().unwrap_or(0);
    let age = now.saturating_sub(at);
    list["meta"]["remote"] != true
        || !(0..TTL).contains(&age)
        || list["subscription"]["expiresAt"]
            .as_i64()
            .is_some_and(|expiry| expiry <= now)
}

fn selected(params: &Value) -> bool {
    !params["account"].as_str().unwrap_or("").is_empty()
}

fn cached(snapshot: &state::Snapshot, account: &account::Snapshot) -> Value {
    let value = &snapshot.value;
    if snapshot.compatible
        && value["binding"] == account.binding()
        && value["list"]["schema"] == 1
        && value["list"]["provider"] == "monkeycode"
        && models::valid(&value["list"])
    {
        value["list"].clone()
    } else {
        models::empty()
    }
}

pub fn list(params: &Value) -> Result<Value> {
    if !selected(params) {
        // 账号目录由宿主聚合；无账号的插件调用不能申请另一账号的快照。
        return Ok(models::empty());
    }
    let account = account::current()?;
    let snapshot = state::get(Scope::Account, KEY, 1)?;
    let mut list = cached(&snapshot, &account);
    list["stale"] = json!(stale(&list, util::now()?));
    account.unchanged()?;
    Ok(list)
}

fn fetch(account: &account::Snapshot) -> Result<Value> {
    let mut rows = Vec::new();
    let mut cursor = None;
    let mut cursors = BTreeSet::new();
    let mut pages = 0;
    let mut scanned = 0;
    let mut completed = None;
    let mut warnings = Vec::new();
    let subscription = match subscription::read(account) {
        Ok(value) => Some(value),
        Err(error) if errors::host_stop(&error) => return Err(error),
        Err(error) => {
            warnings.push(format!("{}；需要订阅确认的模型不会标为可用", error.message));
            None
        }
    };
    while pages < MAX_PAGES {
        let mut query = Vec::new();
        if let Some(cursor) = cursor.take() {
            query.push(("cursor", cursor));
        }
        account.unchanged()?;
        let result = client::get(&account.session, ReadEndpoint::Models, &query);
        account.unchanged()?;
        let data = result?;
        let page = data["models"]
            .as_array()
            .ok_or_else(|| errors::protocol(ReadEndpoint::Models, "缺少 models 数组"))?;
        scanned += page.len();
        if scanned > models::MAX_ROWS {
            return Err(models::too_large());
        }
        for row in page {
            if let Some(projected) = models::project(row, subscription.as_ref(), &account.session)?
            {
                rows.push(projected);
            }
        }
        pages += 1;
        let (next, complete) = models::cursor(&data, &mut cursors)?;
        if next
            .as_ref()
            .is_some_and(|cursor| cursor.contains(&account.session))
        {
            return Err(errors::protocol(
                ReadEndpoint::Models,
                "游标包含受保护内容，未继续分页",
            ));
        }
        completed = complete;
        if next.is_none() {
            break;
        }
        cursor = next;
    }
    if cursor.is_some() {
        return Err(models::too_large());
    }
    let mut list = models::finish(rows, util::now()?, completed)?;
    if list["ambiguousIds"].as_u64().unwrap_or(0) > 0 {
        warnings.push("存在同显示名、不同 UUID 的模型，未任选一项或广告该歧义名称".into());
    }
    if list["filteredOrUnknown"].as_u64().unwrap_or(0) > 0 {
        warnings.push(
            "部分可见模型未启用、缺少已确认权益或名称有歧义；完整审阅项保留在当前账号快照".into(),
        );
    }
    if completed.is_none() {
        warnings.push("上游没有返回分页完成标志，这只是展示清单，不代表完整权限目录".into());
    }
    list["warnings"] = json!(warnings);
    list["subscription"] = json!(subscription);
    if list.to_string().len() > MAX_CACHE {
        return Err(models::too_large());
    }
    Ok(list)
}

fn outcome(mut list: Value, refreshed: bool, message: Option<String>) -> Value {
    list["count"] = json!(list["models"].as_array().map(Vec::len).unwrap_or(0));
    list["refreshed"] = json!(refreshed);
    if let Some(message) = message {
        list["message"] = json!(message);
    }
    list
}

pub fn refresh(params: &Value) -> Result<Value> {
    if !selected(params) {
        return Ok(outcome(
            list(params)?,
            false,
            Some("尚未指定可用的国内账号，未发起目录请求".into()),
        ));
    }
    let _lock = AccountLock::acquire()?;
    let account = account::current()?;
    let snapshot = state::get(Scope::Account, KEY, 1)?;
    let previous = cached(&snapshot, &account);
    if params["force"] != true && !stale(&previous, util::now()?) {
        // 私有缓存命中也允许补发布缺失或落后的同账号宿主快照，不再联网。
        let host = agent2api_wasm_sdk::catalog::cached()?;
        account.unchanged()?;
        let publish = host["models"] != previous["models"] || host["meta"] != previous["meta"];
        return Ok(outcome(previous, publish, None));
    }
    let list = match fetch(&account) {
        Ok(list) => list,
        Err(error) if errors::host_stop(&error) => return Err(error),
        Err(error) => {
            account.unchanged()?;
            let mut result = outcome(previous, false, Some(error.message.clone()));
            result["stale"] = json!(true);
            result["failure"] = errors::report(&error);
            return Ok(result);
        }
    };
    account.unchanged()?;
    if !state::compare_exchange(
        Scope::Account,
        KEY,
        1,
        snapshot.revision,
        json!({ "binding": account.binding(), "list": list }),
        0,
    )? {
        return Err(Error::http(409, "MonkeyCode 目录已被其他刷新更新，请重试"));
    }
    account.unchanged()?;
    let message = list["warnings"]
        .as_array()
        .filter(|warnings| !warnings.is_empty())
        .map(|warnings| {
            warnings
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("；")
        });
    Ok(outcome(list, true, message))
}

/// 只读映射查询。必须有本账号快照；不能拿共享目录、显示名模糊匹配或默认模型代替。
pub fn target(params: &Value) -> Result<Value> {
    if !selected(params) {
        return Err(Error::http(400, "请指定国内账号后查询模型 UUID"));
    }
    let requested = util::text(&params["payload"], &["model"]);
    if requested.is_empty() {
        return Err(Error::http(400, "请指定完整的模型显示 ID"));
    }
    let original = account::current()?;
    let mut list = list(params)?;
    if list["stale"] != false {
        list = refresh(params)?;
        list["stale"] = json!(stale(&list, util::now()?));
    }
    original.unchanged()?;
    let rows = list["models"]
        .as_array()
        .ok_or_else(|| Error::http(503, "请先刷新当前账号目录"))?;
    if list["stale"] != false {
        return Err(Error::http(
            503,
            "当前账号目录已过期，请先刷新；没有使用过期权限映射",
        ));
    }
    let matched: Vec<&Value> = rows
        .iter()
        .filter(|row| util::text(row, &["id"]).eq_ignore_ascii_case(requested))
        .collect();
    if matched.len() != 1 {
        return Err(Error::http(
            403,
            "当前账号目录中没有唯一可用的模型映射，请刷新并确认权益",
        ));
    }
    let model = matched
        .first()
        .copied()
        .ok_or_else(|| Error::http(403, "当前账号模型映射不可用"))?;
    Ok(
        json!({ "id": model["id"], "model_id": model["upstreamId"], "model": model["model"],
        "displayOnly": true, "chatReady": true, "message": "仅提供本账号真实 UUID 映射；任务仅由本次真实对话请求授权" }),
    )
}
