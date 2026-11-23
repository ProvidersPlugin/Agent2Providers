//! 两地纯文本任务及远端 Agent 活动消费者。
//! 仅调用固定 managed_task 服务，取消和 Trap 清理由原宿主所有者承担。

#[path = "conversation/activity.rs"]
mod activity;
#[path = "conversation/plan.rs"]
mod plan;
#[path = "conversation/result.rs"]
mod result;
#[path = "conversation/stream.rs"]
mod stream;

use agent2api_wasm_sdk::{Result, Value};

pub(super) fn forward(params: &Value) -> Result<()> {
    let prepared = plan::prepare(params)?;
    // 唯一create调用：失败、未知、无收据均直接终止，不换账号或回落网络。
    let mut task = result::create(prepared.body)?;
    let mut output = stream::Stream::new(task.correlation(), &prepared.model, prepared.created, prepared.bridge);
    let outcome = (|| {
        task.observe()?;
        loop {
            match task.read()? {
                result::Item::Text { thought, text } => output.text(thought, &text)?,
                result::Item::Activity(activity) => output.activity(activity)?,
                result::Item::RoundEnded => return Ok(()),
                result::Item::Failed(error) => return Err(error),
            }
        }
    })();
    if let Err(error) = outcome {
        let _ = task.close();
        // 已有文本时保留真实错误帧；没有文本/正常终态时不提前发成功head。
        let _ = output.error(&error);
        return Err(error);
    }
    if let Err(error) = task.close().and_then(|_| output.finish()) {
        let _ = output.error(&error);
        return Err(error);
    }
    Ok(())
}
