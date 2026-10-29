//! ZCode 套餐池（Start Plan）的 Anthropic 请求构造。
//! JWT 鉴权与验证码头属于本通道，不与 Coding Plan 的开放平台额度混用。

use std::collections::BTreeMap;

use agent2api_provider_sdk::ChatPlan;
use serde_json::Value;

use crate::errors::GatewayError;
use crate::logging;

use super::anthropic_outbound;
use super::region::Region;
use super::request_context::RequestContext;
use super::system_prompt;

pub use super::system_prompt::{official_blocks_template, official_prompt_approx_chars, GatewayBlocks};

const MESSAGES_PATH: &str = "/anthropic/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const ANTHROPIC_SDK_UA: &str = "ai-sdk/anthropic/3.0.81";

pub(super) fn code_hint(code: i64) -> Option<&'static str> {
    match code {
        3007 => Some(
            "上游要求人机验证（3007）：验证码令牌缺失或已失效。\
             令牌由打开着的主窗口或浏览器面板页自动铸造；若持续出现，请稍后重试",
        ),
        3012 => Some(
            "套餐池请求被上游风控拒绝（3012），不代表套餐额度已用尽。\
             若官方 ZCode 客户端的同一账号也失败，请联系上游支持；\
             编码套餐与套餐池额度独立，切换通道不会转移这份额度",
        ),
        3001 => Some("上游判定参数错误（3001）：请重新登录该账号以刷新设备标识"),
        _ => None,
    }
}

pub(super) fn quota_hint(raw: &str) -> Option<&'static str> {
    let hit = raw.contains("余额不足")
        || raw.contains("资源包")
        || raw.to_lowercase().contains("insufficient balance");
    hit.then_some(
        "这条限额来自开放平台计费平面：编码套餐通道只消耗开放平台的额度\
         （订阅套餐 / 资源包 / 充值余额），与 ZCode 套餐池互不相通。\
         若这个账号的额度在套餐池里（新注册的体验额度、活动领取的额度），\
         请在账号设置里把「使用套餐」切到「套餐池（Start Plan）」",
    )
}

pub(super) fn build_request(
    region: Region,
    session: &Value,
    body: &Value,
    client_headers: &BTreeMap<String, String>,
    official_prompt: bool,
    override_blocks: Option<&GatewayBlocks>,
) -> Result<ChatPlan, GatewayError> {
    let text_of = |key: &str| {
        session.get(key).and_then(Value::as_str).map(str::trim).unwrap_or("").to_string()
    };
    let jwt = text_of("jwt");
    if jwt.is_empty() {
        return Err(GatewayError::with_status(
            400,
            "该 ZCode 账号的套餐凭据字段缺失（jwt），无法使用套餐池（Start Plan）：\
             请检查导入记录及 pluginSecret 的存储形态；只有确认登录态未保存时才需要重新登录",
        ));
    }
    let model = body.get("model").and_then(Value::as_str).map(str::trim).unwrap_or("");
    if model.is_empty() {
        return Err(GatewayError::with_status(
            400,
            "套餐池（Start Plan）通道需要明确的模型名：请在客户端指定模型后重试",
        ));
    }
    let mut payload = anthropic_outbound::anthropic_request_from_chat(body, model)
        .map_err(|message| GatewayError::with_status(400, format!("请求体转换失败：{message}")))?;
    super::reasoning::apply_to_anthropic(
        &mut payload,
        model,
        body.get(super::reasoning::EFFORT_FIELD).and_then(Value::as_str),
        client_max_tokens(body),
    );
    let request_context = RequestContext::new(client_headers, body);
    system_prompt::apply_start_plan(
        &mut payload,
        model,
        region,
        &text_of("deviceMid"),
        official_prompt,
        override_blocks,
        &request_context,
    )?;

    let mut headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Authorization".to_string(), format!("Bearer {jwt}")),
        ("anthropic-version".to_string(), ANTHROPIC_VERSION.to_string()),
    ];
    if super::captcha::required() {
        let (param, captcha_region) = super::captcha::take()
            .ok_or_else(|| captcha_pool_empty(region.provider_id()))?;
        headers.push((super::captcha::VERIFY_PARAM_HEADER.to_string(), param));
        if !captcha_region.trim().is_empty() {
            headers.push((super::captcha::VERIFY_REGION_HEADER.to_string(), captcha_region));
        }
    }
    headers.extend(super::identity_headers(Some(ANTHROPIC_SDK_UA)));
    headers.extend(request_context.headers());
    if let Some(beta) = client_headers.get("anthropic-beta").map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        headers.push(("anthropic-beta".to_string(), beta.to_string()));
    }
    Ok(ChatPlan {
        url: super::endpoint_mapping::apply(region, &format!("{}{MESSAGES_PATH}", plan_base_url(region))),
        headers,
        body: payload,
        upstream: agent2api_provider_protocol::UpstreamKind::Anthropic,
    })
}

fn captcha_pool_empty(provider_id: &str) -> GatewayError {
    let message = "上游当前要求套餐池（Start Plan）推理请求完成人机验证，但本机令牌池为空：\
         请打开主窗口或浏览器面板页补充令牌；这不是账号失效或套餐额度不足";
    if crate::readiness::hold(provider_id, message) {
        logging::log(
            "[Routing]",
            &format!("⚠️ {provider_id} 的套餐池通道在本机发不出去（令牌池为空）；\
                      界面补充令牌后自动恢复，否则最长 {} 秒重新探一次",
                     crate::readiness::GATE_TTL_MS / 1000),
        );
    }
    GatewayError::with_status(
        i32::from(crate::readiness::LOCAL_PRECONDITION_STATUS),
        agent2api_provider_sdk::local_precondition(message),
    )
}

fn client_max_tokens(body: &Value) -> Option<i64> {
    ["max_tokens", "max_completion_tokens"].iter()
        .find_map(|key| body.get(*key).and_then(Value::as_i64).filter(|value| *value > 0))
}

fn plan_base_url(region: Region) -> String {
    region.env_override("PLAN_BASE_URL")
        .unwrap_or_else(|| format!("{}/api/v1/zcode-plan", region.zcode_origin()))
}
