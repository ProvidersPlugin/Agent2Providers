//! state.v1 保存账号私有模式参数，catalog.accounts.v1 保存和聚合各账号展示目录。
//! 只读刷新：身份核对 -> user/launch -> action_bar。失败不覆盖旧目录，不套用其他账号缓存。

use std::collections::BTreeMap;

use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope},
    AccountLock, Result, Value,
};
use serde::{Deserialize, Serialize};

use crate::{
    account::{self, Snapshot},
    endpoints::ReadEndpoint,
    errors,
    models::{self, Model},
    util,
};

const KEY: &str = "doubaowork.catalog";
const SCHEMA: u32 = 2;
const TTL: i64 = 5 * 60 * 1000;
const MAX_CACHE: usize = 240 * 1024;

#[derive(Deserialize, Serialize)]
struct Catalog {
    binding: Value,
    fetched_at: i64,
    bot: String,
    models: BTreeMap<String, Model>,
}

fn selected(params: &Value) -> bool {
    params["account"].as_str().is_some_and(|id| !id.is_empty())
}

fn cached(snapshot: &state::Snapshot, account: &Snapshot) -> Result<Option<Catalog>> {
    if snapshot.value.is_null() {
        return Ok(None);
    }
    if !snapshot.value.is_object() || !snapshot.value["binding"].is_object() {
        return Err(errors::protocol(
            "豆包工作账号目录缓存缺少归属绑定，未当成空目录",
        ));
    }
    if !snapshot.compatible && snapshot.schema != 1 {
        return Err(errors::protocol(
            "豆包工作账号目录格式版本不兼容，未覆盖旧缓存",
        ));
    }
    if snapshot.value["binding"] != account.binding() {
        return Ok(None);
    }
    if snapshot.value.to_string().len() > MAX_CACHE {
        return Err(errors::protocol("豆包工作账号目录缓存过大"));
    }
    let mut catalog: Catalog = serde_json::from_value(snapshot.value.clone())
        .map_err(|_| errors::protocol("豆包工作账号目录缓存损坏，未当成空目录"))?;
    if catalog.fetched_at <= 0
        || catalog.bot.is_empty()
        || catalog.bot.len() > 256
        || util::protected(&catalog.bot, &account.credentials.protected)
        || catalog.models.len() > models::MAX_MODELS
        || catalog.models.iter().any(|(key, model)| {
            key.is_empty()
                || key.len() > 512
                || model.id.is_empty()
                || util::protected(key, &account.credentials.protected)
                || util::protected(&model.id, &account.credentials.protected)
                || !util::private_metadata(&model.extra, &account.credentials.protected, 0)
        })
    {
        return Err(errors::protocol("豆包工作账号目录缓存不完整"));
    }
    for model in catalog.models.values_mut() {
        model.name = util::safe_text(&model.name, &account.credentials.protected, 300);
        model.mode_name = model
            .mode_name
            .as_ref()
            .map(|name| util::safe_text(name, &account.credentials.protected, 100));
        if model
            .mode_id
            .as_ref()
            .is_some_and(|id| util::protected(id, &account.credentials.protected) || id.len() > 256)
        {
            return Err(errors::protocol("豆包工作缓存模式标识无效"));
        }
    }
    Ok(Some(catalog))
}

fn fresh(catalog: &Catalog, now: i64) -> bool {
    now.checked_sub(catalog.fetched_at)
        .is_some_and(|age| (0..TTL).contains(&age))
}

pub fn list(params: &Value) -> Result<Value> {
    if !selected(params) {
        // 账号目录由宿主聚合；未选号的调用不能再借 catalog.get 枚举其他账号。
        return Ok(models::empty());
    }
    let account = account::current()?;
    let saved = state::get(Scope::Account, KEY, SCHEMA).map_err(errors::host)?;
    let previous = cached(&saved, &account)?;
    let has_binding = previous.is_some();
    let mut list = match previous {
        Some(catalog) => {
            let mut list = models::display(&catalog.models, catalog.fetched_at);
            list["stale"] = json!(!saved.compatible || !fresh(&catalog, util::now()?));
            list
        }
        None => {
            let mut list = models::empty();
            list["stale"] = json!(true);
            list["message"] =
                json!("当前账号没有与身份及凭据匹配的目录快照，请刷新；未采用共享清单作为账号缓存");
            list
        }
    };
    list["bindingVerified"] = json!(has_binding);
    account.unchanged()?;
    Ok(list)
}

