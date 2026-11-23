//! 目录/额度的原生三环境策略：sandbox→daily→prod；仅网络、429/5xx或无效集合换环境。
//! 403 只有在携带项目计费头时原样去头再试一次，仍拒绝则停止；不绕资格、订阅或验证。

use super::{
    catalog_shape, client, credentials,
    endpoints::{self, Plane},
    errors, util,
};
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub fn fetch(secret: &Value, quota: bool, check: impl Fn() -> Result<()>) -> Result<Value> {
    let project = util::text(secret, &["projectId"]);
    fetch_for_project(secret, &project, quota, check)
}

pub fn fetch_for_project(
    secret: &Value,
    project: &str,
    quota: bool,
    check: impl Fn() -> Result<()>,
) -> Result<Value> {
    let token = credentials::token(secret)?;
    let body = if project.is_empty() {
        json!({})
    } else {
        json!({ "project": project })
    };
    let plane = if quota { Plane::Quota } else { Plane::Catalog };
    let method = if quota {
        "retrieveUserQuotaSummary"
    } else {
        "fetchAvailableModels"
    };
    let mut last = Error::http(502, "Antigravity 所有环境均未返回可用结果");
    for base in endpoints::CONTROL_BASES {
        check()?;
        let url = endpoints::method(base, method);
        let response = client::post(
            &url,
            endpoints::headers(&token, project, plane, false)?,
            &body,
        );
        check()?;
        let mut reply = match response {
            Ok(reply) => reply,
            Err(error) if errors::local(&error) => return Err(error),
            Err(error) => {
                last = error;
                continue;
            }
        };
        let removed_project_header = reply.status == 403
            && !project.is_empty()
            && errors::upstream(reply.status, &reply.body).code.as_deref()
                != Some("antigravity_account_action");
        if removed_project_header {
            check()?;
            let retried = client::post(&url, endpoints::headers(&token, "", plane, false)?, &body);
            check()?;
            reply = match retried {
                Ok(reply) => reply,
                Err(error) if errors::local(&error) => return Err(error),
                Err(error) => {
                    last = error;
                    continue;
                }
            };
        }
        if !(200..300).contains(&reply.status) || errors::failed(&reply.body) {
            last = errors::upstream(reply.status, &reply.body);
            // 仅令牌失效或上游要求账号核验时立即中止；主机级 400/403/404/429/5xx 继续尝试下一个环境。
            if reply.status == 401 || last.code.as_deref() == Some("antigravity_account_action") {
                return Err(last);
            }
            continue;
        }
        if !reply.body.is_object() {
            last = Error::http(502, "Antigravity 接口没有返回有效对象");
            continue;
        }
        if !quota && !catalog_shape::has_collection(&reply.body) {
            last = Error::http(502, "Antigravity 目录缺少模型集合，保留现有清单");
            continue;
        }
        if quota
            && reply
                .body
                .get("groups")
                .is_some_and(|groups| !groups.is_array())
        {
            last = Error::http(502, "Antigravity 额度响应缺少窗口集合，未将未知读数当作零");
            continue;
        }
        return Ok(reply.body);
    }
    Err(last)
}
