//! Trae 的凭据续期（`ExchangeToken` 的 refreshToken 路径）。
//!
//! ── 为什么这条链的写法比别家更保守 ────────────────────────
//! `refreshToken` **每次换发都会轮换**（旧的当场作废）。这意味着任何一次
//! "发了请求但没把结果存下来"都是**把账号弄丢**：新串在内存里没落盘、
//! 旧串已经被服务端作废。所以这里三条规矩是硬性的：
//!   ① 失败**不改写**任何字段（参考实现同一条），旧的 refreshToken 仍可重试；
//!   ② 成功后**先落盘再往下走**（落盘侧的纪律在 `account_store::trae_accounts`）；
//!   ③ 同一凭据的并发续期走单飞表，只打上游一次 —— 否则两个请求各自拿到
//!      不同的新串，后存的那个把前一个覆盖掉，账号就废了。
//!
//! 到期判定不只看临期：参考实现还加了一条"签发龄超 15 天也换"（服务端有
//! 吊销旧凭据的风险），所以 `needs_refresh` 的两个条件都要传（见 `credentials.rs`）。

use std::time::Duration;

use serde_json::{Value, json};

use crate::errors::GatewayError;
use crate::flight::{Join, Table};
use agent2api_provider_protocol::WireProxy as ResolvedProxy;

use super::credentials::Credential;
use super::headers::CLIENT_USER_AGENT;
use super::http::{describe_candidates, post_json};

/// 提前续期的窗口（参考实现 `defaultRefreshSkew = 24h`）。
pub const REFRESH_LEAD_MS: i64 = 24 * 3600 * 1000;
/// 签发龄上限：超过就换，即使还没到期（`auth.issuedRotateMax` 同一条）。
pub const MAX_ISSUE_AGE_MS: i64 = 15 * 24 * 3600 * 1000;

/// 换证端点（相对各 OAuth host）。注意与**授权码**换证不是同一个路径：
/// 那条是 `/trae/api/v3/oauth/ExchangeToken`，见 `oauth.rs`。
pub const REFRESH_PATH: &str = "/cloudide/api/v3/trae/oauth/ExchangeToken";

/// 兜底 host（凭据里的 `apiHost` 不可用时依次试这两个）。
///
/// 顺序照参考实现的 `exchangeHosts(primary, oauth)`：账号 apiHost →
/// **OAuth host**（= `https://api.trae.cn`，登录时就写进凭据了）→ 平台备用源
/// （`api.trae.com.cn`）。先前写成"备用源在前"，只在 apiHost 为空时看得见差别
/// —— 而那正好是手工粘贴的凭据的常态。
const FALLBACK_HOSTS: [&str; 2] = ["https://api.trae.cn", "https://api.trae.com.cn"];

/// 单飞表：按 refreshToken 的指纹排队（同一个凭据的并发请求只有一个真打上游）。
static FLIGHTS: std::sync::OnceLock<Table<Credential>> = std::sync::OnceLock::new();

