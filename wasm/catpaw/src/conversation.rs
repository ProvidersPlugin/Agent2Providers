//! CatPaw Prepared → 固定宿主 Owner → Stream → 同源 settle 的完整插件消费者。
//! 仅已接通 http.catpaw-turn.v1 时执行；普通 HTTP 不得绕过固定服务。
//! 不用普通 HTTP、WASM Drop/会话 JSON 证明收据，不生成 ID、不读账号凭据或代理。

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

use crate::{
    catalog, errors, gates, models,
    stream::{ParsedTurn, Stream},
    turn_host::{Cleanup, PreparedCall, Settlement},
    turn_output::Output,
    turn_request,
};

/// 只由已有 conversation 分派调用；params 的账号/远端ID/stream不能作为执行许可。
/// 原始 client_stream 仅取 request.context；body.stream 不参与轮次或持久化选择。
pub(super) fn forward(params: &Value) -> Result<Value> {
    if !gates::TURN_CONSUMER_ENABLED {
        return Err(gates::chat());
    }
    execute(params)
}

fn execute(params: &Value) -> Result<Value> {
    let body = params
        .get("body")
        .filter(|body| body.is_object())
        .ok_or_else(|| errors::invalid("CatPaw 对话参数缺少规范 body"))?;
    let client_stream = sdk::request::context()
        .map_err(|_| unavailable())?
        .client_stream;
    let displayed = catalog::list(params)?;
    let selection = models::select(&displayed, &body["model"])?;
    let (constraints, count) = turn_request::preflight(body, &selection, client_stream)?;
    let requested_usage = match body.pointer("/stream_options/include_usage") {
        None => false,
        Some(Value::Bool(value)) => *value,
        _ => return Err(errors::invalid("CatPaw include_usage 必须是真实布尔值")),
    };
    // 非流式也由宿主收集标准 SSE；必须带上真实 usage，不能只按流式选项丢掉它。
    let include_usage = !client_stream || requested_usage;
    let model_name = constraints["model_name"]
        .as_str()
        .ok_or_else(|| errors::protocol("CatPaw 本次模型显示名异常"))?;
    let mut call = PreparedCall::open(&constraints, count)?;
    // 有句柄后所有普通错误路径都显式 close；trap/超时须由宿主真实 Owner 独立收尾。
    let mut output = Output::new(&call, model_name, include_usage);
    let result = run(body, &selection, &constraints, &mut call, &mut output);
    match result {
        Ok((parsed, settlement)) => {
            match call.close() {
                Ok(Cleanup::NotNeeded) => {}
                Ok(Cleanup::Confirmed) if !settlement.awaiting_tools => {}
                Ok(Cleanup::Confirmed) => {
                    let error =
                        errors::protocol("CatPaw 工具交接后宿主仍清理远端轮次，未宣布可续接");
                    let _ = output.failure(&error);
                    return Err(error);
                }
                Ok(Cleanup::Unconfirmed) | Err(_) => {
                    let error = errors::protocol("CatPaw 宿主收尾未确认，未发送成功终帧");
                    let _ = output.failure(&error);
                    return Err(error);
                }
            }
            if let Err(error) = output.finish(&parsed) {
                let _ = output.failure(&error);
                return Err(error);
            }
            // 只有真实 ABI 收尾回执成立才返回；不是只交准备材料就返回 null 成功。
            Ok(
                json!({"completed": !settlement.awaiting_tools, "awaiting_tools": settlement.awaiting_tools,
                "state_committed": true, "session_revision": settlement.session_revision}),
            )
        }
        Err(mut error) => {
            match call.close() {
                Ok(Cleanup::NotNeeded | Cleanup::Confirmed) => {}
                Ok(Cleanup::Unconfirmed) | Err(_) => {
                    error
                        .message
                        .push_str("；本次宿主收尾未确认，不能保证远端已停止");
                }
            }
            let _ = output.failure(&error);
            Err(error)
        }
    }
}

fn run(
    body: &Value,
    selection: &models::Selection,
    constraints: &Value,
    call: &mut PreparedCall,
    output: &mut Output,
) -> Result<(ParsedTurn, Settlement)> {
    let prepared = turn_request::prepare(body, selection, call)?;
    let request = turn_request::build(&prepared, call, constraints)?;
    let mut stream = Stream::new(&prepared);
    let mut active = call.start(
        request.round_json,
        request.turn_json,
        request.history_json,
        request.request_context_json,
    )?;
    output.begin()?;
    loop {
        let chunk = match call.read(&mut active) {
            Ok(chunk) => chunk,
            Err(error) => {
                let _ = stream.interrupted();
                return Err(error);
            }
        };
        for material in stream.push(&chunk.bytes)? {
            output.material(material)?;
        }
        // 完整 DONE 仍不能停止 read；宿主 eof 只能是真实正常HTTP EOF。
        if chunk.eof {
            break;
        }
    }
    let parsed = stream.finish()?;
    let settlement = call.settle(&mut active, &parsed)?;
    // settle 只报真实解析材料，不传 completed；宿主必须核 EOF、消息与工具才可提交状态。
    // close 由外层所有普通分支唯一消费，成功终帧也必须等关闭结果。
    Ok((parsed, settlement))
}

fn unavailable() -> Error {
    Error::http(
        503,
        "CatPaw 缺少本次真实对话上下文，未从 body.stream 或普通参数补造",
    )
    .with_code("catpaw_turn_host_unavailable")
}
