//! 设备授权（PKCE，轮询型）。verifier 只留在后端，不能进入前端状态或日志。
//!
//! ── 四个站点都走这一条（2026-11-03 复核）────────────────────────
//! 千问办公的 `/device/selectAccounts` 一直是可用的：上游只要求 `nonce` 是
//! 标准 UUID（带横线）。本模块曾把横线去掉，于是每次都被判成非法请求
//! （`400 INVALID_DEVICE_FLOW`），当时误判成「入口已停用」并改走授权码 +
//! 本机回调 —— 而授权页对 `qwenwork-desktop-app` 只认它自己登记的
//! `https://gateway.qwenwork.*/oauth/callback`，本机回环一律
//! `redirect_uri does not match`，那条路本来就走不通。现在恢复设备授权，
//! 四个站点（Qoder 国内 / 国际、千问办公国内 / 国际）共用这一份实现。
//!
//! ── Qoder 两个站点同一套流程，只有主机名不同 ──────────────────
//! 授权页、轮询端点、参数形状两站一致；`client_id` / `redirect_uri` 历史上
//! 不带也能登录，保持不变（见 [`Site::device_client_id`]）。
//!
//! ── 千问办公没有作业令牌交换 ────────────────────────────────
//! 它的 `/api/v1/jobToken/exchange` 不存在，设备令牌本身就是对话令牌
//! （见 [`Site::device_only`]）。

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::errors::GatewayError;
use crate::logging;

use super::credentials::{self, Credentials};
use super::{auth, endpoints, machine};

pub struct DeviceLogin {
    pub state: String,
    pub auth_url: String,
    poll_url: String,
    machine_id: String,
    site: endpoints::Site,
}

impl DeviceLogin {
    pub fn new(site: endpoints::Site) -> Result<Self, GatewayError> {
        let verifier = machine::random_secret()?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        // nonce 必须是标准 UUID（带横线）：上游按 UUID 解析它，去横线会被判成
        // 非法参数并整条拒绝（`INVALID_DEVICE_FLOW`，见模块头）。
        let nonce = machine::random_uuid()?;
        let machine_id = machine::machine_id_for(site)?;
        let mut auth_url = url::Url::parse(&site.device_login_url())
            .map_err(|_| GatewayError::with_status(500, "授权地址配置无效"))?;
        auth_url
            .query_pairs_mut()
            .append_pair("challenge", &challenge)
            .append_pair("challenge_method", "S256")
            .append_pair("machine_id", &machine_id)
            .append_pair("nonce", &nonce);
        if let Some(client_id) = site.device_client_id() {
            auth_url.query_pairs_mut().append_pair("client_id", client_id);
        }
        if let Some(redirect) = site.device_redirect() {
            auth_url.query_pairs_mut().append_pair("redirect_uri", redirect);
        }
        let mut poll_url = url::Url::parse(&format!("{}{}", site.open_api(), endpoints::DEVICE_POLL_PATH))
            .map_err(|_| GatewayError::with_status(500, "轮询地址配置无效"))?;
        poll_url.query_pairs_mut()
            .append_pair("nonce", &nonce)
            .append_pair("verifier", &verifier)
            .append_pair("challenge_method", "S256");
        Ok(Self {
            state: machine::random_secret()?,
            auth_url: auth_url.to_string(),
            poll_url: poll_url.to_string(),
            machine_id,
            site,
        })
    }

    pub async fn poll(&self) -> Result<Option<Credentials>, GatewayError> {
        let response = auth::request(
            "GET",
            &self.poll_url,
            None,
            &auth::account_headers(self.site, None),
            None,
        )
        .await?;
        if response.status == 202 || response.status == 404 {
            return Ok(None);
        }
        let data = auth::payload(response, "设备授权")?;
        let token = credentials::secret(&data, &["token", "device_token"])?;
        if token.is_empty() {
            return Err(GatewayError::with_status(502, "设备授权响应缺少 token"));
        }
        let refresh_token = credentials::secret(&data, &["refresh_token"])?;
        if refresh_token.contains('|') {
            return Err(GatewayError::with_status(502, "设备授权响应的 refresh_token 格式无效"));
        }
        let user_id = credentials::text(&data, &["user_id", "uid"]);
        let mut credentials = Credentials::from_payload(&json!({
            "mode": self.site.id(),
            "accessToken": token,
            "refreshToken": if refresh_token.is_empty() { String::new() }
                else { format!("{}|{}|{}", refresh_token, user_id, self.machine_id) },
            "userId": user_id,
            "machineId": self.machine_id,
            // 设备令牌的绝对到期时刻（千问办公的轮询响应带它）；缺省按 30 天算
            "expiresAt": credentials::timestamp(data.get("expires_at"))
                .unwrap_or_else(|| logging::now_ms() + 30 * 24 * 60 * 60 * 1000),
        }))?;
        // 站点以 provider 身份为准（见 `refresh::snapshot` 的同一条规则）
        credentials.set_site(self.site);
        match auth::fetch_profile(&credentials.access_token, self.site, None).await {
            Ok(profile) => auth::apply_profile(&mut credentials, &profile)?,
            Err(error) if credentials.user_id.is_empty() => return Err(error),
            Err(_) => {}
        }
        credentials.complete_identity()?;
        Ok(Some(credentials))
    }
}

// ─── 插件登录协议的入口（轮询型）──────────────────────────────

