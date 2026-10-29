//! 成长任务**映射表**与对象 id 来源（从参考项目 task_runner.py 的 `MAPPING`
//! 与 `ids_for` 逐条移植）。
//!
//! ── 映射表是什么 ────────────────────────────────────────────
//! 上游任务代码（`task_code`）→ 「怎么点亮」的定义：事件构造器类型（kind）、
//! 兜底次数（target，优先读服务端 progress.target）、对象 id 来源说明。
//! 未登记的任务码在有实测判据前**保守跳过**（发错形状的事件不仅点不亮，
//! 还可能让那一天的上报被风控计入）。
//!
//! ── 为什么内置回落表 ────────────────────────────────────────
//! 对象 id（场景 / 专家 / 技能 / 案例 / 主题）优先从上游市场实时拉取；
//! 拉取失败时回落内置表 —— 这些 id 是**实测可用**的稳定值，让任务在
//! 市场接口抖动时仍有机会点亮。两套来源的取舍见各自 `fetch_*` 的注释。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{Value, json};

use super::{GrowthAccount, bill_call, chat_call};

/// 一个任务的完成定义（映射表条目）。
pub struct TaskSpec {
    /// 事件构造器类型（events.rs 的 `light_up` 按它分派）
    pub kind: &'static str,
    /// 兜底次数（服务端 progress.target 优先）
    pub target: i64,
    /// 对象 id 来源说明（界面提示用）
    pub src: &'static str,
    /// 该 kind 是否需要对象 id（false = 无载体类：对话/自动化/领养等）
    pub needs_object: bool,
    /// 需本人操作（真实业务副作用，跳过）
    pub unforgeable: bool,
    /// 需本人操作的原因文案
    pub reason: &'static str,
    /// 是否需要 `X-Client-Platform: miniprogram`（小程序限定任务）
    pub mp: bool,
}

impl TaskSpec {
    const fn new(kind: &'static str, target: i64, src: &'static str, needs_object: bool) -> Self {
        Self { kind, target, src, needs_object, unforgeable: false, reason: "", mp: false }
    }

    const fn unforgeable(reason: &'static str) -> Self {
        Self {
            kind: "",
            target: 1,
            src: "",
            needs_object: false,
            unforgeable: true,
            reason,
            mp: false,
        }
    }
}

/// 任务映射表（task_code → 完成定义）。**逐条对照参考项目 MAPPING 移植**，
/// 新增任务前先确认有实测判据（事件形状 + 点亮记录）再登记。
pub fn spec_of(code: &str) -> Option<TaskSpec> {
    let spec = match code {
        // ── 常规上报（billing 域 /v2/report）──
        "create_canvas" => TaskSpec::new("canvas", 1, "自造 wb- 画布 id", false),
        "template_5" => TaskSpec::new("template", 5, "场景 id（scenes 接口）", true),
        "expert_5" => TaskSpec::new("expert", 5, "专家市场 ex_ id", true),
        "expert_use_maidehao" | "expert_use_douyinyin" | "expert_use_shangjia_xuanchuan"
        | "expert_use_bowangwang" | "WorkBuddy_Valorant" => {
            TaskSpec::new("expert-task", 1, "任务指定专家的真实对话", true)
        }
        "Expert_team_use_3" => TaskSpec::new("team", 3, "团队专家 id", true),
        "skill_1" => TaskSpec::new("skill", 1, "技能市场 skillId", true),
        "automation_1" => TaskSpec::new("automation", 1, "rrule 虚拟对象", false),
        "playbook_prompt" => TaskSpec::new("playbook", 1, "灵感案例 id", true),
        "Expert_lighthouse" => TaskSpec::new("lighthouse", 1, "轻量云专家 id", true),
        "Hp_Appearance" => TaskSpec::new("skin", 1, "主题 resourceKey", true),
        "chat_5" => TaskSpec::new("chat", 5, "独立会话（无对象）", false),
        "Model_chat_GLM5.2" => TaskSpec::new("glmchat", 1, "glm-5.2 模型上下文", false),
        // 夜猫子（时段敏感：23:00-08:00 CST，窗口内最多补 1 次）
        "black_cat" => TaskSpec::new("cat", 3, "glm-5.2 夜猫模式", false),
        // 领养链路（report 前置解锁 → agreement → buddy/first → claim）
        "first_buddy" => TaskSpec::new("buddyfirst", 1, "领养链路（独立处理）", false),
        // 桌面指纹 6 连对话事件链
        "RichMeow_Chat" => TaskSpec::new("richmeow", 1, "桌面指纹对话链", false),
        // 桌面 fingerprint 五连（Buddy 应用）
        "Buddy_App" | "Buddy_App_QQ" => TaskSpec::new("buddy5", 1, "Buddy 应用五连事件", false),
        // web 域 web_element_click
        "Library_read" => TaskSpec::new("library", 1, "资料库介绍点击（web 域）", false),
        // ── 小程序成长任务（growth 域 mp 口径；accept/claim 需 mp 头）──
        "Sequential_Tasks_1" => TaskSpec::new("minichat", 1, "小程序对话（无 activityId）", false).with_mp(),
        "Sequential_Tasks_2" => {
            TaskSpec::new("miniexpert", 1, "小程序选中专家对话", true).with_mp()
        }
        "Sequential_Tasks_3" => TaskSpec::new("minichat", 5, "小程序对话 ×5", false).with_mp(),
        "Sequential_Tasks_4" => {
            TaskSpec::new("miniautomation", 1, "小程序创建定时任务", false).with_mp()
        }
        "Sequential_Tasks_5" => {
            TaskSpec::new("miniglmchat", 1, "小程序 GLM5.2 对话", false).with_mp()
        }
        "school_season" => TaskSpec::new("schoolseason", 1, "校园日活动（mini chat+activityId）", false).with_mp(),
        // ── 需本人操作 ──
        "Expert_Philanthropy" => TaskSpec::unforgeable("真实捐款动作"),
        "wb_wechat_oa_subscribe_task" => TaskSpec::unforgeable("请本人在微信关注官方公众号"),
        _ => return None,
    };
    Some(spec)
}

