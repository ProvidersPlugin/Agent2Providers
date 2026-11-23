use agent2api_wasm_sdk::{json, Result, Value};

pub fn run() -> Result<Value> {
    let message = "ZCode 国内版每日赠送套餐签到活动已下架，已停止自动签到";
    Ok(json!({
        "outcome": "inactive",
        "message": message,
        "credit": null,
        "streak": null,
        "retryAt": null,
        "details": {
            "success": false,
            "alreadyCompleted": false,
            "outcome": "inactive",
            "msg": message,
            "dailyCheckin": Value::Null,
            "retryAt": null,
            "results": []
        }
    }))
}