/// 候选 host 顺序：凭据里那份优先，然后是两个兜底（参考实现 `exchangeHosts`）。
///
/// 这条 host 表**不只服务续期**：`GetUserInfo`（登录后的身份回读）打的是同一组
/// host、同一份优先级，所以把它单独暴露出来，别让第二处再抄一遍顺序
/// （两处各写一份的结局就是"一处 5s、另一处 30s"那种漂移）。
pub fn candidate_hosts(api_host: &str) -> Vec<String> {
    let mut hosts = Vec::new();
    for host in [api_host.trim()].iter().chain(FALLBACK_HOSTS.iter()) {
        let host = host.trim_end_matches('/');
        if host.is_empty() {
            continue;
        }
        let host = if host.starts_with("http://") || host.starts_with("https://") {
            host.to_string()
        } else {
            format!("https://{host}")
        };
        if !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    hosts
}

/// 续期端点的候选 URL（`candidate_hosts` × 本家的换证路径）。
pub fn refresh_candidates(api_host: &str) -> Vec<String> {
    candidate_hosts(api_host).into_iter().map(|host| format!("{host}{REFRESH_PATH}")).collect()
}

/// 续期请求体。`ClientID` 必须按 variant 选：参考实现里出过事故 ——
/// 裸凭据缺 variant 时一律解析成 cn 的 ClientID，SOLO 登录当场换发出
/// 跨类 token，之后积分/签到全 401（`code=1001`），而聊天还是正常的。
pub fn refresh_body(client_id: &str, refresh_token: &str) -> Value {
    json!({"ClientID": client_id, "RefreshToken": refresh_token, "ClientSecret": "-", "UserID": ""})
}

/// 续期该试哪些 `ClientID`（顺序 = 候选，第一个被上游承认的就用它，并回写进凭据）。
///
/// ── 为什么不是一个而是两个 ──────────────────────────────────
/// `variant` 表达的是**转发面**的谱系，`ClientID` 表达的是**这张 refreshToken
/// 当初在哪个 OAuth 应用里 mint**，二者可以不一致，而续期只认后者。生产实测
/// （2026-10-02，NAS 三条从 CPA 迁来的账号）：库里 `variant=solo`，CPA 盘上的
/// `authClientId` 与当年 `ExchangeToken` 响应的 `Result.ClientID` 却都是
/// **非 solo** 的 `ono9krqynydwx5` ⇒ 按 variant 推出来 `en1oxy7wnw8j9n` 去续期，
/// 上游回 `400 {"Code":"10101","__Message.error":"refresh token is not matched
/// to the client"}`，三条凭据从第一次进入续期窗口起就一直续不上。
///
/// 判据不是猜的：拿一枚**假**令牌打同一个端点，回的是
/// `401 20101 "refresh token is invalid"`（换 ClientID、换 UserID 都一样），
/// 与上面那条不同形 —— 所以「not matched to the client」说的是"这枚令牌存在，
/// 但不属于你报的这个 client"，不是"令牌无效"的委婉说法。
///
/// 两个候选的成本上限很小：一次成功续期约两周才发生一次，而**失败不换发**
/// （参考实现第一条规矩），多发一发不会把 refreshToken 用掉。
pub fn refresh_client_ids(credential: &Credential) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    let variant_id = super::oauth::client_id_for(credential.variant());
    // 顺序 = 可信度：① 凭据里记着的归属（上游自己回显过的）②按 variant 推的
    // ③ 本家已知表里的另一把（未知归属时的最后一搏，成功就把结果记回凭据）
    let recorded = credential.auth_client_id.trim();
    let mut candidates: Vec<&str> = Vec::new();
    if !recorded.is_empty() {
        candidates.push(recorded);
    }
    candidates.push(variant_id);
    candidates.extend(super::oauth::other_client_id(variant_id));
    for id in candidates {
        let id = id.trim();
        if !id.is_empty() && !ids.iter().any(|seen| seen == id) {
            ids.push(id.to_string());
        }
    }
    ids
}

/// 从换证响应里读上游承认的那把 `ClientID`（`Result.ClientID`）。
///
/// 读不到给空串 —— 调用方据此**保留**原值，不许把已有字段抹成空。
pub fn response_client_id(body: &str) -> String {
    let Ok(parsed) = serde_json::from_str::<Value>(body) else {
        return String::new();
    };
    ["Result", "result"]
        .iter()
        .find_map(|key| parsed.get(*key).and_then(Value::as_object))
        .and_then(|object| first_string(object, &["ClientID", "clientId", "client_id"]))
        .unwrap_or_default()
}