fn fetch(account: &Snapshot) -> Result<Catalog> {
    account.verify()?;
    let launch = account.read(ReadEndpoint::UserLaunch, &json!({}))?;
    let bot = util::identifier(&launch["data"]["assistant_bot_id"])
        .or_else(|| util::identifier(&launch["assistant_bot_id"]))
        .filter(|bot| !util::protected(bot, &account.credentials.protected))
        .ok_or_else(|| errors::protocol("豆包工作未返回有效助手编号，未请求猜测目录"))?;
    let value = account.read(
        ReadEndpoint::Catalog,
        &json!({ "language_code": "zh", "bot_id": bot }),
    )?;
    let models = models::parse(&value, &account.credentials.protected)?;
    Ok(Catalog {
        binding: account.binding(),
        fetched_at: util::now()?,
        bot,
        models,
    })
}

fn outcome(mut list: Value, refreshed: bool) -> Value {
    list["refreshed"] = json!(refreshed);
    list["count"] = json!(list["models"].as_array().map(Vec::len).unwrap_or(0));
    list
}

fn for_use(account: &Snapshot) -> Result<Catalog> {
    let saved = state::get(Scope::Account, KEY, SCHEMA)?;
    let previous = cached(&saved, account)?;
    let now = util::now()?;
    if !saved.compatible || previous.as_ref().is_none_or(|catalog| !fresh(catalog, now)) {
        refresh(&json!({"account": account.id, "force": false}))?;
    }
    let saved = state::get(Scope::Account, KEY, SCHEMA)?;
    let catalog = cached(&saved, account)?
        .ok_or_else(|| errors::protocol("当前账号没有经过核对的模型目录"))?;
    account.unchanged()?;
    Ok(catalog)
}

pub fn target(params: &Value) -> Result<(String, Model)> {
    let account = account::current()?;
    let catalog = for_use(&account)?;
    let requested = params["wire_model"]
        .as_str()
        .or_else(|| params["body"]["model"].as_str())
        .ok_or_else(|| errors::invalid("请选择模型"))?;
    let model = catalog
        .models
        .get(requested)
        .cloned()
        .or_else(|| {
            let mut matches = catalog
                .models
                .values()
                .filter(|model| model.name == requested);
            let first = matches.next()?.clone();
            matches.next().is_none().then_some(first)
        })
        .ok_or_else(|| errors::invalid("当前账号没有此模式和模型，请先刷新目录"))?;
    account.unchanged()?;
    Ok((catalog.bot, model))
}

pub fn agent_target() -> Result<(String, Model)> {
    let account = account::current()?;
    let catalog = for_use(&account)?;
    let model = catalog
        .models
        .values()
        .find(|model| model.agent_mode == 1)
        .or_else(|| catalog.models.values().find(|model| model.agent_mode == 2))
        .cloned()
        .ok_or_else(|| errors::invalid("当前账号目录没有可用于生图的工作任务模式"))?;
    account.unchanged()?;
    Ok((catalog.bot, model))
}

pub fn refresh(params: &Value) -> Result<Value> {
    if !selected(params) {
        return Err(agent2api_wasm_sdk::Error::http(
            400,
            "请先选择豆包工作账号刷新目录，未读取本机或其他账号",
        ));
    }
    let _lock = AccountLock::acquire().map_err(errors::host)?;
    let account = account::current()?;
    let saved = state::get(Scope::Account, KEY, SCHEMA).map_err(errors::host)?;
    let previous = cached(&saved, &account)?;
    let now = util::now()?;
    if params["force"] != true && saved.compatible {
        if let Some(previous) = previous.as_ref().filter(|catalog| fresh(catalog, now)) {
            // 私有快照已有，但宿主展示快照可能还未发布（例如上次在写入后取消）。
            let published = sdk::catalog::cached().map_err(errors::host)?;
            let display = models::display(&previous.models, previous.fetched_at);
            let republish =
                published["models"] != display["models"] || published["meta"] != display["meta"];
            account.unchanged()?;
            return Ok(outcome(display, republish));
        }
    }
    let current = match fetch(&account) {
        Ok(catalog) => catalog,
        Err(error) => {
            account.unchanged()?;
            if errors::host_stop(&error) || error.status == Some(401) {
                return Err(error);
            }
            let Some(previous) = previous else {
                return Err(error);
            };
            let mut result = outcome(
                models::display(&previous.models, previous.fetched_at),
                false,
            );
            result["stale"] = json!(true);
            result["message"] = json!(format!("{}；保留当前账号最后成功的目录", error.message));
            result["failure"] = errors::report(&error);
            return Ok(result);
        }
    };
    let value =
        serde_json::to_value(&current).map_err(|_| errors::protocol("豆包工作目录无法序列化"))?;
    let display = models::display(&current.models, current.fetched_at);
    if value.to_string().len() > MAX_CACHE || display.to_string().len() > MAX_CACHE {
        return Err(errors::protocol("豆包工作目录超过持久快照上限，保留旧目录"));
    }
    account.unchanged()?;
    if !state::compare_exchange(Scope::Account, KEY, SCHEMA, saved.revision, value, 0)
        .map_err(errors::host)?
    {
        return Err(errors::changed());
    }
    account.unchanged()?;
    Ok(outcome(display, true))
}
