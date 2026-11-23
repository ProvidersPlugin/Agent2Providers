//! 国际管理动作独立分派；模型保活只能经宿主已授权的activity.run，不由tasks或checkin旁路。
use crate::{activity, billing, journal, session};
use agent2api_wasm_sdk::{Error, Result, Value};

pub fn dispatch(params: &Value) -> Result<Value> {
    let action = params["action"].as_str().unwrap_or("");
    match action {
        "activity.run" => activity::run(&params["payload"]),
        "activity.cancel" => journal::cancel(),
        "activity.run-status" => journal::status(),
        "activity.reset" => journal::reset_pending(),
        "billing.checkin-status" => activity::status(),
        "billing.banner" | "billing.ambassador" => {
            let current = session::ensure(false)?;
            if action == "billing.banner" {
                billing::operations::banner(&current)
            } else {
                billing::operations::ambassador(&current)
            }
        }
        "billing.checkin-claim" => Err(Error::http(
            400,
            "国际版没有普通签到入口，请使用每日活跃的领取模式",
        )),
        "billing.reward" => Err(Error::http(400, "国际版没有国内新手礼包或补偿领取")),
        _ if action.starts_with("growth.") => {
            Err(Error::http(400, "国际版没有国内成长任务、抽奖或旅行中心"))
        }
        _ => Err(Error::unsupported()),
    }
}
