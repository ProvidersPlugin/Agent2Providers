//! 编码套餐凭证换取：把 OAuth 登录拿到的 `accessToken` 换成**能用于推理的
//! API Key**（`apiKey` 或 `apiKey.secret` 两段形式）。
//!
//! ── 为什么必须有这一步（曾经漏掉的根因）─────────────────────
//! OAuth 轮询给的 `access_token` **不是**推理凭证：直接拿它去
//! `{openai_base}/chat/completions` 会被上游按「OAuth 令牌」那条路校验并回
//! 401（`token expired or incorrect` / `Authentication Failed`）。官方客户端与
//! 参考实现都先做一次换取，拿到的才是编码套餐的 API Key：
//!
//! ```text
//!   z/login（仅国际版）→ 查默认机构/项目 → 找或建名为 zcode-api-key 的密钥
//!     → 取 secretKey → 拼成 {apiKey}.{secret}
//! ```
//!
//! 参考实现的原文（`src/auth/resolver.ts` 的 `resolveCodingPlanCredential`）
//! 就是这条链，且国际版**拿不到 secretKey 就判登录失败**（bundle `dJr` 的
//! `requireSecretKey=true`）—— 理由是「宁可登录失败，也不要落一条永远签不了名、
//! 发出去必 401 的凭证」。本模块沿用同一取舍：换取失败就报错，不落半条账号。
//!
//! ── 两地的差别（三处，都在下面按 region 分支）───────────────
//!   1. 国际版多一步 `POST {host}/api/auth/z/login`（用 OAuth 令牌换 biz 令牌），
//!      国内版直接拿 OAuth 令牌当 biz 令牌用；
//!   2. 业务接口的 `Authorization` 头：国际版是 `Bearer {bizToken}`，
//!      国内版是**裸令牌**（参考实现逐字如此，实测两者都被上游接受）；
//!   3. 国际版必须拿到 secretKey，国内版拿不到就退回单段 `apiKey`。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic；持锁不做网络
//! （本模块不发锁，纯网络 + 纯计算）。


// ── 为什么整份保留（迁移时加的 allow）──────────────────────
// 「OAuth 令牌 → 能用于推理的 API Key」这一步在**登录链**里，而登录留在网关
// （它要开浏览器窗口）。插件这条路上，用户粘贴进来的已经是可用的 accessToken，
// 所以本模块没有调用方。
//
// 保留的理由：它是登录链的一部分，网关那份仍在使用；两份实现逐字相同，
// 将来把登录也搬进来时直接就有。
#![allow(dead_code)]
use serde_json::Value;

use crate::http::send_raw;
use crate::errors::GatewayError;

use super::region::Region;

/// 换取链路里每个请求的超时。
///
/// 与 `claim` 的 15 秒同量级：这条链最多 4~5 次往返，每次都是轻量 JSON
/// 接口（没有流式、没有大 body）。给 30 秒是给「国内网络 + 首次建密钥」
/// 留余量，同时兜住「某个接口挂住」——登录任务本身还有 5 分钟总超时，
/// 但那是**轮询**的兜底，不该让一次换取把整个登录拖到那时。
const REQUEST_TIMEOUT_MS: u64 = 30_000;

/// 网关在用户账号里认的密钥名（参考实现 `ZAI_API_KEY_NAME`）。
///
/// 它同时是「复用」的判据：同名密钥已存在就直接用，不存在才新建 ——
/// 于是重复登录不会在用户账号里堆出一串同名密钥。
const KEY_NAME: &str = "zcode-api-key";

/// 默认机构 / 默认项目的名字片段（参考实现的 `DEFAULT_ORG_MARKER` 等）。
///
/// 上游会返回该账号下的全部机构与项目；官方客户端用的是「默认机构 / 默认项目」
/// 那一个，所以优先按名字片段找，找不到就退回第一个（与参考实现同序）。
const DEFAULT_ORG_MARKER: &str = "默认机构";
const DEFAULT_PROJECT_MARKER: &str = "默认项目";

