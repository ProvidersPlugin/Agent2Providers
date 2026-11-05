use std::time::Duration;

use agent2api_provider_protocol::WireProxy;
use serde_json::Value;

use super::{client, credentials::MonkeyCodeCredentials, endpoints, region::Region};
use crate::errors::GatewayError;

pub(super) async fn wait_ready(
    region: Region,
    credentials: &MonkeyCodeCredentials,
    task_id: &str,
    proxy: Option<&WireProxy>,
) -> Result<(), GatewayError> {
    let timeout =
        Duration::from_millis(crate::timeouts::current().headers_ms().min(60 * 60 * 1000));
    let deadline = tokio::time::Instant::now() + timeout;
    let mut url = url::Url::parse(&endpoints::task_create_url(region))
        .map_err(|_| GatewayError::with_status(500, "MonkeyCode 任务地址无效"))?;
    url.path_segments_mut()
        .map_err(|_| GatewayError::with_status(500, "MonkeyCode 任务地址无法追加路径"))?
        .push(task_id);
    let headers = endpoints::authed_headers(region, &credentials.session);
    let mut previous = String::new();
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(GatewayError::with_status(504, format!(
                "MonkeyCode 任务环境在 {} 秒内未准备完成，已停止等待；请在官网确认镜像和环境状态", timeout.as_secs(),
            )));
        }
        let response = crate::http::send_raw(
            "GET",
            url.as_str(),
            None,
            &headers,
            proxy,
            Some((deadline - now).as_millis().min(20_000) as u64),
        )
        .await
        .map_err(|error| {
            GatewayError::with_status(
                if error.is_timeout() { 504 } else { 502 },
                format!(
                    "MonkeyCode 查询任务环境失败：{}",
                    crate::http::describe_chain(&error)
                ),
            )
        })?;
        let payload = response.payload.unwrap_or(Value::Null);
        if client::is_auth_failure(response.status, &payload) {
            return Err(GatewayError::with_status(
                401,
                "MonkeyCode 登录态失效，请重新登录",
            ));
        }
        if !response.ok || client::business_code(&payload) != Some(client::SUCCESS_CODE) {
            let detail = client::upstream_message(&payload);
            return Err(GatewayError::with_status(
                502,
                format!(
                    "MonkeyCode 查询任务环境失败（HTTP {}）：{}",
                    response.status,
                    if detail.is_empty() {
                        "上游未返回有效任务状态"
                    } else {
                        &detail
                    },
                ),
            ));
        }
        let task = payload.get("data").unwrap_or(&payload);
        let status = task.get("status").and_then(Value::as_str).unwrap_or("");
        let condition = task
            .pointer("/virtualmachine/conditions")
            .and_then(Value::as_array)
            .and_then(|items| items.last());
        let condition_type = condition
            .and_then(|item| item.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let detail = condition
            .and_then(|item| item.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(300)
            .collect::<String>();
        if status == "error" || condition_type.eq_ignore_ascii_case("failed") {
            return Err(GatewayError::with_status(
                502,
                format!(
                    "MonkeyCode 任务环境准备失败：{}",
                    if detail.is_empty() {
                        "请在官网查看任务详情"
                    } else {
                        &detail
                    },
                ),
            ));
        }
        match status {
            // 已完成的极短任务仍要 attach 回放结果，不能再次发起一轮。
            "processing" | "finished" => return Ok(()),
            "pending" => {}
            _ => {
                return Err(GatewayError::with_status(
                    502,
                    "MonkeyCode 返回了无法识别的任务状态，已停止等待",
                ))
            }
        }
        let progress = format!("{status}/{condition_type}");
        if progress != previous {
            crate::logging::verbose(
                "[MonkeyCode]",
                &format!("任务 {task_id} 等待环境就绪（{progress}）"),
            );
            previous = progress;
        }
        tokio::time::sleep(
            Duration::from_secs(2)
                .min(deadline.saturating_duration_since(tokio::time::Instant::now())),
        )
        .await;
    }
}
