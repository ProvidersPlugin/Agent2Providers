//! 可恢复的小批目录进度；Scope::Account 的持久化内容均带冻结绑定，不作为权限表。
//! 旧好清单始终独立于本轮进度；超时、全空、换凭据不会把部分/空结果写成完整权限。

use agent2api_wasm_sdk::{self as sdk, json, state::Scope, Result, Value};
use serde::{Deserialize, Serialize};

use crate::{
    binding::{Account, Binding},
    errors,
    models::Model,
    util,
};

const KEY: &str = "codearts.catalog.foundation";
const SCHEMA: u32 = 1;
pub const MAX_STATE_BYTES: usize = 240 * 1024;
pub const JOB_TTL_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Pending,
    Running,
    Complete,
    Partial,
    Failed,
    Disabled,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceStatus {
    pub phase: Phase,
    pub requests: usize,
    pub models: usize,
    pub failures: usize,
    pub limited: bool,
}
impl Default for SourceStatus {
    fn default() -> Self {
        Self {
            phase: Phase::Pending,
            requests: 0,
            models: 0,
            failures: 0,
            limited: false,
        }
    }
}
impl SourceStatus {
    fn valid(&self, max_requests: usize) -> bool {
        self.requests <= max_requests
            && self.models <= crate::models::MAX_MODELS
            && self.failures <= self.requests
            && (!self.successful() || self.failures == 0 && !self.limited)
    }
    pub fn successful(&self) -> bool {
        matches!(self.phase, Phase::Complete | Phase::Disabled)
    }
    pub fn finish(&mut self, count: usize) {
        self.models = count;
        self.phase = if self.failures == 0 && !self.limited {
            Phase::Complete
        } else if count == 0 {
            Phase::Failed
        } else {
            Phase::Partial
        };
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Sources {
    pub agent: SourceStatus,
    pub builtin: SourceStatus,
    pub benefit: SourceStatus,
}
impl Sources {
    fn valid(&self) -> bool {
        self.agent.valid(10_100) && self.builtin.valid(1) && self.benefit.valid(2)
    }
    pub fn successful(&self) -> bool {
        self.agent.successful() && self.builtin.successful() && self.benefit.successful()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    AgentPages,
    AgentDetails,
    Builtin,
    BenefitGate,
    BenefitModels,
    Finished,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub binding: Binding,
    pub started_at: i64,
    pub updated_at: i64,
    pub stage: Stage,
    pub page: usize,
    pub agent_ids: Vec<String>,
    pub detail_index: usize,
    pub agent: Vec<Model>,
    pub builtin: Vec<Model>,
    pub benefit: Vec<Model>,
    pub sources: Sources,
}
impl Job {
    pub fn new(binding: Binding, now: i64) -> Self {
        Self {
            binding,
            started_at: now,
            updated_at: now,
            stage: Stage::AgentPages,
            page: 0,
            agent_ids: Vec::new(),
            detail_index: 0,
            agent: Vec::new(),
            builtin: Vec::new(),
            benefit: Vec::new(),
            sources: Sources::default(),
        }
    }
    pub fn current(&self, binding: &Binding, now: i64) -> bool {
        self.binding == *binding
            && now >= self.started_at
            && now.saturating_sub(self.started_at) < JOB_TTL_MS
    }
    pub fn public(&self) -> Value {
        json!({ "stage": self.stage, "startedAt": self.started_at, "updatedAt": self.updated_at,
            "pagesRead": self.page, "agentsFound": self.agent_ids.len(), "detailsRead": self.detail_index,
            "sources": self.sources, "complete": self.stage == Stage::Finished,
            "permissionComplete": false })
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub binding: Binding,
    pub fetched_at: i64,
    pub models: Vec<Model>,
    pub sources: Sources,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Attempt {
    pub binding: Binding,
    pub at: i64,
    pub nonempty: bool,
    pub sources: Sources,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Cache {
    pub published: Option<Snapshot>,
    pub job: Option<Job>,
    pub last_attempt: Option<Attempt>,
}

pub struct Store {
    revision: u64,
    pub cache: Cache,
}
impl Store {
    pub fn load() -> Result<Self> {
        let snapshot = sdk::state::get(Scope::Account, KEY, SCHEMA).map_err(errors::host)?;
        if !snapshot.compatible || snapshot.schema != SCHEMA {
            return Err(errors::protocol(
                "CodeArts 私有目录状态版本不兼容，未当空记录覆盖",
            ));
        }
        let cache = if snapshot.value.is_null() {
            if snapshot.revision != 0 {
                return Err(errors::protocol(
                    "CodeArts 目录状态已存在但内容为空，未重置读取进度",
                ));
            }
            Cache::default()
        } else {
            serde_json::from_value(snapshot.value)
                .map_err(|_| errors::protocol("CodeArts 私有目录状态无法读取，未清除已有记录"))?
        };
        Ok(Self {
            revision: snapshot.revision,
            cache,
        })
    }

    pub fn value(&self) -> Result<Value> {
        serde_json::to_value(&self.cache).map_err(|_| errors::protocol("CodeArts 目录状态无法编码"))
    }

    pub fn fits(&self) -> bool {
        self.value()
            .is_ok_and(|value| value.to_string().len() <= MAX_STATE_BYTES)
    }

    pub fn save(&mut self, account: &Account) -> Result<()> {
        account.unchanged()?;
        let value = self.value()?;
        if value.to_string().len() > MAX_STATE_BYTES {
            return Err(errors::limited(
                "CodeArts 目录已超过私有状态容量，未截断为完整清单；原有目录仍保留",
            ));
        }
        let updated =
            sdk::state::compare_exchange(Scope::Account, KEY, SCHEMA, self.revision, value, 0)
                .map_err(errors::host)?;
        if !updated {
            return Err(util::invalidated());
        }
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| errors::protocol("CodeArts 目录状态版本已耗尽"))?;
        // 宿主 state CAS 不含账号代次原子条件；晚写的数据仍带旧 binding，绝不供新凭据使用。
        account.unchanged()
    }

    pub fn good(&self, binding: &Binding) -> Option<&Snapshot> {
        self.cache
            .published
            .as_ref()
            .filter(|snapshot| snapshot.binding == *binding)
    }

    pub fn validate_current(&self, account: &Account) -> Result<()> {
        let secrets = account.credential.secrets();
        if let Some(good) = self.good(&account.binding) {
            if good.fetched_at <= 0
                || good.models.len() > crate::models::MAX_MODELS
                || !good.sources.valid()
                || good.models.is_empty() && !good.sources.successful()
            {
                return Err(errors::protocol("CodeArts 已保存目录的元数据无效"));
            }
            crate::models::entries(&good.models, &secrets)?;
        }
        if let Some(last) = self
            .cache
            .last_attempt
            .as_ref()
            .filter(|last| last.binding == account.binding)
        {
            if last.at <= 0 || !last.sources.valid() {
                return Err(errors::protocol("CodeArts 目录上次读取记录无效"));
            }
        }
        if let Some(job) = self
            .cache
            .job
            .as_ref()
            .filter(|job| job.binding == account.binding)
        {
            if job.started_at <= 0
                || job.updated_at < job.started_at
                || job.page > 100
                || job.agent_ids.len() > 10_000
                || job.detail_index > job.agent_ids.len()
                || !job.sources.valid()
                || job.stage == Stage::AgentPages && job.page >= 100
                || job.agent_ids.iter().any(|id| {
                    id.is_empty()
                        || id.len() > 512
                        || id.chars().any(char::is_control)
                        || errors::contains_secret(id, &secrets)
                })
            {
                return Err(errors::protocol(
                    "CodeArts 目录读取进度无效，未重新发起或清除记录",
                ));
            }
            for (items, source) in [
                (&job.agent, crate::models::Source::Agent),
                (&job.builtin, crate::models::Source::Builtin),
                (&job.benefit, crate::models::Source::Benefit),
            ] {
                if items.iter().any(|model| model.source != source) {
                    return Err(errors::protocol("CodeArts 目录来源与读取进度不一致"));
                }
                crate::models::entries(items, &secrets)?;
            }
        }
        Ok(())
    }
}