/// 换取编码套餐凭证：成功返回**直接可用的**推理凭证串。
///
/// 返回值就是发给推理端点的 `Authorization: Bearer <它>`：
/// 国际版恒为 `{apiKey}.{secret}`，国内版有 secret 时是两段、否则单段。
///
/// 失败一律返回可读中文（带上游 `msg`），由调用方决定怎么落 ——
/// 登录链上它是「登录失败」的原因，而不是一条静默的半成品账号。
pub async fn resolve(region: Region, access_token: &str) -> Result<String, GatewayError> {
    let oauth_token = access_token.trim();
    if oauth_token.is_empty() {
        return Err(GatewayError::with_status(
            401,
            "ZCode 登录响应里没有可换取推理凭证的登录态",
        ));
    }
    let host = region.biz_host();
    // 国际版：OAuth 令牌 → biz 令牌；国内版：OAuth 令牌直接当 biz 令牌
    let biz_token = match region {
        Region::Intl => z_login(host, oauth_token).await?,
        Region::Cn => oauth_token.to_string(),
    };
    // 头形态两地不同（见模块头第 2 条）
    let authorization = match region {
        Region::Intl => format!("Bearer {biz_token}"),
        Region::Cn => biz_token,
    };

    let (org_id, project_id) = customer_info(host, &authorization).await?;
    let api_key = find_or_create_api_key(host, &authorization, &org_id, &project_id).await?;
    let secret = copy_secret_key(host, &authorization, &org_id, &project_id, &api_key).await?;

    match region {
        // 国际版必须有 secret：没有它签不了名，发出去必 401（参考实现同样
        // 判登录失败，见模块头）。报错要指向「去哪看」而不是只说失败。
        Region::Intl if secret.trim().is_empty() => Err(GatewayError::with_status(
            502,
            "ZCode 国际版未返回密钥 secret（无法用于推理），请在 z.ai 控制台确认编码套餐已开通后重试",
        )),
        Region::Intl => Ok(format!("{api_key}.{secret}")),
        // 国内版拿不到 secret 也能用（单段 API Key）
        Region::Cn if secret.trim().is_empty() => Ok(api_key),
        Region::Cn => Ok(format!("{api_key}.{secret}")),
    }
}

/// 国际版第一步：`POST {host}/api/auth/z/login`，用 OAuth 令牌换 biz 令牌。
///
/// 响应形状按参考实现取三种可能（`access_token` / `accessToken` /
/// `data.access_token`）—— 上游在不同版本里换过位置，只认一种会静默拿到
/// 空串，而空串在下一步表现为一个与「登录态坏了」无异的 401。
async fn z_login(host: &str, oauth_token: &str) -> Result<String, GatewayError> {
    let url = format!("{host}/api/auth/z/login");
    let body = serde_json::json!({ "token": oauth_token });
    let payload = biz_request("POST", &url, "", Some(&body)).await?;
    let token = payload
        .get("access_token")
        .or_else(|| payload.get("accessToken"))
        .or_else(|| {
            payload
                .get("data")
                .and_then(|data| data.get("access_token"))
        })
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            GatewayError::with_status(502, "ZCode 登录换取失败：上游未返回访问令牌")
        })?;
    Ok(token.to_string())
}

/// 查默认机构与默认项目。
async fn customer_info(
    host: &str,
    authorization: &str,
) -> Result<(String, String), GatewayError> {
    let url = format!("{host}/api/biz/customer/getCustomerInfo");
    let payload = biz_request("GET", &url, authorization, None).await?;
    let orgs = payload
        .get("organizations")
        .or_else(|| payload.get("orgs"))
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
        .ok_or_else(|| {
            GatewayError::with_status(502, "ZCode 登录换取失败：该账号下没有可用机构")
        })?;
    let org = orgs
        .iter()
        .find(|item| {
            org_name(item)
                .map(|name| name.contains(DEFAULT_ORG_MARKER))
                .unwrap_or(false)
        })
        .or_else(|| orgs.first())
        .ok_or_else(|| {
            GatewayError::with_status(502, "ZCode 登录换取失败：该账号下没有可用机构")
        })?;
    let org_id = pick_id(org, &["organizationId", "id", "orgId"]).ok_or_else(|| {
        GatewayError::with_status(502, "ZCode 登录换取失败：机构缺少标识")
    })?;
    let projects = org
        .get("projects")
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
        .ok_or_else(|| {
            GatewayError::with_status(502, "ZCode 登录换取失败：默认机构下没有可用项目")
        })?;
    let project = projects
        .iter()
        .find(|item| {
            project_name(item)
                .map(|name| name.contains(DEFAULT_PROJECT_MARKER))
                .unwrap_or(false)
        })
        .or_else(|| projects.first())
        .ok_or_else(|| {
            GatewayError::with_status(502, "ZCode 登录换取失败：默认机构下没有可用项目")
        })?;
    let project_id = pick_id(project, &["projectId", "id"]).ok_or_else(|| {
        GatewayError::with_status(502, "ZCode 登录换取失败：项目缺少标识")
    })?;
    Ok((org_id, project_id))
}

