//! 出网全部经宿主。登录是code:0信封；积分/每日登录必须flag:true且code:0，不把HTTP200当成功。
//! 极验脚本由主程序的真人交互页加载；插件只取本家配置，不连接任意验证域名。

use super::{account, endpoints, errors};
use agent2api_wasm_sdk::{self as sdk, http, json, Error, Result, Value};

const MAX_RESPONSE: usize = 4 * 1024 * 1024;

pub struct Reply {
    pub status: u16,
    pub body: Value,
}

pub fn read(response: http::Response) -> Result<Reply> {
    let status = response.status;
    let bytes = response.bytes(MAX_RESPONSE).map_err(errors::transport)?;
    let body = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) if !(200..300).contains(&status) => {
            json!({ "message": String::from_utf8_lossy(&bytes) })
        }
        Err(_) => return Err(Error::http(502, "AStudio 响应不是有效 JSON")),
    };
    Ok(Reply { status, body })
}

pub fn studio_headers(secret: Option<&Value>) -> Result<Value> {
    let mut headers = json!({ "accept": "application/json", "studioVersion": endpoints::VERSION,
        "clientVersion": endpoints::VERSION });
    let platform = sdk::host("runtime.platform", json!({}))?;
    match platform.as_str() {
        Some("win") => headers["clientType"] = json!("21"),
        Some("mac" | "linux") => headers["clientType"] = json!("22"),
        Some("other") => {
            return Err(Error::http(400, "AStudio 账号服务没有该平台的客户端类型")
                .with_code("local_precondition"))
        }
        _ => return Err(Error::http(502, "主程序未提供有效的平台类型")),
    }
    if let Some(secret) = secret {
        headers["cookie"] = json!(account::cookie(secret)?);
    }
    Ok(headers)
}

pub fn studio(method: &str, path: &str, secret: Option<&Value>) -> Result<Reply> {
    read(
        http::open(
            method,
            &format!("{}{path}", endpoints::STUDIO),
            studio_headers(secret)?,
            None,
        )
        .map_err(errors::transport)?,
    )
}

pub fn form(path: &str, fields: Value) -> Result<Reply> {
    read(
        http::multipart(
            "POST",
            &format!("{}{path}", endpoints::STUDIO),
            studio_headers(None)?,
            fields,
            Vec::new(),
        )
        .map_err(errors::transport)?,
    )
}

pub fn catalog(key: &str) -> Result<Reply> {
    read(
        http::open(
            "GET",
            &format!("{}{}", endpoints::MODEL_ORIGIN, endpoints::MODELS),
            json!({ "authorization": format!("Bearer {key}"), "accept": "application/json" }),
            None,
        )
        .map_err(errors::transport)?,
    )
}

pub fn responses(key: &str, body: &Value) -> Result<http::Response> {
    http::open("POST", &format!("{}{}", endpoints::MODEL_ORIGIN, endpoints::RESPONSES),
        json!({ "authorization": format!("Bearer {key}"), "accept": "text/event-stream", "content-type": "application/json" }),
        Some(body)).map_err(errors::transport)
}

impl Reply {
    pub fn data(&self, strict: bool, scope: &str) -> Result<Value> {
        if !(200..300).contains(&self.status) || errors::failed(&self.body) {
            return Err(errors::upstream(
                if self.status >= 400 { self.status } else { 502 },
                &self.body,
                scope,
            ));
        }
        if strict {
            if self.body["flag"] != true || errors::code(&self.body) != Some(0) {
                return Err(Error::http(502, "AStudio 接口未明确确认操作成功"));
            }
            return self
                .body
                .get("data")
                .filter(|value| !value.is_null())
                .cloned()
                .ok_or_else(|| Error::http(502, "AStudio 接口返回的数据不完整"));
        }
        // 登录原生同时接受code:0信封和直接JSON对象/数组；非0业务码已在上面拦截。
        // 每个调用方还会逐项校验必要字段，不能因HTTP200就当成登录或目录成功。
        Ok(self.body.get("data").unwrap_or(&self.body).clone())
    }

    pub fn form_ok(&self) -> Result<()> {
        if !(200..300).contains(&self.status) || errors::failed(&self.body) {
            return Err(errors::upstream(
                if self.status >= 400 { self.status } else { 502 },
                &self.body,
                "session",
            ));
        }
        if errors::code(&self.body) != Some(0) {
            return Err(Error::http(502, "AStudio 未明确确认短信已发送，请稍后再试"));
        }
        Ok(())
    }
}

pub fn init(secret: &Value) -> Result<()> {
    let value = studio("POST", endpoints::INIT_APP, Some(secret))?.data(true, "session")?;
    match value["banned"].as_bool() {
        Some(false) => Ok(()),
        Some(true) => Err(Error::http(403, "AStudio 账号已被封禁").with_code("astudio_banned")),
        None => Err(Error::http(502, "AStudio 初始化返回的账号状态不完整")),
    }
}
