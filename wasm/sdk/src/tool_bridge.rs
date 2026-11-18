use crate::{host, json, Error, Result, Value};
use std::collections::BTreeSet;

pub struct ToolBridge {
    marker: String,
    names: BTreeSet<String>,
    forced: bool,
    parallel: bool,
    declarations: Value,
}

impl ToolBridge {
    pub fn new(body: &Value) -> Result<Self> {
        let rows = match body.get("tools") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(rows)) if rows.len() <= 256 => rows.clone(),
            _ => return Err(invalid("tools 必须是最多 256 项的数组")),
        };
        let mut names = BTreeSet::new();
        for row in &rows {
            let name = row["function"]["name"].as_str().filter(|name| !name.is_empty()
                && name.len() <= 128 && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte)))
                .ok_or_else(|| invalid("工具名称无效"))?;
            if row["type"] != "function" || !row["function"]["parameters"].is_object()
                || !names.insert(name.to_string()) {
                return Err(invalid("只支持名称唯一且具有参数描述的 function 工具"));
            }
        }
        let forced = match body.get("tool_choice") {
            None | Some(Value::Null) => false,
            Some(Value::String(choice)) if choice == "auto" => false,
            Some(Value::String(choice)) if choice == "none" => { names.clear(); false },
            Some(Value::String(choice)) if choice == "required" && !names.is_empty() => true,
            Some(choice) if choice.is_object() => {
                let name = choice["function"]["name"].as_str().unwrap_or("");
                if choice["type"] != "function" || !names.contains(name) { return Err(invalid("tool_choice 必须选择已声明的工具")); }
                names.retain(|item| item == name);
                true
            }
            _ => return Err(invalid("tool_choice 无效")),
        };
        use base64::Engine;
        let random = host("random.bytes", json!({"length": 24}))?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(random.as_str().unwrap_or(""))
            .map_err(|_| Error::new("工具关联随机值无效"))?;
        if bytes.len() != 24 { return Err(Error::new("工具关联随机值无效")); }
        let marker = format!("agent2api_tools_{}", bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>());
        Ok(Self { marker, names, forced, parallel: body["parallel_tool_calls"] != false, declarations: json!(rows) })
    }

    pub fn active(&self) -> bool { !self.names.is_empty() }

    pub fn instruction(&self) -> String {
        if !self.active() { return String::new(); }
        format!("The following JSON conversation is the complete client history, including system/developer instructions, tool call IDs and actual tool results. Client-side tools are available. Do not execute substitute remote tools. To request tools, return ONLY a JSON object with key {} and an array of objects {{\"name\":\"declared name\",\"arguments\":{{...}}}}. Allowed names: {}. Definitions: {}. Tool results arrive in the next request. Otherwise return text. Tool use is {}. Parallel use is {}.\n", json!(self.marker), json!(self.names), self.declarations, if self.forced { "required" } else { "optional" }, self.parallel)
    }

    pub fn message(&self, text: &str) -> Result<Value> {
        if !self.active() { return Ok(json!({"role":"assistant","content":text})); }
        let parsed = serde_json::from_str::<Value>(text.trim()).ok();
        let calls = parsed.as_ref().and_then(|value| value.get(&self.marker));
        if let Some(calls) = calls {
            let calls = calls.as_array().filter(|calls| !calls.is_empty() && calls.len() <= 256)
                .ok_or_else(|| Error::http(502, "上游工具调用格式无效"))?;
            if !self.parallel && calls.len() > 1 { return Err(Error::http(502, "上游未遵守禁止并行工具的选项")); }
            let mut normalized = Vec::new();
            for (index, call) in calls.iter().enumerate() {
                let name = call["name"].as_str().unwrap_or("");
                if !self.names.contains(name) || !call["arguments"].is_object() || call["arguments"].to_string().len() > 256 * 1024 {
                    return Err(Error::http(502, "上游返回了未声明的工具或无效参数"));
                }
                normalized.push(json!({"id":format!("call_{}_{index}",self.marker),"type":"function",
                    "function":{"name":name,"arguments":call["arguments"].to_string()}}));
            }
            return Ok(json!({"role":"assistant","content":null,"tool_calls":normalized}));
        }
        if text.contains(&self.marker) { return Err(Error::http(502, "上游返回了不完整的工具调用")); }
        if self.forced { return Err(Error::http(502, "上游没有按要求调用工具")); }
        Ok(json!({"role":"assistant","content":text}))
    }
}

fn invalid(message: &str) -> Error { Error::http(400, message).with_code("unsupported_parameter") }
