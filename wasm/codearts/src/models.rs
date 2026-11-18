//! 三源目录的纯数据解析，改写自本仓 models.rs（AGPL-3.0-only，2026-11-10）。
//! 缺字段不猜权限；展示隐藏不等于禁用。

use agent2api_wasm_sdk::{json, Result, Value};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::{endpoints, errors};

pub const MAX_MODELS: usize = 1024;

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Agent,
    Builtin,
    Benefit,
}
impl Source {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Builtin => "builtin",
            Self::Benefit => "benefit",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub description: String,
    pub context: Option<i64>,
    pub output: Option<i64>,
    pub images: Option<bool>,
    pub source: Source,
    pub visible: Option<bool>,
    pub credits: String,
}

impl Model {
    pub fn entry(&self) -> Value {
        let mut value = json!({ "id": self.id, "name": self.name, "providerModel": self.id,
            "source": self.source.name(), "benefit": self.source == Source::Benefit,
            "visibility": if self.visible == Some(false) { "hidden" } else { "shown" },
            "codeartsCatalogFormat": 1, "catalogOnly": true,
            "capabilityEvidence": "upstream_catalog", "permissionEvidence": "not_exhaustive",
            "runtimeAvailable": true, "migrationStage": "complete", "supportsToolCall": true, "supportsReasoning": true });
        if let Some(context) = self.context {
            value["contextWindow"] = json!(context);
            value["maxInputTokens"] = json!(context);
        }
        if let Some(output) = self.output {
            value["maxOutputTokens"] = json!(output.min(endpoints::MAX_OUTPUT));
        }
        if let Some(images) = self.images {
            value["supportsImages"] = json!(images);
        }
        if let Some(visible) = self.visible {
            value["display_enabled"] = json!(visible);
        }
        if !self.description.is_empty() {
            value["description"] = json!(self.description);
        }
        if !self.credits.is_empty() {
            value["credits"] = json!(self.credits);
        }
        value
    }

    pub fn validate(&self, secrets: &[String]) -> Result<()> {
        if self.id.is_empty()
            || self.id.len() > 512
            || self.name.len() > 1024
            || self.description.len() > 2048
            || self.credits.len() > 128
            || [&self.id, &self.name, &self.description, &self.credits]
                .iter()
                .any(|text| {
                    text.chars().any(char::is_control)
                        || text.contains("://")
                        || errors::contains_secret(text, secrets)
                })
            || self.context.is_some_and(|n| n <= 0)
            || self
                .output
                .is_some_and(|n| n <= 0 || n > endpoints::MAX_OUTPUT)
        {
            return Err(errors::protocol(
                "CodeArts 目录状态包含无效模型资料，未公开或作为权限使用",
            ));
        }
        Ok(())
    }
}

pub fn merge(target: &mut Vec<Model>, incoming: Vec<Model>) -> Result<()> {
    // 先检查本轮新增数，失败不留下半份修改。
    let mut known: HashSet<String> = target.iter().map(|model| model.id.clone()).collect();
    let incoming: Vec<_> = incoming
        .into_iter()
        .filter(|model| known.insert(model.id.clone()))
        .collect();
    if target.len().saturating_add(incoming.len()) > MAX_MODELS {
        return Err(errors::limited(
            "CodeArts 目录超过当前私有状态条目预算，未截断为完整目录",
        ));
    }
    target.extend(incoming);
    Ok(())
}

fn unique(models: Vec<Model>, secrets: &[String]) -> Result<Vec<Model>> {
    for model in &models {
        model.validate(secrets)?;
    }
    let mut out = Vec::new();
    merge(&mut out, models)?;
    Ok(out)
}

#[allow(dead_code)]
pub fn resolve<'a>(models: &'a [Model], requested: &str) -> Option<&'a Model> {
    models
        .iter()
        .find(|model| model.id.eq_ignore_ascii_case(requested.trim()))
}

