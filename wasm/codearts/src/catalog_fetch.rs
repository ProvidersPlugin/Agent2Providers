//! 目录读取每步保存账号私有进度；中断后显式刷新继续。
//! 每次目录请求独立使用宿主 30 秒总期限并受外层剩余预算约束。
//! 请求超时或在既有账号复核边界发现停用就停止，不重发、不换账号，不将未完成结果当成成功。

use agent2api_wasm_sdk::Result;

use crate::{
    binding::Account,
    catalog_state::{Job, Phase, Stage},
    endpoints::ReadRequest,
    errors, models, network, util,
};

const MAX_RESPONSE: usize = 2 * 1024 * 1024;

pub fn step(job: &mut Job, account: &Account) -> Result<()> {
    let secrets = account.credential.secrets();
    match job.stage {
        Stage::AgentPages => {
            job.sources.agent.phase = Phase::Running;
            job.sources.agent.requests += 1;
            let result = network::get(account, ReadRequest::AgentPage(job.page), MAX_RESPONSE)
                .and_then(|value| models::agent_ids(&value, &secrets));
            match result {
                Ok(page) => {
                    let before = job.agent_ids.len();
                    for id in page.ids {
                        if !job.agent_ids.contains(&id) {
                            job.agent_ids.push(id);
                        }
                    }
                    job.page += 1;
                    if page.rows < 100 {
                        job.stage = Stage::AgentDetails;
                    } else if job.page >= 100 || job.agent_ids.len() == before {
                        job.sources.agent.limited = true;
                        job.stage = Stage::AgentDetails;
                    }
                }
                Err(error) if errors::host_stop(&error) || error.status == Some(401) => {
                    return Err(error)
                }
                Err(_) => {
                    job.sources.agent.failures += 1;
                    job.stage = Stage::AgentDetails;
                }
            }
            advance_agent(job);
        }
        Stage::AgentDetails => {
            let id = job
                .agent_ids
                .get(job.detail_index)
                .ok_or_else(|| errors::protocol("CodeArts 目录详情游标无效"))?
                .clone();
            job.sources.agent.requests += 1;
            let result = network::get(account, ReadRequest::AgentDetail(&id), MAX_RESPONSE)
                .and_then(|value| models::agent_detail(&value, &secrets))
                .and_then(|incoming| models::merge(&mut job.agent, incoming));
            match result {
                Ok(()) => {}
                Err(error) if errors::host_stop(&error) || error.status == Some(401) => {
                    return Err(error)
                }
                Err(_) => job.sources.agent.failures += 1,
            }
            job.detail_index += 1;
            advance_agent(job);
        }
        Stage::Builtin => {
            job.sources.builtin.phase = Phase::Running;
            job.sources.builtin.requests += 1;
            let result = network::get(account, ReadRequest::Builtin, MAX_RESPONSE)
                .and_then(|value| models::builtin(&value, &secrets));
            match result {
                Ok(incoming) => {
                    models::merge(&mut job.builtin, incoming)?;
                }
                Err(error) if errors::host_stop(&error) || error.status == Some(401) => {
                    return Err(error)
                }
                Err(_) => job.sources.builtin.failures += 1,
            }
            job.sources.builtin.finish(job.builtin.len());
            job.stage = Stage::BenefitGate;
        }
        Stage::BenefitGate => {
            job.sources.benefit.phase = Phase::Running;
            job.sources.benefit.requests += 1;
            let result = network::get(account, ReadRequest::BenefitGate, MAX_RESPONSE)
                .and_then(|value| models::benefit_gate(&value));
            match result {
                Ok(true) => job.stage = Stage::BenefitModels,
                Ok(false) => {
                    job.sources.benefit.phase = Phase::Disabled;
                    job.stage = Stage::Finished;
                }
                Err(error) if errors::host_stop(&error) || error.status == Some(401) => {
                    return Err(error)
                }
                Err(_) => {
                    job.sources.benefit.failures += 1;
                    job.sources.benefit.finish(0);
                    job.stage = Stage::Finished;
                }
            }
        }
        Stage::BenefitModels => {
            job.sources.benefit.requests += 1;
            let result = network::get(account, ReadRequest::BenefitModels, MAX_RESPONSE)
                .and_then(|value| models::benefit(&value, &secrets));
            match result {
                Ok(incoming) => {
                    models::merge(&mut job.benefit, incoming)?;
                }
                Err(error) if errors::host_stop(&error) || error.status == Some(401) => {
                    return Err(error)
                }
                Err(_) => job.sources.benefit.failures += 1,
            }
            job.sources.benefit.finish(job.benefit.len());
            job.stage = Stage::Finished;
        }
        Stage::Finished => {}
    }
    job.sources.agent.models = job.agent.len();
    job.sources.builtin.models = job.builtin.len();
    job.sources.benefit.models = job.benefit.len();
    job.updated_at = util::now()?;
    Ok(())
}

fn advance_agent(job: &mut Job) {
    if job.stage == Stage::AgentDetails && job.detail_index >= job.agent_ids.len() {
        job.sources.agent.finish(job.agent.len());
        job.stage = Stage::Builtin;
    }
}

pub fn assembled(job: &Job) -> Result<Vec<models::Model>> {
    let mut merged = Vec::new();
    models::merge(&mut merged, job.agent.clone())?;
    models::merge(&mut merged, job.builtin.clone())?;
    models::merge(&mut merged, job.benefit.clone())?;
    Ok(merged)
}