/// 找同名密钥，没有就建一个（见 [`KEY_NAME`] 的说明）。
async fn find_or_create_api_key(
    host: &str,
    authorization: &str,
    org_id: &str,
    project_id: &str,
) -> Result<String, GatewayError> {
    let url = format!("{host}/api/biz/v1/organization/{org_id}/projects/{project_id}/api_keys");
    // 列表失败不致命（参考实现同样是 `catch { /* will create */ }`）：
    // 没有列表权限但能建密钥的账号依然可用。真正的失败留给「建也失败」那一步。
    if let Ok(payload) = biz_request("GET", &url, authorization, None).await {
        let found = payload
            .as_array()
            .into_iter()
            .flatten()
            .find(|item| {
                item.get("name").and_then(Value::as_str) == Some(KEY_NAME)
                    && item
                        .get("apiKey")
                        .and_then(Value::as_str)
                        .map(|key| !key.trim().is_empty())
                        .unwrap_or(false)
            })
            .and_then(|item| item.get("apiKey").and_then(Value::as_str))
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty());
        if let Some(key) = found {
            return Ok(key);
        }
    }
    let body = serde_json::json!({ "name": KEY_NAME });
    let created = biz_request("POST", &url, authorization, Some(&body)).await?;
    created
        .get("apiKey")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            GatewayError::with_status(502, "ZCode 登录换取失败：上游未返回新建密钥")
        })
}

/// 取密钥的 secret（`api_keys/copy/{apiKey}`）。
///
/// 失败与「没给 secret」在这里**同义**（都返回空串）：调用方按地区决定
/// 空值能不能接受 —— 国际版判失败、国内版退回单段（见 [`resolve`]）。
/// 这样区分的是「地区要求」，而不是把上游的一次 404 直接变成登录失败。
async fn copy_secret_key(
    host: &str,
    authorization: &str,
    org_id: &str,
    project_id: &str,
    api_key: &str,
) -> Result<String, GatewayError> {
    let url = format!(
        "{host}/api/biz/v1/organization/{org_id}/projects/{project_id}/api_keys/copy/{}",
        encode_path_segment(api_key)
    );
    let Ok(payload) = biz_request("GET", &url, authorization, None).await else {
        return Ok(String::new());
    };
    Ok(payload
        .get("secretKey")
        .or_else(|| payload.get("secret_key"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string())
}

/// 一次业务接口调用：按参考实现 `requestBizApi` 的信封语义解包。
///
/// `authorization` 为空表示**不发这个头**（`z/login` 是匿名接口，
/// 发一个空 Authorization 会让部分网关直接判 401）。
async fn biz_request(
    method: &str,
    url: &str,
    authorization: &str,
    body: Option<&Value>,
) -> Result<Value, GatewayError> {
    let mut headers: Vec<(String, String)> = Vec::new();
    if !authorization.trim().is_empty() {
        headers.push(("Authorization".to_string(), authorization.to_string()));
    }
    let response = send_raw(method, url, body, &headers, None, Some(REQUEST_TIMEOUT_MS))
        .await
        .map_err(|error| {
            if error.is_timeout() {
                GatewayError::with_status(504, "ZCode 登录换取超时，请重试")
            } else {
                GatewayError::with_status(502, format!("ZCode 登录换取失败: {error}"))
            }
        })?;
    let payload = response.payload.ok_or_else(|| {
        GatewayError::with_status(502, "ZCode 登录换取失败：上游响应不是 JSON")
    })?;
    // 信封判定与参考实现同序：`code` 缺失视为成功（有的接口直接回数据体），
    // 取值 0 / 200（数字或字符串）都算成功。
    if let Some(code) = payload.get("code").or_else(|| payload.get("status")) {
        if !biz_code_ok(code) {
            let message = payload
                .get("msg")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("上游未说明原因");
            return Err(GatewayError::with_status(
                502,
                format!("ZCode 登录换取被拒：{message}"),
            ));
        }
    }
    Ok(payload.get("data").cloned().unwrap_or(payload))
}

/// `code` 是不是成功值（0 / 200，数字与字符串两种写法都认）
fn biz_code_ok(code: &Value) -> bool {
    if let Some(number) = code.as_i64() {
        return number == 0 || number == 200;
    }
    matches!(
        code.as_str().map(str::trim),
        Some("0") | Some("200") | Some("")
    )
}

/// 机构/项目的展示名（上游不同版本用过 `organizationName` / `name` 等）
fn org_name(item: &Value) -> Option<&str> {
    item.get("organizationName")
        .or_else(|| item.get("name"))
        .and_then(Value::as_str)
}

fn project_name(item: &Value) -> Option<&str> {
    item.get("projectName")
        .or_else(|| item.get("name"))
        .and_then(Value::as_str)
}

/// 从候选键里取第一个非空字符串（上游的 id 字段名换过几轮）
fn pick_id(item: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| item.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// 路径段编码：密钥里可能带 `.`（两段形式）与其它保留字符，直接拼进 URL
/// 会让 `copy/{apiKey}` 断在第一个非法字符上。
///
/// 只放行 RFC 3986 的 unreserved 集合 —— 与 `claim::urlencode` 同一套规则，
/// 但那条是查询值编码（`&`/`=` 必须转义），路径段要的更少，因此这里不复用，
/// 免得哪天有人把查询值的宽松规则挪到路径上。
fn encode_path_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        let ch = *byte as char;
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '~') {
            out.push(ch);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}