fn flag(value: &Value, key: &str) -> Result<Option<bool>> {
    match value.get(key).filter(|value| !value.is_null()) {
        None => Ok(None),
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or_else(|| errors::protocol("CodeArts 目录开关类型无效")),
    }
}
fn number(value: &Value, key: &str) -> Result<Option<i64>> {
    match value.get(key).filter(|value| !value.is_null()) {
        None => Ok(None),
        Some(value) => value
            .as_i64()
            .map(|number| (number > 0).then_some(number))
            .ok_or_else(|| errors::protocol("CodeArts 目录额度字段类型无效")),
    }
}
fn text(value: &Value, key: &str, secrets: &[String], maximum: usize) -> Result<String> {
    errors::upstream_text(value, key, secrets, maximum)
}
fn pick(value: &Value, keys: &[&str], secrets: &[String], maximum: usize) -> Result<String> {
    for key in keys {
        let value = text(value, key, secrets, maximum)?;
        if !value.is_empty() {
            return Ok(value);
        }
    }
    Ok(String::new())
}

pub struct AgentPage {
    pub ids: Vec<String>,
    pub rows: usize,
}
pub fn agent_ids(value: &Value, secrets: &[String]) -> Result<AgentPage> {
    let rows = value["agents"]
        .as_array()
        .ok_or_else(|| errors::protocol("CodeArts Agent 列表缺少 agents 数组"))?;
    if rows.len() > 100 {
        return Err(errors::protocol("CodeArts Agent 页返回条数超过请求范围"));
    }
    let mut ids = Vec::new();
    for row in rows {
        if !row.is_object() {
            return Err(errors::protocol("CodeArts Agent 列表条目无效"));
        }
        let id = pick(row, &["agent_id", "original_id"], secrets, 256)?;
        if id.is_empty() {
            return Err(errors::protocol(
                "CodeArts Agent 条目缺少可关联的标识，分页完整性未知",
            ));
        }
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(AgentPage {
        ids,
        rows: rows.len(),
    })
}

pub fn agent_detail(value: &Value, secrets: &[String]) -> Result<Vec<Model>> {
    let gpts = value["gpts"]
        .as_object()
        .ok_or_else(|| errors::protocol("CodeArts Agent 详情缺少 gpts，未当成无权限"))?;
    let rows = gpts
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| errors::protocol("CodeArts Agent 详情未提供 models 数组，不能确认空目录"))?;
    if rows.len() > MAX_MODELS {
        return Err(errors::limited("CodeArts Agent 模型条目过多"));
    }
    let mut models = Vec::new();
    for row in rows {
        if !row.is_object() {
            return Err(errors::protocol("CodeArts Agent 模型条目无效"));
        }
        let empty = json!({});
        let parameters = row
            .get("model_parameters")
            .filter(|value| !value.is_null())
            .unwrap_or(&empty);
        if !parameters.is_object() {
            return Err(errors::protocol("CodeArts Agent 模型参数不是对象"));
        }
        if flag(parameters, "enabled")? == Some(false) {
            continue;
        }
        let id = text(parameters, "model_id", secrets, 256)?;
        let id = if id.is_empty() {
            pick(row, &["model_alias", "model_id"], secrets, 256)?
        } else {
            id
        };
        if id.is_empty() {
            return Err(errors::protocol("CodeArts Agent 模型缺少真实名称"));
        }
        let name = text(row, "model_name", secrets, 256)?;
        models.push(Model {
            name: if name.is_empty() { id.clone() } else { name },
            id,
            description: pick(parameters, &["model_desc_en", "model_desc"], secrets, 512)?,
            context: number(parameters, "context_window")?,
            output: number(parameters, "max_tokens")?.map(|value| value.min(endpoints::MAX_OUTPUT)),
            images: flag(parameters, "supports_images")?,
            source: Source::Agent,
            visible: flag(parameters, "display_enabled")?,
            credits: credit_display(row, secrets)?,
        });
    }
    unique(models, secrets)
}

