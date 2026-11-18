//! 国际独立账号 state + 完整分页；无全局缓存，不读取国内目录。
//! 同账号好快照只有整次成功才 CAS 更新；catalog.accounts.v1 由宿主保存并聚合展示。

use crate::{
    budget::Budget,
    client,
    endpoints::ReadEndpoint,
    errors, models,
    snapshot::{self, Snapshot},
    util,
};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope},
    AccountLock, Error, Result, Value,
};
use std::collections::BTreeSet;

const KEY: &str = "monkeycode-intl.catalog";
const TTL_MS: i64 = 600_000;
const MAX_PAGES: usize = 32;
const MAX_BYTES: usize = 4 * 1024 * 1024;
const PAGE_LIMIT: &str = "128";

fn cached(state: &state::Snapshot, account: &Snapshot) -> Value {
    if state.compatible
        && state.value["binding"] == account.binding()
        && models::valid(&state.value["list"], Some(&account.guard))
    {
        state.value["list"].clone()
    } else {
        models::empty()
    }
}

fn mark_age(mut list: Value) -> Result<Value> {
    let at = list["meta"]["refreshed_at"].as_i64().unwrap_or(0);
    let age = util::now()?.saturating_sub(at);
    list["stale"] = json!(list["meta"]["remote"] != true || !(0..TTL_MS).contains(&age));
    Ok(list)
}

pub fn list(params: &Value) -> Result<Value> {
    let Some(account) = snapshot::selected(params)? else {
        // 账号快照的合并展示由宿主完成；无账号不能申请另一个账号的目录。
        return mark_age(models::empty());
    };
    let state = state::get(Scope::Account, KEY, 1)?;
    let list = mark_age(cached(&state, &account))?;
    account.unchanged()?;
    Ok(list)
}

fn fetch(account: &Snapshot, budget: &Budget) -> Result<Value> {
    let mut rows = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen = BTreeSet::new();
    let mut scanned = 0usize;
    let mut bytes = 0usize;
    let mut projected_bytes = 0usize;
    let mut guard = account.guard.clone();
    for page_number in 0..MAX_PAGES {
        budget.remaining()?;
        let mut query = vec![("limit", PAGE_LIMIT.to_string())];
        if let Some(cursor) = cursor.take() {
            query.push(("cursor", cursor));
        }
        account.unchanged()?;
        let result = client::get(&account.session, ReadEndpoint::Models, &query, budget);
        account.unchanged()?;
        let document = result?;
        bytes = bytes.saturating_add(document.bytes);
        if bytes > MAX_BYTES {
            return Err(models::too_large());
        }
        guard.observe(&document.data)?;
        let page = document.data["models"]
            .as_array()
            .ok_or_else(|| errors::protocol(ReadEndpoint::Models, "缺少 models 数组"))?;
        scanned = scanned.saturating_add(page.len());
        if scanned > models::MAX_ROWS {
            return Err(models::too_large());
        }
        for row in page {
            if let Some(row) = models::project(row, &guard)? {
                projected_bytes = projected_bytes.saturating_add(row.to_string().len());
                if projected_bytes > MAX_BYTES {
                    return Err(models::too_large());
                }
                rows.push(row);
            }
        }
        cursor = models::next(&document.data, &mut seen, &guard)?;
        if cursor.is_none() {
            budget.remaining()?;
            let list = models::finish(rows, util::now()?)?;
            // 后页新发现的 api_key 也不能存在于前页投影里。
            if !models::valid(&list, Some(&guard)) {
                return Err(errors::protocol(
                    ReadEndpoint::Models,
                    "目录安全投影校验失败",
                ));
            }
            return Ok(list);
        }
        if page_number + 1 == MAX_PAGES {
            return Err(models::too_large());
        }
    }
    Err(models::too_large())
}

fn outcome(mut list: Value, refreshed: bool) -> Value {
    list["refreshed"] = json!(refreshed);
    list["count"] = json!(list["models"].as_array().map(Vec::len).unwrap_or(0));
    list
}

pub fn refresh(params: &Value) -> Result<Value> {
    let Some(_) = snapshot::selected(params)? else {
        let mut result = outcome(list(params)?, false);
        result["message"] = json!("尚未指定国际账号，没有发起目录请求");
        return Ok(result);
    };
    let budget = Budget::new()?;
    let _lock = AccountLock::acquire()?;
    let account = snapshot::selected(params)?.ok_or_else(util::changed)?;
    let state = state::get(Scope::Account, KEY, 1)?;
    if !state.compatible && !state.value.is_null() {
        return Err(Error::http(409, "国际目录快照格式不兼容，未覆盖原账号状态"));
    }
    let previous = mark_age(cached(&state, &account))?;
    account.unchanged()?;
    budget.remaining()?;
    if params["force"] != true && previous["meta"]["remote"] == true && previous["stale"] == false {
        // 私有缓存命中时只补发缺失或落后的本账号宿主快照，不再联网。
        let shared = sdk::catalog::cached()?;
        account.unchanged()?;
        budget.remaining()?;
        let republish =
            shared["models"] != previous["models"] || shared["meta"] != previous["meta"];
        let mut result = outcome(previous, republish);
        result["cacheHit"] = json!(true);
        result["republished"] = json!(republish);
        return Ok(result);
    }
    let list = match fetch(&account, &budget) {
        Ok(list) => list,
        Err(error) => {
            account.unchanged()?;
            if errors::host_stop(&error) {
                return Err(error);
            }
            let mut result = outcome(previous, false);
            result["stale"] = json!(true);
            result["failure"] = errors::report(&error);
            result["message"] = json!(error.message);
            // 没有 CAS，没有以失败或半页清单覆盖旧好快照。
            return Ok(result);
        }
    };
    budget.remaining()?;
    account.unchanged()?;
    let value = json!({ "binding": account.binding(), "list": list });
    if value.to_string().len() > 256 * 1024 {
        return Err(models::too_large());
    }
    if !state::compare_exchange(Scope::Account, KEY, 1, state.revision, value, 0)? {
        return Err(Error::http(409, "国际目录已被另一轮刷新更新，请重新读取"));
    }
    account.unchanged()?;
    Ok(outcome(list, true))
}

pub fn target(params: &Value) -> Result<Value> {
    let account =
        snapshot::selected(params)?.ok_or_else(|| Error::http(400, "请先选择国际账号"))?;
    let requested = util::text(&params["payload"], "model")
        .ok_or_else(|| Error::http(400, "请填写完整展示模型 ID"))?;
    let mut list = list(params)?;
    if list["stale"] != false {
        list = mark_age(refresh(params)?)?;
    }
    account.unchanged()?;
    if list["stale"] != false {
        return Err(Error::http(503, "国际目录已过期，请先刷新；未使用过期映射"));
    }
    let matches: Vec<&Value> = list["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| {
            row["id"]
                .as_str()
                .is_some_and(|id| id.eq_ignore_ascii_case(requested))
        })
        .collect();
    if matches.len() != 1 {
        return Err(Error::http(
            403,
            "当前国际目录没有唯一模型映射，未模糊匹配或回落默认模型",
        ));
    }
    let model = matches
        .first()
        .copied()
        .ok_or_else(|| Error::http(403, "模型映射不可用"))?;
    let result = json!({ "id": model["id"], "model_id": model["upstreamId"], "model": model["model"],
        "displayOnly": true, "permissionsVerified": false, "chatReady": true,
        "message": "仅查询当前国际账号配置 UUID；不证明任务授权，没有创建任务" });
    account.unchanged()?;
    Ok(result)
}
