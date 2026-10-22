//! 一轮 Trae 网页登录的编排：绑端口 → 要授权地址 → 等回调 → 换凭据 → 读身份。
//!
//! ── 这一段为什么必须串在一个对象里 ──────────────────────────
//! 三样东西同生共死，分开传就会漏掉某一条收尾路径：
//!   * **回调监听器**（端口）—— 授权地址里的 `auth_callback_url` 就是它，
//!     地址发出去之后端口不能再变（变了回调就落到别的进程上）；
//!   * **PKCE 的 code_verifier** 与**设备密钥对** —— 换证请求要用当轮的那一份，
//!     公钥已经在授权页那一步跟 DeviceID/MachineID 绑过了；
//!   * **guidance 拿到的 loginHost** —— 回调可能带回另一个（`loginHost` 键），
//!     以回调为准、以发起时的为兜底。
//!
//! 所以它们合在 [`Session`] 里，`Drop` 即释放端口。
//!
//! ── 回调的两条分支（都实测存在于上游）───────────────────────
//! `authCode`（标准授权码）与 `refreshToken`（直接给续期串）。参考实现的优先级
//! 是 **refreshToken 在前**（`pending_persist.go:363`）：拿到续期串就直接走续期链
//! 换新凭据，**换证失败也仍然把这条串存成凭据**（`accessToken = refreshToken`），
//! 因为「存下一条可能可用的凭据」比「丢掉一次已经花掉用户点击的授权」划算。
//! 授权码那条同理：响应只有 refreshToken 时也用它的值当 accessToken 兜底。
//!
//! ── 谁负责超时与取消（迁移时改的）───────────────────────────
//! 网关那版由 `core::login::trae` 用任务表判（`/cancel` 之后 `/wait` 立刻 404、
//! 端口释放）。插件里那张表在 `login.rs`：`login_cancel` 显式调
//! [`Session::close`]，超时由网关的 `LOGIN_TIMEOUT_MS` 驱动 `login.poll` 失败。
//! `complete()` 本身只等回调，不设超时 —— 它跑在后台任务里，超时由调用方管。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap / expect / panic。

use crate::errors::GatewayError;
use agent2api_provider_protocol::WireProxy as ResolvedProxy;

use super::callback_server::CallbackListener;
use super::credentials::Credential;
use super::oauth::{
    exchange_auth_code, request_login_guidance, verification_uri, Callback, LoginContext,
    DEFAULT_LOGIN_HOST,
};
use super::profile::{callback_identity, get_user_info, Identity};
use super::refresh::{refresh_candidates, refresh_once, response_client_id};

/// 登录落盘时写死的两个字段（参考实现 `selfCompleteCN` 同值）。
///
/// `domain` 是凭据的谱系标记（决定 ClientID 与目录血统），`api_host` 是
/// OAuth/积分那条链的第一候选 —— 两者都**不**跟着 loginHost 漂：
/// CN 账号的兑换永远打固定 origin（见 `oauth_fallback.go` 注释）。
pub const CN_DOMAIN: &str = "trae.cn";

/// 进行中的一轮登录。
pub struct Session {
    pub context: LoginContext,
    pub login_host: String,
    pub auth_url: String,
    listener: CallbackListener,
}

impl Session {
    /// 起一轮登录：先绑端口，再问 guidance 要登录 host，最后拼授权地址。
    ///
    /// 顺序是有讲究的：**端口必须先落地**，因为 `LoginContext::new` 拿端口去
    /// 拼 `auth_callback_url`；反过来（先造上下文再绑）就会让地址里的端口
    /// 与实际监听的端口不是同一个 —— 上游那个正则照样放行，回调却落错地方。
    pub async fn begin(variant: &str, proxy: Option<&ResolvedProxy>) -> Result<Self, String> {
        let listener = CallbackListener::bind().await?;
        let context = LoginContext::new(variant, listener.port(), crate::http::now_ms())?;
        let login_host = request_login_guidance(proxy).await;
        let auth_url = verification_uri(&login_host, &context);
        Ok(Self { context, login_host, auth_url, listener })
    }

    /// 主动结束这一轮：释放回调端口，并让正在等的 `complete()` 以错误退出。
    ///
    /// 取消路径必须能走到这里 —— 见 `callback_server::CallbackListener::close`
    /// 那一段（会话被 `Arc` 共享着，只靠 `Drop` 释放端口会变成「等超时才还」）。
    pub fn close(&self) {
        self.listener.close();
    }

    /// 把远程浏览器粘贴的回调 query 投递给正在等待的登录流程。
    pub fn submit_callback(&self, query: &str) -> Result<(), String> {
        self.listener.submit_callback(query)
    }

