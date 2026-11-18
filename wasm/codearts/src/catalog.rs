//! 目录读取在同一次限时调用内推进，未完成进度仍按账号保存。
//! 共享目录不作为账号完整权限表。

use agent2api_wasm_sdk::{self as sdk, json, Result, Value};

use crate::{
    binding::{self, Account},
    catalog_fetch,
    catalog_state::{Attempt, Job, Snapshot, Stage, Store},
    errors, gates, models, util,
};

const CLEAN_TTL: i64 = 5 * 60_000;
const WARNING_TTL: i64 = 30_000;
const REQUESTS_PER_CALL: usize = 128;
const START_MORE_BEFORE_MS: i64 = 80_000;

fn empty() -> Value {
    json!({ "models": [], "meta": { "remote": false, "refreshed_at": 0 } })
}
fn annotate(mut list: Value) -> Value {
    list["catalogOnly"] = json!(true);
    list["permissionComplete"] = json!(false);
    list["migrationStage"] = json!("complete");
    list["message"] =
        json!("模型目录按账号隔离；目录缺项不代表上游拒绝该模型，实际权限由本次请求确认");
    list
}

fn from_snapshot(snapshot: &Snapshot, account: &Account) -> Result<Value> {
    let entries = models::entries(&snapshot.models, &account.credential.secrets())?;
    Ok(annotate(
        json!({ "models": entries, "meta": { "remote": true, "refreshed_at": snapshot.fetched_at },
        "sourceStatus": snapshot.sources, "sourcesComplete": snapshot.sources.successful() }),
    ))
}

fn private(store: &Store, account: &Account) -> Result<Value> {
    match store.good(&account.binding) {
        Some(snapshot) => from_snapshot(snapshot, account),
        None => Ok(annotate(empty())),
    }
}

pub fn list(params: &Value) -> Result<Value> {
    if !binding::has_account(params) {
        return shared_display();
    }
    let account = Account::current()?;
    let store = Store::load()?;
    store.validate_current(&account)?;
    let mut list = private(&store, &account)?;
    if let Some(job) = store
        .cache
        .job
        .as_ref()
        .filter(|job| job.binding == account.binding)
    {
        list["progress"] = job.public();
    }
    if let Some(last) = store
        .cache
        .last_attempt
        .as_ref()
        .filter(|last| last.binding == account.binding)
    {
        list["lastAttempt"] =
            json!({ "at": last.at, "nonempty": last.nonempty, "sources": last.sources });
    }
    account.unchanged()?;
    Ok(list)
}

fn shared_display() -> Result<Value> {
    // 无账号时不读 catalog.get：该能力只返回本次账号快照，宿主负责跨账号聚合。
    let mut value = annotate(empty());
    value["sourceScope"] = json!("host_aggregated_accounts");
    Ok(value)
}

pub fn refresh(params: &Value) -> Result<Value> {
    if !binding::has_account(params) {
        return Ok(
            json!({ "refreshed": false, "count": 0, "message": "没有指定 CodeArts 账号，未读取其他账号或请求上游" }),
        );
    }
    let _lock = sdk::AccountLock::acquire().map_err(errors::host)?;
    let account = Account::current()?; // 锁内重新读，避免并发目录覆盖或导入前的凭据快照。
    let started = util::now()?;
    let mut store = Store::load()?;
    store.validate_current(&account)?;
    if store
        .cache
        .job
        .as_ref()
        .is_some_and(|job| job.binding != account.binding)
    {
        store.cache.job = None;
    }
    if params["force"] != true && store.cache.job.is_none() {
        if let Some(previous) = store.good(&account.binding) {
            let ttl = if previous.sources.successful() {
                CLEAN_TTL
            } else {
                WARNING_TTL
            };
            let age = started.saturating_sub(previous.fetched_at);
            if (0..ttl).contains(&age) {
                return result(&store, &account, false, false, "已保留本账号最近的展示目录");
            }
        }
    }
    account.readable()?;
    let resumable = store
        .cache
        .job
        .as_ref()
        .is_some_and(|job| job.current(&account.binding, started));
    if !resumable {
        store.cache.job = Some(Job::new(account.binding.clone(), started));
        store.save(&account)?;
    }
    for index in 0..REQUESTS_PER_CALL {
        if index > 0 && util::now()?.saturating_sub(started) >= START_MORE_BEFORE_MS {
            break;
        }
        if store
            .cache
            .job
            .as_ref()
            .is_some_and(|job| job.stage == Stage::Finished)
        {
            break;
        }
        let before = store.cache.clone();
        let job = store
            .cache
            .job
            .as_mut()
            .ok_or_else(|| errors::protocol("CodeArts 目录读取进度缺失"))?;
        catalog_fetch::step(job, &account)?;
        if !store.fits() {
            store.cache = before;
            return result(&store, &account, false, true,
                "目录超过当前私有状态容量，已保留原清单与上一读取位置；需后续扩展分片存储，未把截断结果当成完整目录");
        }
        store.save(&account)?;
    }
    if store
        .cache
        .job
        .as_ref()
        .is_some_and(|job| job.stage == Stage::Finished)
    {
        finish(&mut store, &account)
    } else {
        result(&store, &account, false, true,
            "本次已保存一小批目录读取进度，尚未发布新清单；再次刷新会继续，不会把未读部分判为无权限")
    }
}