/// 解析换证响应。
///
/// 上游把令牌放在 `Result` 里，且字段名有两套写法（`Token` / `token` 等），
/// 所以取值要像参考实现那样逐个试；`TokenExpireAt` 是**毫秒**。
///
/// ── 到期字段数字与**字符串**都要认（issue #263）──────────────
/// 中国版的 `TokenExpireAt` 是**字符串**形态（参考实现 Trae2api-cn 的两处
/// 测试夹具原样：`"TokenExpireAt": "1900000000000"`，他们的解析全程 `str()`
/// 兜底）。旧解析只走 `as_i64` / `as_f64`，字符串静默落成 0 —— 面板上的
/// 有效期与自动续期节奏（`needs_refresh` 看到期）一起坏，而且不报错。
pub fn parse_refresh_response(body: &str) -> Result<(String, String, i64), String> {
    let parsed: Value = serde_json::from_str(body).map_err(|error| format!("换证响应不是合法 JSON：{error}"))?;
    let candidates = ["Result", "result"];
    for key in candidates {
        if let Some(object) = parsed.get(key).and_then(Value::as_object) {
            if let Some(token) = first_string(object, &["Token", "token", "AccessToken", "accessToken"]) {
                let refresh = first_string(object, &["RefreshToken", "refreshToken", "refresh_token"]).unwrap_or_default();
                let expires = ["TokenExpireAt", "tokenExpireAt", "ExpiresAt", "expiresAt"]
                    .iter()
                    .find_map(|field| timestamp_field(object.get(*field)))
                    .unwrap_or(0);
                return Ok((token, refresh, normalize_expires_at(expires)));
            }
        }
    }
    Err("换证响应里没有 access token".to_string())
}

/// 时间戳字段的三种形态都认：整数、浮点、**字符串**数字。
fn timestamp_field(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number.as_i64().or_else(|| number.as_f64().map(|raw| raw as i64)),
        Value::String(text) => text.trim().parse::<f64>().ok().map(|raw| raw as i64),
        _ => None,
    }
}

/// 秒与毫秒的归一（同 `Credential::expires_at_ms` 的判据）。
pub fn normalize_expires_at(value: i64) -> i64 {
    if value <= 0 {
        return 0;
    }
    if value < 100_000_000_000 { value * 1000 } else { value }
}

fn first_string(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    // 空串不算命中：参考实现的 `jsonString` 把 `""` 与"没有这个键"归成同一件事
    // （`Token:""` 要往下一个键/下一层继续找，而不是当成换证成功）。
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str).filter(|text| !text.is_empty()))
        .map(str::to_string)
}

/// 上游这次失败是不是在说「归属不对」。**只有**这种失败值得换一把 ClientID 再试。
///
/// 反面同样是实测出来的：一枚假令牌打这个端点回的是 `401 20101
/// "refresh token is invalid"`，与归属无关 —— 那种情况下换 ClientID 只是白撞；
/// 传输层失败（`Err`）也一样，跟参数无关。少这一格判据，一次续期就要多打
/// 一到两发没有意义的请求。
fn attribution_reject(status: u16, body: &str) -> bool {
    if status != 400 {
        return false;
    }
    let text = body.to_lowercase();
    text.contains("10101") || text.contains("not matched to the client")
}

