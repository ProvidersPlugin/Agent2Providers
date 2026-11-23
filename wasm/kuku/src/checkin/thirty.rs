//! 30天活动：配置->homepage->jsToken（接口/活动页）->signin->必要时只读复核。
//! 缺signed_today不默认true；缺连续天数/积分不填0或10，也不把累计天数自行加一。

use super::{report, token};
use crate::{credentials::Credentials, endpoints, errors, network, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};

fn config(value: &Value) -> Result<Option<String>> {
    let rows = value["data"]
        .as_array()
        .ok_or_else(|| Error::http(502, "库库签到活动配置格式不明，不能当作无活动"))?;
    for row in rows {
        let Some(conf) = row.get("conf_value") else {
            continue;
        };
        if !conf.is_object() {
            return Err(Error::http(502, "库库活动配置结构发生变化，未当作无活动"));
        }
        let Some(raw_tid) = conf.get("activityTid") else {
            continue;
        };
        let tid = raw_tid
            .as_str()
            .map(str::trim)
            .map(str::to_string)
            .or_else(|| conf["activityTid"].as_u64().map(|value| value.to_string()));
        if let Some(tid) = tid.filter(|tid| util::identifier(tid, 128)) {
            return Ok(Some(tid));
        }
        return Err(Error::http(502, "库库活动编号无效，未当作无活动"));
    }
    Ok(None)
}
fn homepage(c: &Credentials, tid: &str) -> Result<Value> {
    network::get(
        &format!(
            "{}{}?tid={}",
            endpoints::BASE,
            endpoints::ACTIVITY_HOME,
            util::query(tid)
        ),
        network::activity_headers(c),
        &c.secrets(),
    )
}
fn confirmed(value: &Value, outcome: &str, message: &str) -> Value {
    let mut result = report(outcome, message);
    if let Some(days) =
        util::uint(value.get("continuous")).or_else(|| util::uint(value.get("continuous_days")))
    {
        result["streak"] = json!(days);
    }
    // signPoint仅活动配置计划值，不充当到账读数；credit保留空。
    result
}
pub fn claim(c: &Credentials) -> Result<Option<Value>> {
    let conf = match network::get(
        &format!("{}{}", endpoints::BASE, endpoints::ACTIVITY_CONF),
        network::activity_headers(c),
        &c.secrets(),
    ) {
        Err(error) if error.status == Some(404) => return Ok(None),
        result => result?,
    };
    if let Some(code) = util::integer(&conf["errno"]) {
        if matches!(code, 151004) {
            return Ok(None);
        }
        if code != 0 {
            return Err(errors::upstream(502, Some(code), &conf, &c.secrets()));
        }
    }
    let Some(tid) = config(&conf)? else {
        return Ok(None);
    };
    let home = homepage(c, &tid)?;
    let code = util::integer(&home["errno"]);
    if matches!(code, Some(151004 | -6)) {
        return Ok(None);
    } // 此活动面板原生明确的无活动口径；freepoint仍独立验证登录。
    errors::errno(&home, &c.secrets())?;
    crate::session::unchanged(c)?;
    let data = &home["data"];
    if data["signed_today"] == true {
        return Ok(Some(confirmed(data, "done", "库库今日已签到")));
    }
    if data["signed_today"] != false {
        return Ok(Some(report(
            "failed",
            "库库活动面板未给出今日签到状态，未重复提交",
        )));
    }
    let key = format!("kuku.thirty.{}", util::hash(&tid));
    let before =
        agent2api_wasm_sdk::state::get(agent2api_wasm_sdk::state::Scope::Account, &key, 1)?;
    if !before.compatible {
        return Err(Error::http(409, "库库签到记录格式不兼容，未覆盖旧记录"));
    }
    let today = super::day()?;
    let owner = super::binding(c);
    if before.value["binding"] == owner && before.value["day"] == today {
        return Ok(Some(report(
            "failed",
            "库库本日签到曾提交但结果待核实，未重复提交",
        )));
    }
    let token = token::fetch(c)?;
    crate::session::unchanged(c)?;
    if !agent2api_wasm_sdk::state::compare_exchange(
        agent2api_wasm_sdk::state::Scope::Account,
        &key,
        1,
        before.revision,
        json!({"binding":owner,"day":today,"submitted":true}),
        0,
    )? {
        return Err(Error::http(409, "库库签到记录已变化，未重复提交"));
    }
    let url = format!(
        "{}{}?tid={}&jsToken={}&aid={}",
        endpoints::BASE,
        endpoints::ACTIVITY_SIGN,
        util::query(&tid),
        util::query(&token),
        endpoints::ACTIVITY_AID
    );
    let mut secrets = c.secrets();
    secrets.push(&token);
    let signed = network::form(&url, network::activity_headers(c), "", &secrets)?;
    crate::session::unchanged(c)?;
    let code = util::integer(&signed["errno"]);
    if code == Some(10152) {
        return Ok(Some(confirmed(data, "done", "库库确认今日已签到")));
    }
    if matches!(code, Some(8001 | 8002)) {
        return Ok(Some(report(
            "captcha",
            "库库签到风控未通过，请在官方客户端确认登录和活动状态",
        )));
    }
    errors::errno(&signed, &secrets)?;
    if signed["data"]["signed_today"] == true {
        return Ok(Some(confirmed(
            &signed["data"],
            "claimed",
            "库库确认签到完成；奖励积分未提供实际到账读数",
        )));
    }
    // 响应缺失或false只做只读回查，不再次POST。
    let checked = homepage(c, &tid)?;
    errors::errno(&checked, &c.secrets())?;
    crate::session::unchanged(c)?;
    if checked["data"]["signed_today"] == true {
        Ok(Some(confirmed(
            &checked["data"],
            "claimed",
            "库库活动面板已确认今日签到；奖励积分未提供实际到账读数",
        )))
    } else {
        Ok(Some(report(
            "failed",
            "库库未确认签到完成，本次状态待核实，未重复提交",
        )))
    }
}
