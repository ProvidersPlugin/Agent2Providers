//! 国际原生chat_request_send完整形状，只报告行为，不代表奖励已发放。
//! /activity/growth/streak在国际版仅作为日活回读oracle，不导入国内成长任务中心。
use super::Context;
use crate::{errors, util};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub(super) fn run(ctx: &mut Context) -> Result<Value> {
    let saved = ctx.get("report")?;
    if saved["day"] == ctx.day && saved["done"] == true {
        let mut result = saved["result"].clone();
        result["reused"] = json!(true);
        return Ok(result);
    }
    if saved["day"] == ctx.day && saved["pending"] == true {
        return Ok(
            json!({ "succeeded": false, "uncertain": true, "streakDays": null,
            "msg": "本日上次活跃上报结果未确认，未重复提交相同日活" }),
        );
    }
    let uid = util::text(&ctx.current.secret, &["uid"]);
    if uid.is_empty() {
        return Err(Error::http(
            400,
            "国际日活缺少uid，未发出会被静默丢弃的事件",
        ));
    }
    ctx.check()?;
    let id = format!("agent2api-{}", util::message_id()?);
    let now = sdk::now();
    let event = json!({
        "eventCode": "chat_request_send", "timestamp": now, "reportDelay": 0, "mode": "craft",
        "conversationId": id, "requestId": id, "inputLength": 12,
        "requestModelId": "deepseek-v4.1-flash", "requestModelName": "DeepSeek V4.1 Flash",
        "isPlan": false, "isAutoExecuteTerminal": false, "isAutoModify": false, "codebaseEnable": false,
        "maxToken": 0, "maxSteps": 0, "temperature": 0, "maxRetries": 0, "mentionContexts": [],
        "knowledgeId": [], "knowledgeName": [], "codebaseId": "", "mentionContextCount": 0, "command": "",
        "expertId": "", "recommendId": "", "skillId": "", "skillCount": 0, "totalCount": 0, "fileUri": "",
        "presentAt": now, "traceId": "", "rootRequestId": id, "parentConversationId": id,
        "agentName": "default", "agentType": "conversation", "userId": uid
    });
    // requestModelId是原生事件常量，不作为实际模型调用授权；真正对话仅消费主程序链。
    ctx.put(
        "report",
        &json!({ "day": ctx.day, "pending": true, "eventId": id }),
    )?;
    let reply = match ctx.call("POST", "/v2/report", Some(&json!([event]))) {
        Ok(reply) => reply,
        Err(_) => {
            return Ok(
                json!({ "succeeded": false, "uncertain": true, "streakDays": null,
            "msg": "国际活跃上报结果未确认，已保留待核对记录" }),
            )
        }
    };
    let succeeded = reply.ok() && reply.code() == Some(0);
    let uncertain = !succeeded
        && (!(400..500).contains(&reply.status) && reply.code().is_none()
            || reply.status >= 500
            || reply.status == 408);
    let mut result = json!({ "succeeded": succeeded, "status": reply.status, "uncertain": uncertain, "streakDays": null,
        "msg": if succeeded { "活跃事件上报成功，奖励到账需另行核对".into() } else { errors::clean(reply.message(), &ctx.current.secrets()) } });
    // 先落上报结果再做回读，避免回读失败使已确认上报被当作没执行。
    if ctx
        .put(
            "report",
            &json!({ "day": ctx.day, "done": succeeded, "pending": uncertain, "result": result }),
        )
        .is_err()
    {
        util::warning(
            &mut result,
            "上报已有官方结果，但步骤台账未保存，未重复提交",
        );
        return Ok(result);
    }
    if succeeded {
        match ctx.call("GET", "/activity/growth/streak", None) {
            Ok(reply) if reply.ok() => {
                result["streakDays"] =
                    json!(util::integer(&reply.data()["streak"]["days"]).filter(|days| *days > 0));
            }
            _ => {
                result["warning"] = json!("上报已确认，但连登天数尚未读到有效值");
            }
        }
        if ctx
            .put(
                "report",
                &json!({ "day": ctx.day, "done": true, "pending": false, "result": result }),
            )
            .is_err()
        {
            util::warning(&mut result, "连登读数未能保存，上报结果不受影响");
        }
    }
    Ok(result)
}
