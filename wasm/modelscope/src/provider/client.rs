//! 控制面沿用原生大小写信封校验。网络和账号代理一律交宿主，插件没有系统网络。

use agent2api_wasm_sdk::{http, json, Error, Result, Value};

use super::{account, errors, Region};

pub struct Client {
    region: Region,
    token: String,
    source: Option<account::Snapshot>,
}

impl Client {
    pub fn new(region: Region, token: String) -> Result<Self> {
        if token.trim().is_empty() {
            return Err(Error::http(401, "ModelScope 账号缺少令牌，无法查询"));
        }
        Ok(Self {
            region,
            token,
            source: None,
        })
    }

    pub fn for_account(region: Region) -> Result<Self> {
        Self::for_snapshot(region, account::Snapshot::read(region)?)
    }

    pub fn for_snapshot(region: Region, source: account::Snapshot) -> Result<Self> {
        let mut client = Self::new(region, source.token.clone())?;
        client.source = Some(source);
        Ok(client)
    }

    pub fn check(&self) -> Result<()> {
        self.source
            .as_ref()
            .map_or(Ok(()), account::Snapshot::check)
    }

    pub fn site_json(&self, path: &str, operation: &str) -> Result<Value> {
        let base = self.region.site_base();
        self.get(&format!("{base}{path}"),
            json!({
                "Cookie": format!("m_session_id={}", self.token),
                "Referer": format!("{base}/"),
                "Accept": "application/json, text/plain, */*",
                "Accept-Language": "zh-CN,zh;q=0.9,en;q=0.8",
                "Sec-Fetch-Site": "same-origin",
                "Sec-Fetch-Mode": "cors",
                "Sec-Fetch-Dest": "empty"
            }),
            operation, true)
    }

    pub fn models_json(&self) -> Result<Value> {
        self.get(&format!("{}/models", self.region.inference_base()),
            json!({ "Authorization": format!("Bearer {}", self.token), "Accept": "application/json" }),
            "模型目录查询", false)
    }

    fn get(&self, url: &str, headers: Value, operation: &str, site: bool) -> Result<Value> {
        self.check()?;
        let response = http::open("GET", url, headers, None).map_err(|mut error| {
            error.message = format!("{operation}失败：{}", error.message);
            error
        })?;
        self.check()?;
        let status = response.status;
        let bytes = response.bytes(4 * 1024 * 1024)?;
        self.check()?;
        let payload: Value = serde_json::from_slice(&bytes).map_err(|_| {
            Error::http(
                if status >= 400 { status } else { 502 },
                format!("{operation}未返回有效 JSON"),
            )
        })?;
        if !(200..300).contains(&status) {
            let mut error = errors::http_error(self.region, status, &payload);
            error.message = self.redact(&format!("{operation}失败：{}", error.message));
            return Err(error);
        }
        if !payload.is_object() && !(!site && payload.is_array()) {
            return Err(Error::http(502, format!("{operation}返回无法识别的响应")));
        }
        if !succeeded(&payload) {
            let code = code_of(&payload);
            let status = code.filter(|code| (400..600).contains(code)).unwrap_or(502) as u16;
            let mut error = errors::http_error(self.region, status, &payload);
            error.message = self.redact(&format!("{operation}失败：{}", error.message));
            return Err(error);
        }
        if site && !data_of(&payload).is_object() {
            return Err(Error::http(502, format!("{operation}响应缺少业务数据")));
        }
        Ok(payload)
    }

    fn redact(&self, message: &str) -> String {
        message.replace(&self.token, "[凭据已隐藏]")
    }
}

pub fn succeeded(payload: &Value) -> bool {
    if payload
        .get("error")
        .is_some_and(|error| !error.is_null() && error != &Value::Bool(false))
    {
        return false;
    }
    if ["Success", "success"].iter().any(|key| {
        payload
            .get(*key)
            .is_some_and(|flag| !flag.is_null() && flag.as_bool() != Some(true))
    }) {
        return false;
    }
    match payload
        .get("Code")
        .or_else(|| payload.get("code"))
        .filter(|value| !value.is_null())
    {
        Some(_) => matches!(code_of(payload), Some(0 | 200)),
        None => true,
    }
}

pub fn data_of(payload: &Value) -> Value {
    payload
        .get("Data")
        .or_else(|| payload.get("data"))
        .cloned()
        .unwrap_or(Value::Null)
}

pub fn code_of(payload: &Value) -> Option<i64> {
    let code = payload.get("Code").or_else(|| payload.get("code"))?;
    code.as_i64().or_else(|| code.as_str()?.trim().parse().ok())
}