pub fn builtin(value: &Value, secrets: &[String]) -> Result<Vec<Model>> {
    let rows = value["builtinModels"]
        .as_array()
        .ok_or_else(|| errors::protocol("CodeArts 内置目录缺少 builtinModels 数组"))?;
    if rows.len() > MAX_MODELS {
        return Err(errors::limited("CodeArts 内置模型条目过多"));
    }
    let mut models = Vec::new();
    for row in rows {
        if !row.is_object() {
            return Err(errors::protocol("CodeArts 内置模型条目无效"));
        }
        if flag(row, "enable")? == Some(false) {
            continue;
        }
        let id = pick(row, &["model_id", "model_name"], secrets, 256)?;
        if id.is_empty() {
            return Err(errors::protocol("CodeArts 内置模型缺少真实名称"));
        }
        let name = text(row, "model_name", secrets, 256)?;
        models.push(Model {
            name: if name.is_empty() { id.clone() } else { name },
            id,
            description: text(row, "model_desc", secrets, 512)?,
            context: number(row, "context_window")?,
            output: number(row, "max_tokens")?.map(|value| value.min(endpoints::MAX_OUTPUT)),
            images: flag(row, "supports_images")?,
            source: Source::Builtin,
            visible: flag(row, "display_enabled")?,
            credits: credit_display(row, secrets)?,
        });
    }
    unique(models, secrets)
}

pub fn benefit_gate(value: &Value) -> Result<bool> {
    value["enabled"]
        .as_bool()
        .ok_or_else(|| errors::protocol("CodeArts 福利目录开关缺少 enabled 布尔值"))
}

pub fn benefit(value: &Value, secrets: &[String]) -> Result<Vec<Model>> {
    if value["error_code"].as_str() != Some("0000") {
        return Err(errors::protocol(
            "CodeArts 福利目录未得到 error_code=0000 确认",
        ));
    }
    let rows = value["result"]["models"]
        .as_array()
        .ok_or_else(|| errors::protocol("CodeArts 福利目录缺少 result.models 数组"))?;
    if rows.len() > MAX_MODELS {
        return Err(errors::limited("CodeArts 福利模型条目过多"));
    }
    let mut ordered: Vec<_> = rows.iter().collect();
    ordered.sort_by_key(|row| row["sort"].as_i64().unwrap_or(0));
    let mut models = Vec::new();
    for row in ordered {
        if !row.is_object() {
            return Err(errors::protocol("CodeArts 福利模型条目无效"));
        }
        let id = text(row, "model_id", secrets, 256)?;
        if id.is_empty() {
            return Err(errors::protocol("CodeArts 福利模型缺少真实名称"));
        }
        let name = text(row, "model_name", secrets, 256)?;
        models.push(Model {
            name: if name.is_empty() { id.clone() } else { name },
            id,
            description: String::new(),
            context: number(row, "context_window")?,
            output: number(row, "max_tokens")?.map(|value| value.min(endpoints::MAX_OUTPUT)),
            // 原生这里写 false，但上游没有给出能力结论；保留 unknown，不能用于本地拦图。
            images: None,
            source: Source::Benefit,
            visible: None,
            credits: String::new(),
        });
    }
    unique(models, secrets)
}

fn credit_display(value: &Value, secrets: &[String]) -> Result<String> {
    let Some(rows) = value.get("credit").filter(|value| !value.is_null()) else {
        return Ok(String::new());
    };
    let rows = rows
        .as_array()
        .filter(|rows| rows.len() <= 64)
        .ok_or_else(|| errors::protocol("CodeArts 积分倍率档位格式无效"))?;
    let mut fallback = String::new();
    for row in rows {
        if !row.is_object() {
            return Err(errors::protocol("CodeArts 积分倍率条目无效"));
        }
        if row
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| status.trim() != "0")
        {
            continue;
        }
        let display = text(row, "ratio_display", secrets, 64)?;
        if display.is_empty() {
            continue;
        }
        if row["input_from"].as_i64() == Some(0) {
            return Ok(display);
        }
        if fallback.is_empty() {
            fallback = display;
        }
    }
    Ok(fallback)
}

pub fn entries(models: &[Model], secrets: &[String]) -> Result<Vec<Value>> {
    let mut seen = HashSet::new();
    for model in models {
        model.validate(secrets)?;
        if !seen.insert(&model.id) {
            return Err(errors::protocol("CodeArts 保存的目录含重复模型标识"));
        }
    }
    Ok(models.iter().map(Model::entry).collect())
}