impl TaskSpec {
    fn with_mp(mut self) -> Self {
        self.mp = true;
        self
    }
}

/// 上报所需的一个对象 id（+ 元数据）。
#[derive(Clone)]
pub struct ObjectId {
    pub id: String,
    pub meta: Value,
}

/// 取上报对象 id（对齐参考项目 `ids_for`；`offset` 用于多轮次避免复用同一对象）。
///
/// 各来源优先实时拉取、失败回落内置表（见文件头说明）。无载体类 kind 返回
/// 空 id 的占位（`needs_object=false` 时调用方不会用到）。
pub async fn ids_for(
    kind: &str,
    account: &GrowthAccount,
    need: usize,
    offset: usize,
) -> Result<Vec<ObjectId>, String> {
    if need == 0 {
        return Ok(Vec::new());
    }
    let slice = |list: Vec<ObjectId>| -> Vec<ObjectId> {
        let mut seen: Vec<String> = Vec::new();
        let mut out: Vec<ObjectId> = Vec::new();
        for item in list {
            if seen.iter().any(|id| id == &item.id) {
                continue;
            }
            seen.push(item.id.clone());
            out.push(item);
        }
        out.into_iter().skip(offset).take(need).collect()
    };
    match kind {
        "canvas" => {
            let base = crate::logging::now_ms();
            Ok((0..need)
                .map(|index| ObjectId {
                    id: format!("wbx-canvas-{}", base + index as i64),
                    meta: Value::Null,
                })
                .collect())
        }
        "template" => Ok(slice(fetch_scenes(account).await)),
        "expert" => Ok(slice(fetch_market_experts(account, false, None).await)),
        "team" => Ok(slice(fetch_market_experts(account, true, None).await)),
        "lighthouse" => Ok(slice(fetch_lighthouse(account).await)),
        "skill" => Ok(slice(fetch_skills(account).await)),
        "playbook" => Ok(slice(fetch_playbook_cases().await)),
        "skin" => Ok(slice(fetch_pe_theme(account).await)),
        "miniexpert" => Ok(slice(fetch_market_experts(account, false, None).await)),
        // 无载体类型：返回空 id 占位（events.rs 构造时不需要对象）
        "chat" | "glmchat" | "cat" | "automation" | "richmeow" | "buddy5" | "library"
        | "buddyfirst" | "minichat" | "schoolseason" | "miniautomation" | "miniglmchat" => {
            Ok((0..need).map(|_| ObjectId { id: String::new(), meta: Value::Null }).collect())
        }
        other => Err(format!("未知 kind：{other}")),
    }
}

