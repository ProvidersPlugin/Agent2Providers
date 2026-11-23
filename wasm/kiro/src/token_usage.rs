//! AWS 事件里的真实 tokenUsage；不用上下文百分比和字符数捏造 token 计费读数。
//! 两种metadata均沿原生增量计数；已给出的0也是有效值，缺字段保留None。
//! uncachedInputTokens优先（包括显式0），inputTokens仅作旧形态兜底；缓存分桶保留。

use agent2api_wasm_sdk::{json, Error, Result, Value};

#[derive(Clone, Default)]
pub struct Usage {
    input: Option<u64>,
    output: Option<u64>,
    total: Option<u64>,
    read: Option<u64>,
    write: Option<u64>,
    metering: Option<Value>,
    credits: Option<f64>,
}
fn count(value: &Value, key: &str) -> Result<Option<u64>> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .filter(|n| *n <= 9_007_199_254_740_991)
            .map(Some)
            .ok_or_else(|| Error::http(502, "Kiro 返回了无效 token 用量")),
    }
}
fn update(slot: &mut Option<u64>, value: Option<u64>) -> Result<()> {
    if let Some(value) = value {
        *slot = Some(
            slot.unwrap_or(0)
                .checked_add(value)
                .filter(|value| *value <= 9_007_199_254_740_991)
                .ok_or_else(|| Error::http(502, "Kiro累计token用量溢出"))?,
        );
    }
    Ok(())
}
impl Usage {
    pub fn read(&mut self, value: &Value) -> Result<()> {
        if !value.is_object() {
            return Err(Error::http(502, "Kiro tokenUsage 不是有效对象"));
        }
        let input = count(value, "uncachedInputTokens")?.or(count(value, "inputTokens")?);
        let mut next = self.clone();
        update(&mut next.input, input)?;
        update(&mut next.output, count(value, "outputTokens")?)?;
        update(&mut next.total, count(value, "totalTokens")?)?;
        update(&mut next.read, count(value, "cacheReadInputTokens")?)?;
        update(&mut next.write, count(value, "cacheWriteInputTokens")?)?;
        let sum = next
            .input
            .and_then(|input| input.checked_add(next.read.unwrap_or(0)))
            .unwrap_or(0);
        if sum > 9_007_199_254_740_991
            || next.input.zip(next.output).is_some_and(|(input, output)| {
                input
                    .saturating_add(next.read.unwrap_or(0))
                    .saturating_add(output)
                    > 9_007_199_254_740_991
            })
        {
            return Err(Error::http(502, "Kiro累计token用量超过精确整数上限"));
        }
        *self = next;
        Ok(())
    }
    pub fn metering(&mut self, value: &Value) -> Result<()> {
        // metering 的 token 是整次计量兜底；不能与 metadata 的同一批 token 再相加。
        let mut token = json!({});
        for key in ["inputTokens", "outputTokens", "totalTokens"] {
            if let Some(count) = count(value, key)? {
                token[key] = json!(count);
            }
        }
        if token.as_object().is_some_and(|fields| !fields.is_empty()) {
            self.metering = Some(token);
        }
        if let Some(unit) = value.get("unit") {
            if !unit.is_string() {
                return Err(Error::http(502, "Kiro 积分计量单位无效"));
            }
            if !unit.as_str().is_some_and(|unit| {
                unit.eq_ignore_ascii_case("credit") || unit.eq_ignore_ascii_case("credits")
            }) {
                return Ok(());
            }
        }
        if let Some(usage) = value.get("usage") {
            let credits = usage
                .as_f64()
                .filter(|credits| credits.is_finite() && *credits >= 0.0)
                .ok_or_else(|| Error::http(502, "Kiro 积分计量读数无效"))?;
            let total = self.credits.unwrap_or(0.0) + credits;
            if !total.is_finite() {
                return Err(Error::http(502, "Kiro 积分计量累计溢出"));
            }
            self.credits = Some(total);
        }
        Ok(())
    }

    pub fn value(&self) -> Option<Value> {
        // 与原生Chat输出一致：input加已知cache-read；cache-write独立保留，不猜它是否已计入input。
        let fallback = self.metering.as_ref();
        let prompt = self
            .input
            .and_then(|input| input.checked_add(self.read.unwrap_or(0)))
            .or_else(|| fallback.and_then(|value| value["inputTokens"].as_u64()));
        let output = self
            .output
            .or_else(|| fallback.and_then(|value| value["outputTokens"].as_u64()));
        let total = self
            .total
            .or_else(|| fallback.and_then(|value| value["totalTokens"].as_u64()));
        if prompt.is_none()
            && output.is_none()
            && total.is_none()
            && self.read.is_none()
            && self.write.is_none()
            && self.credits.is_none()
        {
            return None;
        }
        let mut value = json!({});
        if let Some(prompt) = prompt {
            value["prompt_tokens"] = json!(prompt);
        }
        if let Some(output) = output {
            value["completion_tokens"] = json!(output);
        }
        if let Some(total) = total.or_else(|| {
            prompt
                .zip(output)
                .and_then(|(input, output)| input.checked_add(output))
        }) {
            value["total_tokens"] = json!(total);
        }
        if let Some(read) = self.read {
            value["prompt_tokens_details"] = json!({ "cached_tokens": read });
        }
        if let Some(write) = self.write {
            value["cache_creation_input_tokens"] = json!(write);
        }
        if let Some(credits) = self.credits {
            value["credits_used"] = json!(credits); // 积分计量，不当成美元或 token 数。
        }
        Some(value)
    }
}