/// 设备授权的 flow id 前缀（`<prefix><state>`；`state` 由 `DeviceLogin` 生成）。
///
/// ── 为什么把 state 编进 flow ────────────────────────────────
/// `login.poll` 只收到一个 `flow` 字符串，而 `DeviceLogin` 要保留 verifier 与
/// poll_url（都只留在后端，见模块头）。插件是**单进程**，因此可以把 flow
/// 当作键存进进程内表（见 [`flows`]）—— 但把 state 编进 flow 之后，日志与
/// 排障能直接对上「这次登录是哪一次发起的」，不必反查表。
fn flow_prefix(site: endpoints::Site) -> String {
    format!("{}-device-", site.provider_id())
}

/// 进程内的设备授权表（`flow` → `DeviceLogin`）。
///
/// ── 为什么可以放进程内 ──────────────────────────────────────
/// 授权在几分钟内完成（网关侧超时 5 分钟），而插件进程在网关运行期间不会重启。
/// 进程真重启了，那次登录本来也就失败了（用户重新点一次即可）。
fn flows() -> &'static std::sync::Mutex<std::collections::HashMap<String, DeviceLogin>> {
    static FLOWS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, DeviceLogin>>,
    > = std::sync::OnceLock::new();
    FLOWS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 发起登录：四个站点统一走设备授权（见模块头）。
/// `token` 由 `accounts::normalize` 处理（这里只回一条「请直接粘贴」的说明）。
pub(super) async fn login_start(
    site: endpoints::Site,
    method: &str,
    _payload: &serde_json::Value,
) -> Result<agent2api_provider_protocol::LoginStart, String> {
    if method != "browser" {
        return Err(if site.supports_pat() {
            "「填写凭证」请直接在账号页粘贴个人访问令牌（PAT）；网页登录请点「网页登录」".to_string()
        } else {
            format!("{}请点「网页登录」（这家没有可粘贴的令牌）", site.display_name())
        });
    }
    let flow = DeviceLogin::new(site).map_err(|error| error.message)?;
    let flow_id = format!("{}{}", flow_prefix(site), flow.state);
    let auth_url = flow.auth_url.clone();
    let state = flow.state.clone();
    match flows().lock() {
        Ok(mut table) => {
            table.insert(flow_id.clone(), flow);
        }
        Err(poisoned) => {
            poisoned.into_inner().insert(flow_id.clone(), flow);
        }
    }
    logging::log(
        "[Login]",
        &format!(
            "发起 {} 网页登录（设备授权，state={state}）",
            site.display_name()
        ),
    );
    Ok(agent2api_provider_protocol::LoginStart {
        flow: flow_id,
        url: auth_url,
        instructions: format!(
            "在打开的页面里完成 {} 的授权，完成后会自动收下账号",
            site.display_name()
        ),
        // 轮询型：授权在上游域名终结，网关按这个间隔问 `login.poll`
        poll_interval_ms: 2_000,
        external_browser: true,
        allowed_hosts: site.allowed_login_hosts(),
    })
}

/// 轮询登录进度（设备授权型）。
pub(super) async fn login_poll(
    _site: endpoints::Site,
    flow: &str,
) -> Result<agent2api_provider_protocol::LoginPoll, String> {
    let Some(entry) = ({
        match flows().lock() {
            Ok(table) => table.get(flow).map(clone_entry),
            Err(poisoned) => poisoned.into_inner().get(flow).map(clone_entry),
        }
    }) else {
        return Ok(agent2api_provider_protocol::LoginPoll::Failed {
            message: "这次登录流程已失效（插件进程重启过？），请重新发起".to_string(),
        });
    };
    match entry.poll().await {
        Ok(None) => Ok(agent2api_provider_protocol::LoginPoll::Pending {
            hint: Some("等待你在浏览器里完成授权…".to_string()),
        }),
        Ok(Some(credentials)) => {
            // 授权完成：从表里移除（一次性）
            remove_flow(flow);
            Ok(agent2api_provider_protocol::LoginPoll::Done {
                account: super::accounts::account_of(credentials.site, &credentials, "web", None),
            })
        }
        Err(error) if error.code.as_deref() == Some("qoder_transport") => {
            // 暂时不可达：继续等（与网关同一条处置）
            logging::verbose("[Login]", "设备授权暂时不可达，继续等待");
            Ok(agent2api_provider_protocol::LoginPoll::Pending {
                hint: Some("授权服务暂时不可达，正在重试…".to_string()),
            })
        }
        Err(error) => {
            remove_flow(flow);
            Ok(agent2api_provider_protocol::LoginPoll::Failed {
                message: error.message,
            })
        }
    }
}

fn clone_entry(entry: &DeviceLogin) -> DeviceLogin {
    DeviceLogin {
        state: entry.state.clone(),
        auth_url: entry.auth_url.clone(),
        poll_url: entry.poll_url.clone(),
        machine_id: entry.machine_id.clone(),
        site: entry.site,
    }
}

/// 取消一次登录：把这一轮从表里摘掉（表项是一次性的）。
pub(super) async fn login_cancel(_site: endpoints::Site, flow: &str) {
    remove_flow(flow);
}

/// 从表里移除一个 flow（成功或失败都移除：它是一次性的）。
fn remove_flow(flow: &str) {
    match flows().lock() {
        Ok(mut table) => {
            table.remove(flow);
        }
        Err(poisoned) => {
            poisoned.into_inner().remove(flow);
        }
    }
}