// ─── 内置回落表（实测可用值；实时拉取失败时用）──────────────

const DEFAULT_SCENES: &[(i64, &str)] = &[
    (0, "幻灯片"),
    (2, "视频生成"),
    (4, "深度研究"),
    (6, "文档处理"),
    (8, "数据分析"),
    (10, "可视化"),
    (12, "金融服务"),
    (14, "产品管理"),
];
const DEFAULT_EXPERT_IDS: &[&str] = &[
    "ContentCreator",
    "UiDesigner",
    "DataAnalyticsReporter",
    "ChinaEcommerceOperationsExpert",
    "DouyinStrategist",
    "SalesCoach",
    "BrandGuardian",
    "XiaohongshuOperationsExpert",
];
const DEFAULT_TEAM_IDS: &[&str] = &[
    "CloudOpsTeam",
    "SoftwareCompany",
    "TradingAgentTeam",
    "GPTResearcherTeam",
    "MarketingCampaignTeam",
];
const DEFAULT_SKILL_IDS: &[(&str, &str)] = &[
    ("skill_2096525080079265792", "pptx"),
    ("skill_2096528888507297792", "xlsx"),
    ("skill_2070033533400236032", "qqmusic"),
    ("skill_2095322904487550976", "pdf"),
];
const LIGHTHOUSE_FALLBACK: &[(&str, &str)] = &[("ex_2cvvUZQhDyeJ", "腾讯轻量云专家")];
const PE_THEME: &str = "theme-tkmw7j";
const PE_THEME_NAME: &str = "和平精英激战金秋";
/// COS 静态专家清单（团队专家市场列表很薄，COS 有全量 52 个 team）
const COS_EXPERT_URL: &str = "https://acc-1258344699.cos.accelerate.myqcloud.com/\
workbuddy/expert-marketplace/expert_center.json";
/// 灵感案例注册表
const PLAYBOOK_BASE: &str = "https://static.workbuddy.cn/workbuddy/playbook";

// ─── 实时拉取（只读）────────────────────────────────────────

/// 场景清单（GET /console/as/support/scenes）。
async fn fetch_scenes(account: &GrowthAccount) -> Vec<ObjectId> {
    let call = chat_call(
        account,
        "GET",
        "/console/as/support/scenes?locale=zh-CN",
        None,
        &[],
    )
    .await;
    let mut out: Vec<ObjectId> = Vec::new();
    if let Ok(call) = call {
        if call.ok() {
            if let Some(scenes) = call.data().get("scenes").and_then(Value::as_array) {
                for scene in scenes {
                    if let Some(id) = scene.get("id").and_then(Value::as_i64) {
                        out.push(ObjectId {
                            id: id.to_string(),
                            meta: json!({
                                "name": scene.get("name").and_then(Value::as_str).unwrap_or(""),
                            }),
                        });
                    }
                }
            }
        }
    }
    if out.is_empty() {
        out = DEFAULT_SCENES
            .iter()
            .map(|(id, name)| ObjectId { id: id.to_string(), meta: json!({ "name": name }) })
            .collect();
    }
    out
}