fn finish(store: &mut Store, account: &Account) -> Result<Value> {
    let job = store
        .cache
        .job
        .clone()
        .ok_or_else(|| errors::protocol("CodeArts 目录读取进度缺失"))?;
    let models = catalog_fetch::assembled(&job)?;
    models::entries(&models, &account.credential.secrets())?;
    let now = util::now()?;
    store.cache.last_attempt = Some(Attempt {
        binding: account.binding.clone(),
        at: now,
        nonempty: !models.is_empty(),
        sources: job.sources.clone(),
    });
    if models.is_empty() && !job.sources.successful() {
        store.cache.job = None;
        store.save(account)?;
        return result(
            store,
            account,
            false,
            false,
            "部分目录来源未完成且没有模型结果；已保留旧目录，未将读取失败当作空目录",
        );
    }
    if !job.sources.successful() && store.good(&account.binding).is_some() {
        // 部分来源失败不能删除上次该来源的模型；本轮诊断独立保存，继续保留完整好清单。
        store.cache.job = None;
        store.save(account)?;
        return result(
            store,
            account,
            false,
            false,
            "部分目录来源读取失败或不完整；已保留本账号上次清单",
        );
    }
    let previous = store.cache.clone();
    store.cache.published = Some(Snapshot {
        binding: account.binding.clone(),
        fetched_at: now,
        models,
        sources: job.sources.clone(),
    });
    store.cache.job = None;
    if !store.fits() {
        store.cache = previous;
        return result(
            store,
            account,
            false,
            true,
            "新目录超出私有状态容量，未覆盖旧目录",
        );
    }
    store.save(account)?;
    result(
        store,
        account,
        true,
        false,
        if job.sources.successful()
            && store
                .good(&account.binding)
                .is_some_and(|snapshot| snapshot.models.is_empty())
        {
            "已成功确认本账号当前目录为空"
        } else if job.sources.successful() {
            "三源读取已结束；这是本账号的展示目录，不是完整调用权限证明"
        } else {
            "本账号目录已更新，但部分来源读取失败或不完整；缺项不代表权限已被撤销"
        },
    )
}

fn result(
    store: &Store,
    account: &Account,
    refreshed: bool,
    incomplete: bool,
    message: &str,
) -> Result<Value> {
    account.unchanged()?;
    let mut value = private(store, account)?;
    value["refreshed"] = json!(refreshed);
    value["count"] = json!(value["models"].as_array().map(Vec::len).unwrap_or(0));
    value["readingPending"] = json!(incomplete);
    let sources_incomplete = store
        .good(&account.binding)
        .is_some_and(|snapshot| !snapshot.sources.successful())
        || store
            .cache
            .last_attempt
            .as_ref()
            .filter(|attempt| attempt.binding == account.binding)
            .is_some_and(|attempt| !attempt.sources.successful());
    value["incomplete"] = json!(incomplete || sources_incomplete);
    value["message"] = json!(message);
    value["httpBudget"] = json!(gates::HTTP_BUDGET);
    if let Some(job) = store
        .cache
        .job
        .as_ref()
        .filter(|job| job.binding == account.binding)
    {
        value["progress"] = job.public();
    }
    if let Some(last) = store
        .cache
        .last_attempt
        .as_ref()
        .filter(|last| last.binding == account.binding)
    {
        value["lastAttempt"] =
            json!({ "at": last.at, "nonempty": last.nonempty, "sources": last.sources });
    }
    Ok(value)
}
