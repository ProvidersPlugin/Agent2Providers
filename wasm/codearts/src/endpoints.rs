//! 固定来源和操作枚举；没有接受任意 URL/Host/方法/body 的管理入口。

use crate::{encoding, errors};
use agent2api_wasm_sdk::Result;

pub const PROVIDER: &str = "codearts";
pub const BASE: &str = "https://snap-access.cn-north-4.myhuaweicloud.com";
pub const BENEFIT: &str = "https://opengw.developer.huaweicloud.com";
pub const PLUGIN: &str = "snap_vscode";
pub const VERSION: &str = "26.9.101";
pub const LANGUAGE: &str = "en-us";
pub const MAX_OUTPUT: i64 = 65_536;

pub enum ReadRequest<'a> {
    AgentPage(usize),
    AgentDetail(&'a str),
    Builtin,
    BenefitGate,
    BenefitModels,
    Statistics,
    Delivery,
}

impl ReadRequest<'_> {
    /// 原生 balance/welfare 为 20 秒，models 的三源请求为 30 秒。
    /// 这是宿主收到 open 后覆盖准备、连接及完整响应体的总期限，非上游头或签名内容。
    pub fn timeout_ms(&self) -> u64 {
        match self {
            Self::Statistics | Self::Delivery => 20_000,
            Self::AgentPage(_)
            | Self::AgentDetail(_)
            | Self::Builtin
            | Self::BenefitGate
            | Self::BenefitModels => 30_000,
        }
    }
}

pub struct Target {
    pub origin: &'static str,
    pub path: String,
    pub query: String,
    pub label: &'static str,
    pub include_host: bool,
    pub domainless: bool,
    pub headers: Vec<(String, String)>,
}

impl Target {
    pub fn url(&self) -> String {
        format!(
            "{}{}{}{}",
            self.origin,
            self.path,
            if self.query.is_empty() { "" } else { "?" },
            self.query
        )
    }
    pub fn authority(&self) -> Result<&'static str> {
        match self.origin {
            BASE => Ok("snap-access.cn-north-4.myhuaweicloud.com"),
            BENEFIT => Ok("opengw.developer.huaweicloud.com"),
            _ => Err(errors::invalid("CodeArts 请求来源未获本包许可")),
        }
    }
}

fn pair(name: &str, value: &str) -> (String, String) {
    (name.into(), value.into())
}

fn catalog_headers(agent_type: &str) -> Vec<(String, String)> {
    let mut headers = vec![
        pair("Content-Type", "application/json"),
        pair("Accept", "application/json"),
        pair("X-Language", LANGUAGE),
        pair("plugin-name", PLUGIN),
        pair("plugin-version", VERSION),
        pair("client_version", &format!("Vscode_{VERSION}")),
        pair("is_confidential", "false"),
    ];
    if !agent_type.is_empty() {
        headers.push(pair("Agent-Type", agent_type));
    }
    headers
}

pub fn target(request: ReadRequest<'_>) -> Result<Target> {
    let mut value = Target {
        origin: BASE,
        path: String::new(),
        query: String::new(),
        label: "只读查询",
        include_host: false,
        domainless: false,
        headers: Vec::new(),
    };
    match request {
        ReadRequest::AgentPage(page) => {
            if page >= 100 {
                return Err(errors::invalid("CodeArts 目录页数超过原生上限"));
            }
            value.path = "/v1/agent-center/agents/useragents".into();
            value.query = format!("offset={}&limit=100&is_primary_agent=true&supported_client=VSCODE&min_compatible_plugin_version={}", page * 100, encoding::form(VERSION));
            value.headers = catalog_headers("AgentCenter");
            value.label = "Agent 列表";
        }
        ReadRequest::AgentDetail(id) => {
            if id.is_empty() || id.len() > 512 || id.chars().any(char::is_control) {
                return Err(errors::invalid("CodeArts Agent 标识无效"));
            }
            value.path = "/v1/agent-center/agents/detail".into();
            value.query = format!("agent_id={}", encoding::form(id));
            value.headers = catalog_headers("AgentCenter");
            value.label = "Agent 详情";
        }
        ReadRequest::Builtin => {
            value.path = "/v1/model/builtin".into();
            value.headers = catalog_headers("PromptCenter");
            value.label = "内置目录";
        }
        ReadRequest::BenefitGate => {
            value.path = "/v1/benefit-gateway-config".into();
            value.headers = catalog_headers("PromptCenter");
            value.label = "福利目录开关";
        }
        ReadRequest::BenefitModels => {
            value.origin = BENEFIT;
            value.path = "/api/v1/gateway/config".into();
            value.headers = catalog_headers("");
            value.include_host = true;
            value.domainless = true;
            value.label = "福利目录";
        }
        ReadRequest::Statistics => {
            value.path = "/snap-manager/v1/statistics/plugin".into();
            value.label = "订阅统计";
            value.headers = vec![
                pair("Accept", "application/json"),
                pair("X-Language", LANGUAGE),
                pair("plugin-name", PLUGIN),
                pair("plugin-version", VERSION),
            ];
        }
        ReadRequest::Delivery => {
            value.path = "/v1/ops/delivery".into();
            value.query = "channel=IDE".into();
            value.label = "福利活动状态";
            value.headers = vec![
                pair("Content-Type", "application/json"),
                pair("Agent-Type", "PromptCenter"),
                pair("X-Language", LANGUAGE),
            ];
        }
    }
    Ok(value)
}
