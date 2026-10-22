//! Qoder 的 PKCE 设备授权（国际版与中国版同构，只有站点主机不同）。
//! verifier 只留在后端，不能进入前端状态或日志。

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
    region: endpoints::Region,
}

impl DeviceLogin {
    pub fn new(region: endpoints::Region) -> Result<Self, GatewayError> {
        let verifier = machine::random_secret()?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let nonce = machine::random_uuid()?.replace('-', "");
        let machine_id = machine::machine_id()?;
        let mut auth_url = url::Url::parse(&region.device_login_url())
            .map_err(|_| GatewayError::with_status(500, "Qoder 授权地址配置无效"))?;
        auth_url.query_pairs_mut()
            .append_pair("challenge", &challenge)
            .append_pair("challenge_method", "S256")
            .append_pair("machine_id", &machine_id)
            .append_pair("nonce", &nonce);
        let mut poll_url = url::Url::parse(&format!("{}{}", region.open_api(), endpoints::DEVICE_POLL_PATH))
            .map_err(|_| GatewayError::with_status(500, "Qoder 轮询地址配置无效"))?;
        poll_url.query_pairs_mut()
            .append_pair("nonce", &nonce)
            .append_pair("verifier", &verifier)
            .append_pair("challenge_method", "S256");
        Ok(Self {
            state: machine::random_secret()?,
            auth_url: auth_url.to_string(),
            poll_url: poll_url.to_string(),
            machine_id,
            region,
        })
    }

    pub async fn poll(&self) -> Result<Option<Credentials>, GatewayError> {
        let response = auth::request("GET", &self.poll_url, None, &endpoints::open_api_headers(None), None).await?;
        if response.status == 202 || response.status == 404 {
            return Ok(None);
        }
        let data = auth::payload(response, "设备授权")?;
        let token = credentials::secret(&data, &["token"])?;
        if token.is_empty() {
            return Err(GatewayError::with_status(502, "Qoder 设备授权响应缺少 token"));
        }
        let refresh_token = credentials::secret(&data, &["refresh_token"])?;
        if refresh_token.contains('|') {
            return Err(GatewayError::with_status(502, "Qoder 设备授权响应的 refresh_token 格式无效"));
        }
        let user_id = credentials::text(&data, &["user_id"]);
        let mut credentials = Credentials::from_payload(&json!({
            "mode": self.region.id(),
            "accessToken": token,
            "refreshToken": if refresh_token.is_empty() { String::new() }
                else { format!("{}|{}|{}", refresh_token, user_id, self.machine_id) },
            "userId": user_id,
            "machineId": self.machine_id,
            "expiresAt": credentials::timestamp(data.get("expires_at"))
                .unwrap_or_else(|| logging::now_ms() + 30 * 24 * 60 * 60 * 1000),
        }))?;
        match auth::fetch_profile(&credentials.access_token, credentials.region, None).await {
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
/// 当作键存进进程内表（见 [`FLOWS`]）—— 但把 state 编进 flow 之后，日志与
/// 排障能直接对上「这次登录是哪一次发起的」，不必反查表。
const FLOW_PREFIX: &str = "qoder-device-";

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

/// 发起登录：`method == "browser"` 走设备授权；`token` 由 `accounts::normalize`
/// 处理（这里只回一条「请直接粘贴」的说明）。
pub(super) async fn login_start(
    region: endpoints::Region,
    method: &str,
    _payload: &serde_json::Value,
) -> Result<agent2api_provider_protocol::LoginStart, String> {
    if method != "browser" {
        return Err(
            "Qoder 的「填写凭证」请直接在账号页粘贴个人访问令牌（PAT）；网页登录请点「网页登录」"
                .to_string(),
        );
    }
    let flow = DeviceLogin::new(region).map_err(|error| error.message)?;
    let flow_id = format!("{FLOW_PREFIX}{}", flow.state);
    let auth_url = flow.auth_url.clone();
    let state = flow.state.clone();
    // 存进表（同名覆盖：同一 state 不可能重复，但锁中毒时要能继续）
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
            "发起 Qoder {}网页登录（设备授权，state={state}）",
            region.label()
        ),
    );
    Ok(agent2api_provider_protocol::LoginStart {
        flow: flow_id,
        url: auth_url,
        instructions: format!(
            "在打开的页面里完成 Qoder {} 的授权，完成后会自动收下账号",
            region.label()
        ),
        // 轮询型：授权在上游域名终结，网关按这个间隔问 `login.poll`
        poll_interval_ms: 2_000,
        external_browser: true,
        allowed_hosts: vec![
            "qoder.sh".to_string(),
            "qoder.com".to_string(),
            "qoder.com.cn".to_string(),
        ],
    })
}

/// 轮询登录进度。
pub(super) async fn login_poll(
    region: endpoints::Region,
    flow: &str,
) -> Result<agent2api_provider_protocol::LoginPoll, String> {
    let Some(entry) = ({
        match flows().lock() {
            Ok(table) => table.get(flow).map(|entry| DeviceLogin {
                state: entry.state.clone(),
                auth_url: entry.auth_url.clone(),
                poll_url: entry.poll_url.clone(),
                machine_id: entry.machine_id.clone(),
                region: entry.region,
            }),
            Err(poisoned) => poisoned.into_inner().get(flow).map(|entry| DeviceLogin {
                state: entry.state.clone(),
                auth_url: entry.auth_url.clone(),
                poll_url: entry.poll_url.clone(),
                machine_id: entry.machine_id.clone(),
                region: entry.region,
            }),
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
                account: super::accounts::account_of(region, &credentials, "web", None),
            })
        }
        Err(error) if error.code.as_deref() == Some("qoder_transport") => {
            // 暂时不可达：继续等（与网关 `run_qoder_login` 同一处置）
            logging::verbose("[Login]", "Qoder 设备授权暂时不可达，继续等待");
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