/// 专家市场清单（POST /v2/operation-platform/market/expert/list，最多 3 页）。
/// `team_only` 只留 expert_type=team；`keyword` 关键词过滤。COS 静态清单补足。
pub(super) async fn fetch_market_experts(
    account: &GrowthAccount,
    team_only: bool,
    keyword: Option<&str>,
) -> Vec<ObjectId> {
    let mut out: Vec<ObjectId> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for page in 1..=3 {
        let mut body = json!({ "page": page, "page_size": 50 });
        if let Some(keyword) = keyword {
            body["keyword"] = Value::from(keyword);
        }
        let call = chat_call(
            account,
            "POST",
            "/v2/operation-platform/market/expert/list",
            Some(&body),
            &[],
        )
        .await;
        let Ok(call) = call else { break };
        if !call.ok() {
            break;
        }
        let experts = call
            .data()
            .get("experts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if experts.is_empty() {
            break;
        }
        let count = experts.len();
        for expert in experts {
            let id = expert
                .get("expert_id")
                .or_else(|| expert.get("source_id"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if id.is_empty() || seen.iter().any(|item| item == id) {
                continue;
            }
            let expert_type = expert.get("expert_type").and_then(Value::as_str).unwrap_or("agent");
            if team_only && expert_type != "team" {
                continue;
            }
            seen.push(id.to_string());
            out.push(ObjectId {
                id: id.to_string(),
                meta: json!({
                    "name": expert
                        .get("display_name_zh")
                        .or_else(|| expert.get("profession_zh"))
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                    "displayName": expert.get("display_name_zh").and_then(Value::as_str).unwrap_or(""),
                    "profession": expert.get("profession_zh").and_then(Value::as_str).unwrap_or(""),
                    "category": expert
                        .get("categories")
                        .and_then(Value::as_array)
                        .and_then(|items| items.first())
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                    "expertType": expert_type,
                    "version": expert.get("version").and_then(Value::as_str).unwrap_or(""),
                    "agent_name": expert.get("agent_name").and_then(Value::as_str).unwrap_or(""),
                }),
            });
        }
        if count < 50 {
            break;
        }
    }
    // COS 静态清单补足（团队专家市场接口列表很薄，M4 实测 COS 有 52 个 team）
    if let Some(cos) = fetch_cos_experts().await {
        for expert in cos {
            let id = expert.get("id").and_then(Value::as_str).unwrap_or("");
            let expert_type = expert.get("expertType").and_then(Value::as_str).unwrap_or("agent");
            if id.is_empty()
                || (team_only && expert_type != "team")
                || seen.iter().any(|item| item == id)
            {
                continue;
            }
            seen.push(id.to_string());
            out.push(ObjectId {
                id: id.to_string(),
                meta: json!({
                    "name": expert
                        .get("profession")
                        .and_then(|profession| profession.get("zh"))
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                    "displayName": expert.get("displayName").and_then(|value| value.get("zh")).and_then(Value::as_str).unwrap_or(""),
                    "profession": expert.get("profession").and_then(|value| value.get("zh")).and_then(Value::as_str).unwrap_or(""),
                    "category": expert.get("categoryId").and_then(Value::as_str).unwrap_or(""),
                    "expertType": expert_type,
                    "version": "",
                }),
            });
        }
    }
    if out.is_empty() {
        let fallback: &[&str] = if team_only { DEFAULT_TEAM_IDS } else { DEFAULT_EXPERT_IDS };
        out = fallback
            .iter()
            .map(|id| {
                ObjectId {
                    id: (*id).to_string(),
                    meta: json!({
                        "name": "",
                        "category": "",
                        "expertType": if team_only { "team" } else { "agent" },
                        "version": "",
                    }),
                }
            })
            .collect();
    }
    out
}

/// COS 静态专家清单（公开对象存储，无需鉴权）。
async fn fetch_cos_experts() -> Option<Vec<Value>> {
    let response = crate::http::control_client(None, "direct")
        .get(COS_EXPERT_URL)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let text = response.text().await.ok()?;
    let payload: Value = serde_json::from_str(&text).ok()?;
    payload.get("experts").and_then(Value::as_array).cloned()
}

/// 技能清单（POST /v2/operation-platform/market/skill/list）。
async fn fetch_skills(account: &GrowthAccount) -> Vec<ObjectId> {
    let call = chat_call(
        account,
        "POST",
        "/v2/operation-platform/market/skill/list",
        Some(&json!({ "page": 1, "page_size": 8 })),
        &[],
    )
    .await;
    let mut out: Vec<ObjectId> = Vec::new();
    if let Ok(call) = call {
        if call.ok() {
            if let Some(skills) = call.data().get("skills").and_then(Value::as_array) {
                for skill in skills {
                    if let Some(id) = skill.get("skill_id").and_then(Value::as_str) {
                        out.push(ObjectId {
                            id: id.to_string(),
                            meta: json!({
                                "name": skill.get("name").and_then(Value::as_str).unwrap_or(""),
                            }),
                        });
                    }
                }
            }
        }
    }
    if out.is_empty() {
        out = DEFAULT_SKILL_IDS
            .iter()
            .map(|(id, name)| ObjectId { id: (*id).to_string(), meta: json!({ "name": name }) })
            .collect();
    }
    out
}

/// 灵感案例注册表（静态 CDN，无需鉴权）。
async fn fetch_playbook_cases() -> Vec<ObjectId> {
    let url = format!("{PLAYBOOK_BASE}/registry.json");
    let response = crate::http::control_client(None, "direct")
        .get(&url)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await;
    let mut out: Vec<ObjectId> = Vec::new();
    if let Ok(response) = response {
        if response.status().is_success() {
            if let Ok(text) = response.text().await {
                if let Ok(payload) = serde_json::from_str::<Value>(&text) {
                    if let Some(cases) = payload.get("cases").and_then(Value::as_array) {
                        for item in cases {
                            if let Some(id) = item.get("id").and_then(Value::as_str) {
                                out.push(ObjectId {
                                    id: id.to_string(),
                                    meta: json!({
                                        "title": item.get("title").and_then(Value::as_str).unwrap_or(""),
                                        "artifact_type": item.get("artifact_type").and_then(Value::as_str).unwrap_or("other"),
                                        "categories": item.get("categories").cloned().unwrap_or(json!([])),
                                        "skills": item.get("skills").cloned().unwrap_or(json!([])),
                                        "experts": item.get("experts").cloned().unwrap_or(json!([])),
                                        "prompt": item.get("prompt").and_then(Value::as_str).unwrap_or(""),
                                    }),
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    if out.is_empty() {
        out = vec![ObjectId {
            id: "worker-ledger-freedom-dashboard".to_string(),
            meta: json!({
                "title": "打工人小账本",
                "artifact_type": "other",
                "categories": [""],
                "skills": [],
                "experts": [],
                "prompt": "",
            }),
        }];
    }
    out
}

/// 轻量云专家（市场关键词过滤，回落内置表）。
async fn fetch_lighthouse(account: &GrowthAccount) -> Vec<ObjectId> {
    let mut out: Vec<ObjectId> = Vec::new();
    for keyword in ["lighthouse", "轻量云"] {
        for item in fetch_market_experts(account, false, Some(keyword)).await {
            let blob = format!(
                "{}{}",
                item.id.to_lowercase(),
                item.meta.get("name").and_then(Value::as_str).unwrap_or("").to_lowercase()
            );
            let agent = item
                .meta
                .get("agent_name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_lowercase();
            let blob = format!("{blob}{agent}");
            if ["lighthouse", "轻量", "light", "yun"].iter().any(|needle| blob.contains(needle)) {
                out.push(item);
            }
        }
    }
    if out.is_empty() {
        out = LIGHTHOUSE_FALLBACK
            .iter()
            .map(|(id, name)| ObjectId { id: (*id).to_string(), meta: json!({ "name": name }) })
            .collect();
    }
    out
}

/// 外观主题（POST {bill}/v2/operation-platform/appearance/resources，过滤和平精英）。
async fn fetch_pe_theme(account: &GrowthAccount) -> Vec<ObjectId> {
    let call = bill_call(
        account,
        "POST",
        "/v2/operation-platform/appearance/resources",
        Some(&json!({
            "platform": "client",
            "kind": "theme",
            "version": crate::workbuddy_shared::endpoints::WORKBUDDY_CLIENT_VERSION,
            "lang": "zh-CN",
        })),
        &[],
    )
    .await;
    let mut out: Vec<ObjectId> = Vec::new();
    if let Ok(call) = call {
        if call.ok() {
            if let Some(resources) = call.data().get("resources").and_then(Value::as_array) {
                for item in resources {
                    let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                    if !name.contains("和平精英") && !name.to_lowercase().contains("pubg") {
                        continue;
                    }
                    out.push(ObjectId {
                        id: item.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
                        meta: json!({
                            "name": name,
                            "vipLevel": item.get("vip_level").and_then(Value::as_str).unwrap_or(""),
                            "series": item.get("series").and_then(Value::as_str).unwrap_or(""),
                            "appearance": item.get("appearance").and_then(Value::as_str).unwrap_or(""),
                        }),
                    });
                }
            }
        }
    }
    if out.is_empty() {
        out = vec![ObjectId {
            id: PE_THEME.to_string(),
            meta: json!({
                "name": PE_THEME_NAME,
                "vipLevel": "free",
                "series": "craft",
                "appearance": "light",
            }),
        }];
    }
    out
}