/// 打上游换新凭据（不含存储；调用方负责先落盘再继续）。
///
/// 返回的是**新凭据**（原凭据的其余字段带过来），失败时原凭据不受影响。
///
/// `urls` 由调用方给（生产路径就是 `refresh_candidates(&credential.api_host)`）。
/// 把候选列表做成入参而不是在函数里派生，是为了让"多 host 轮询""失败不改写"
/// "单飞合并"这三条能在测试里用**进程内 mock**验到：写死派生逻辑的测试会
/// 一路打到真实上游（v0 就踩过 —— 404 之后兜底 host 是 api.trae.cn，
/// 单测里那条假 refreshToken 真的被发出去了，判 401 还是 502 全看本机能不能
/// 联网，等于把一个纪律性断言挂在网络上）。
pub async fn refresh_once(
    credential: &Credential,
    urls: &[String],
    proxy: Option<&ResolvedProxy>,
) -> Result<Credential, GatewayError> {
    if !credential.can_refresh() {
        return Err(GatewayError::with_status(401, "Trae 凭据里没有 refreshToken，无法续期（请重新登录）"));
    }
    let headers = vec![("User-Agent", CLIENT_USER_AGENT.to_string())];
    let mut errors = Vec::new();
    // 外层是 ClientID 候选、内层是 host 候选：归属不对时换归属，而不是换域名
    for client_id in refresh_client_ids(credential) {
        let body = refresh_body(&client_id, credential.refresh_token.trim());
        let mut wrong_attribution = false;
        for url in urls {
            let outcome = post_json(url, &body, &headers, Duration::from_secs(30), proxy).await;
            // 每条失败都要带上用的哪把 ClientID：今天的 10101 排查里，
            // 日志只写了 host 与上游原文，"归属不对"这个原因看不出来
            let label = format!("{url}（ClientID {client_id}）");
            match outcome {
                Ok(reply) if reply.status >= 400 => {
                    if attribution_reject(reply.status, &reply.body) {
                        wrong_attribution = true;
                    }
                    errors.push(format!("{} => HTTP {} {}", label, reply.status, trim(&reply.body)));
                }
                Ok(reply) => match parse_refresh_response(&reply.body) {
                    Ok((access_token, refresh_token, expires_at_ms)) => {
                        let mut next = credential.clone();
                        next.access_token = access_token;
                        // 上游没回新 refreshToken 时**保留旧的**：清空它等于让账号变成
                        // "过期后不可恢复"，而参考实现也是这么兜的。
                        if !refresh_token.is_empty() {
                            next.refresh_token = refresh_token;
                        }
                        next.expires_at = expires_at_ms;
                        // 把上游承认的那把归属记下来（响应里没有就沿用本次用的那把）：
                        // 下一次续期直接一发命中，不再靠 variant 猜
                        let acknowledged = response_client_id(&reply.body);
                        next.auth_client_id = if acknowledged.is_empty() { client_id } else { acknowledged };
                        return Ok(next);
                    }
                    Err(reason) => errors.push(format!("{label} => {reason}")),
                },
                Err(error) => errors.push(format!("{label} => {}", error.message)),
            }
        }
        // 换下一把归属的唯一理由：上游明说"这枚令牌不属于你报的这个 client"。
        // 401（凭据被拒 = 该重登）与传输层失败都不是这个问题，换参数再打只是白撞
        if !wrong_attribution {
            break;
        }
    }
    // 4xx 语义原样透出（"凭据被拒（该重登）"与"上游挂了（该重试）"不能长一样）：
    // 全轮失败时按最后一个状态判，其它情况是 502。
    let status = if errors.iter().any(|error| error.contains("HTTP 401")) { 401 } else { 502 };
    Err(GatewayError::with_status(status, describe_candidates(urls, &errors)))
}

/// 带单飞的续期：并发请求只打上游一次，其余等结果。
pub async fn refresh_shared(
    credential: &Credential,
    urls: &[String],
    proxy: Option<&ResolvedProxy>,
) -> Result<Credential, GatewayError> {
    let key = crate::flight::fingerprint(&credential.refresh_token);
    match FLIGHTS.get_or_init(Table::new).join(&key) {
        Join::Waiter(waiter) => waiter.wait().await,
        Join::Leader(leader) => {
            let result = refresh_once(credential, urls, proxy).await;
            leader.finish(result.clone());
            result
        }
    }
}

/// 上游错误体截断用（`www.*` 那类 host 会回一整页 HTML，不截会把日志淹掉）。
fn trim(text: &str) -> String {
    let collapsed: String = text.chars().filter(|character| !character.is_control()).collect();
    match collapsed.char_indices().nth(200) {
        Some((index, _)) => format!("{}…", &collapsed[..index]),
        None => collapsed,
    }
}