    /// 等回调并把它换成凭据（**不**含落盘，落盘由调用方做）。
    ///
    /// 回调一次只认一个：监听器已经把「认不出材料的噪音」（favicon /
    /// preconnect）挡在门外，所以这里拿到的一定是授权材料或错误。
    pub async fn complete(&self, proxy: Option<&ResolvedProxy>) -> Result<Credential, GatewayError> {
        let query = self
            .listener
            .next_callback()
            .await
            .ok_or_else(|| GatewayError::with_status(408, "Trae 登录回调已丢失（本轮监听已结束），请重新发起登录"))?;
        let callback = Callback::from_query(&query);
        if !callback.error.is_empty() {
            // 上游明说的失败（用户取消、errorCode=20405 设备绑定被拒、
            // isRedirect=false）要原样透出去：那是「这一轮没成」，不是「网关坏了」。
            return Err(GatewayError::with_status(400, callback.error.clone()));
        }
        if callback.auth_code.is_empty() && callback.refresh_token.is_empty() {
            return Err(GatewayError::with_status(400, "Trae 回调里没有授权码也没有续期串"));
        }
        // 回调带回的 loginHost 优先（它才是这次授权实际发生的站点），
        // 没有就退回发起时 guidance 的那一份。
        let login_host = if callback.login_host.is_empty() { self.login_host.clone() } else { callback.login_host.clone() };
        let (access_token, refresh_token, expires_at, auth_client_id) = if !callback.refresh_token.is_empty() {
            self.exchange_via_refresh_token(&callback, proxy).await
        } else {
            self.exchange_via_auth_code(&callback, &login_host, proxy).await?
        };
        if access_token.is_empty() && refresh_token.is_empty() {
            return Err(GatewayError::with_status(502, "Trae 换证成功但响应里没有任何令牌，请重试"));
        }
        let mut credential = Credential {
            access_token,
            refresh_token,
            expires_at,
            domain: CN_DOMAIN.to_string(),
            api_host: DEFAULT_LOGIN_HOST.to_string(),
            machine_id: self.context.machine_id.clone(),
            device_id: self.context.device_id.clone(),
            variant: self.context.variant.clone(),
            device_public_key: self.context.device_public_pem.clone(),
            device_private_key: self.context.device_private_pem.clone(),
            // 上游在这一次换证里承认的归属，落盘后就不再需要按 variant 猜
            auth_client_id,
            ..Default::default()
        };
        // 身份是**读数**，不是门槛：GetUserInfo 挂了就用回调回显，两个都没有
        // 就落一个每个 variant 固定的 unknown 名（见 `profile.rs` 模块头 ——
        // 用 per-login 的随机 id 兜底会造出重复账号）。
        let authoritative = get_user_info(&credential, proxy).await.unwrap_or_default();
        let identity = Identity::merged(&authoritative, &callback_identity(&query), credential.variant());
        credential.uid = identity.uid;
        credential.nickname = identity.nickname;
        credential.enterprise_id = identity.enterprise_id;
        Ok(credential)
    }

    /// `refreshToken` 分支：直接走续期链换新凭据。
    ///
    /// 续期也失败时**不报错**：把这条串既当 accessToken 又当 refreshToken 存下
    /// （参考实现同一条）。判成失败等于把一次已经花掉用户点击的授权丢掉，
    /// 而存下来最坏也只是第一次转发时 401。
    async fn exchange_via_refresh_token(
        &self,
        callback: &Callback,
        proxy: Option<&ResolvedProxy>,
    ) -> (String, String, i64, String) {
        let seed = Credential {
            refresh_token: callback.refresh_token.clone(),
            api_host: DEFAULT_LOGIN_HOST.to_string(),
            domain: CN_DOMAIN.to_string(),
            machine_id: self.context.machine_id.clone(),
            device_id: self.context.device_id.clone(),
            variant: self.context.variant.clone(),
            ..Default::default()
        };
        match refresh_once(&seed, &refresh_candidates(&seed.api_host), proxy).await {
            // 第四个位置是上游承认的 ClientID：换证成功时 `refresh_once` 已经把它
            // 归并进新凭据，这里原样带出去落盘（丢了它，下一次续期又要猜）
            Ok(next) => (next.access_token, next.refresh_token, next.expires_at, next.auth_client_id),
            Err(_) => (seed.refresh_token.clone(), seed.refresh_token.clone(), 0, String::new()),
        }
    }

    /// `authCode` 分支：授权码换证。
    async fn exchange_via_auth_code(
        &self,
        callback: &Callback,
        login_host: &str,
        proxy: Option<&ResolvedProxy>,
    ) -> Result<(String, String, i64, String), GatewayError> {
        let exchanged = exchange_auth_code(&self.context, &callback.auth_code, login_host, proxy).await?;
        // 只有 refreshToken 回来时用它当 accessToken（参考实现 `if accessToken == "" { = refreshToken }`）。
        let access_token = if exchanged.access_token.is_empty() { exchanged.refresh_token.clone() } else { exchanged.access_token };
        Ok((access_token, exchanged.refresh_token, exchanged.expires_at_ms, response_client_id(&exchanged.raw)))
    }
}
