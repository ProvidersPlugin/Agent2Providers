//! Qoder / 千问办公的**站点**定义；站点不接受任意 URL。
//!
//! ── 站点即 provider 身份（2026-10 拆家 → 2026-11 扩到千问办公）────────
//! 与 AutoClaw / Accio / ZCode / ModelScope 同款：一个站点是**一家 provider**。
//! 现在有四个站点，两两成对、相邻排列：
//!
//! | 站点 | provider id | 产品 | 版本 |
//! |---|---|---|---|
//! | [`Site::QoderCn`] | `qoder` | Qoder | 中国版 |
//! | [`Site::QoderIntl`] | `qoder-intl` | Qoder | 国际版 |
//! | [`Site::QwenworkCn`] | `qwenwork` | 千问办公 | 国内版 |
//! | [`Site::QwenworkIntl`] | `qwenwork-intl` | 千问办公 | 国际版 |
//!
//! 站点 → 身份的互查只在本文件（`provider_id` / `from_provider_id`），
//! **别处不要再写 `"qwenwork-intl"` 这类字面量**。存量身份只增不改：
//! `qoder`（中国版）当年不带后缀，`qwenwork`（国内版）沿用网关停用清单里的
//! 那一个 id，存量账号因此原地复活而不是变成另一个家。
//!
//! ── 千问办公和 Qoder 是什么关系（为什么共用一个模块）────────────────
//! **同一套协议栈**：同一个聊天路径
//! （`/algo/api/v2/service/pro/sse/agent_chat_generation`）、同一套
//! `Bearer COSY.<payload>.<sig>` 信封、同一把服务端 RSA 公钥、同一个
//! `client_id`（国内版与 Qoder CN 逐字相同）。差异只有一批**客户端身份常量**
//! 与账号接口路径。因此实现只有一份、按本站点取值（`Site` 的那些方法），
//! 而不是复制一份 7000 行。
//!
//! ── 常量各自的依据（写清来源，改动前先复核）──────────────────────
//! Qoder 一侧来自现有的 `Qoder-Proxy` 移植（已在跑）；千问办公一侧来自
//! **三方实测证据**，按可信度排：
//!   1. `dumate2api` 的 `src/qwenwork/`（2026-10-14 直连 gateway.qwenwork.cn
//!      签名成功、19 帧正常出流；2026-10-31 复核模型目录）—— 提供
//!      `cosyVersion 1.1.52` / `clientType 6` / `data_policy agree` /
//!      `business qoder_work` / `scene qwork` / `session_type qoder_work` /
//!      `user-agent qoderwork/1.1.0` / 账号上下文与钱包路径；
//!   2. `magpie-plugins` 的 `packages/qoder`（0.2.37，对着 QwenWorkCN.app
//!      1.2.1 写）—— 提供 `client_id` / `redirect_uri` / `walletsPath` /
//!      `modelScene=listAllModels`；
//!   3. 本仓库归档的 Bun 参考（分支 `legacy-bun-reference`，
//!      `runtime/vendor/qoder/qwenwork-*.mjs`）—— 同一份协议的本项目实现，
//!      逐字段与上面两份互相印证。
//!
//! **国际版（`gateway.qwenwork.ai`）的差异项只有三条**（主机、`client_id`、
//! `redirect_uri`），来源是 `EchoBird` 的 `qwenwork_accounts`（它的两个
//! edition 常量：`gateway.qwenwork.ai` / `cc65e5fc-…` / `qwenwork://`）。
//! 其余常量按「同一产品的另一个部署」沿用国内版 —— 这是**推断**，不是实测；
//! 如果国际版对话失败，先怀疑这里，再按上面三份参考重新取证。
//!
//! ── 2026-11-03 复核：四个站点都走设备授权（务必读完再改）──────────
//! 上面三份参考（magpie / dumate / ChinaCPA / EchoBird / local-router / xrl）
//! 用的都是**设备授权**：浏览器打开 `gateway.qwenwork.cn/device/selectAccounts`，
//! 授权后自己轮询 `/api/v1/deviceToken/poll`。
//!
//! 曾经有一版把它判成「入口已停用」并改走授权码 + 本机回调 —— 那个结论是错的：
//! 当时本模块把 `nonce` 的 UUID 横线去掉了，上游按 UUID 解析它，于是整条 query
//! 被判成非法（`400 INVALID_DEVICE_FLOW`、`details.field=query`、
//! `reason=not_allowed`）。补回横线之后四个站点的入口全部正常（302 进登录页），
//! 轮询对未完成的流程回 404（= 等待中）。
//!
//! 授权码那条路对本项目不可用：授权页只认客户端自己登记的
//! `https://gateway.qwenwork.{cn,ai}/oauth/callback`，本机回环与深链一律
//! `redirect_uri does not match`，授权码回不到我们手里。所以这里**不要**再
//! 接回授权码 / 本机回调。

use serde_json::Value;

use crate::errors::GatewayError;

/// 本家站点（Qoder 两地 + 千问办公两版）。
///
/// `Copy` 且无堆分配：它随凭证快照一起传遍转发链路，调用点全是值传递。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Site {
    /// Qoder 中国版（`qoder`）
    QoderCn,
    /// Qoder 国际版（`qoder-intl`）
    QoderIntl,
    /// 千问办公国内版（`qwenwork`，`gateway.qwenwork.cn`）
    QwenworkCn,
    /// 千问办公国际版（`qwenwork-intl`，`gateway.qwenwork.ai`）
    QwenworkIntl,
}

impl Site {
    /// 四个站点（注册表顺序：中国版 / 国内版在各自成对里靠前 —— 存量账号都归它们）。
    pub const ALL: [Site; 4] = [
        Site::QoderCn,
        Site::QoderIntl,
        Site::QwenworkCn,
        Site::QwenworkIntl,
    ];

    /// 本家是不是千问办公（协议同源，但客户端身份常量与账号接口不同）。
    pub fn is_qwenwork(self) -> bool {
        matches!(self, Site::QwenworkCn | Site::QwenworkIntl)
    }

    /// 本家是不是 Qoder（决定走 Qoder 的账号接口与活动领取）。
    pub fn is_qoder(self) -> bool {
        !self.is_qwenwork()
    }

    // ── 身份 ────────────────────────────────────────────────────

    /// 本家站点的 provider id（**落盘契约**，存量 id 只增不改）。
    pub fn provider_id(self) -> &'static str {
        match self {
            Site::QoderCn => "qoder",
            Site::QoderIntl => "qoder-intl",
            Site::QwenworkCn => "qwenwork",
            Site::QwenworkIntl => "qwenwork-intl",
        }
    }

    /// provider id → 站点（不认识的 id 返回 None）。
    ///
    /// 插件里目前没有调用方：转发链路的站点直接来自构造时的 provider 身份
    /// （见 `refresh::snapshot`）。留着它是因为它是「id ↔ 站点」的唯一反向
    /// 映射，将来做跨站点的账号迁移 / 排障时要靠它，而不是在别处再写一份
    /// 字面量清单。
    #[allow(dead_code)]
    pub fn from_provider_id(provider_id: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|site| site.provider_id() == provider_id)
    }

    /// 产品名（账号页分组、日志前缀用）。
    pub fn product(self) -> &'static str {
        match self {
            Site::QoderCn | Site::QoderIntl => "Qoder",
            Site::QwenworkCn | Site::QwenworkIntl => "千问办公",
        }
    }

    /// 版本名（中国版 / 国际版 / 国内版）。
    ///
    /// 千问办公的两个版本沿用本项目「国内版 / 国际版」的既有措辞；
    /// Qoder 保持「中国版 / 国际版」不动（那是已发布的叫法）。
    pub fn label(self) -> &'static str {
        match self {
            Site::QoderCn => "中国版",
            Site::QoderIntl => "国际版",
            Site::QwenworkCn => "国内版",
            Site::QwenworkIntl => "国际版",
        }
    }

    /// 展示名（`Qoder 中国版` / `千问办公 国内版`）。
    pub fn display_name(self) -> String {
        format!("{} {}", self.product(), self.label())
    }

    /// 凭证记录里的 `mode` 字段（兼容旧记录的落盘形态）。
    ///
    /// **它不是权威**：站点一律以 provider 身份为准（见 `refresh::snapshot`），
    /// 这个字段只在「同一条记录被两家站点读到」时提供一个人可读的地区标注。
    pub fn id(self) -> &'static str {
        match self {
            Site::QoderCn | Site::QwenworkCn => "cn",
            Site::QoderIntl | Site::QwenworkIntl => "global",
        }
    }

    /// 解析 payload 里的 `mode` / `edition` 兼容字段。
    ///
    /// 只在**不知道 provider 身份**的入口用（例如手工粘贴一份凭证）；
    /// 拿着 provider 身份的入口必须用 `from_provider_id`。
    pub fn parse(value: &str) -> Result<Self, GatewayError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "global" | "intl" => Ok(Site::QoderIntl),
            "cn" => Ok(Site::QoderCn),
            _ => Err(GatewayError::with_status(
                400,
                "站点必须是 global（国际版）或 cn（中国版）",
            )),
        }
    }

    // ── 主机 ────────────────────────────────────────────────────

    /// 网页门户基址（人看的页面：登录授权页所在的那台主机）。
    pub fn web_origin(self) -> &'static str {
        match self {
            Site::QoderCn => "https://qoder.com.cn",
            Site::QoderIntl => "https://qoder.com",
            Site::QwenworkCn => "https://gateway.qwenwork.cn",
            Site::QwenworkIntl => "https://gateway.qwenwork.ai",
        }
    }

    /// 账号接口基址（`openapi`：设备授权轮询、续期、用户资料、额度）。
    ///
    /// 千问办公把网页、账号接口与推理网关**都放在 gateway 这一台主机上**，
    /// 所以它三项取同一个值。
    pub fn open_api(self) -> &'static str {
        match self {
            Site::QoderCn => "https://openapi.qoder.com.cn",
            Site::QoderIntl => "https://openapi.qoder.sh",
            Site::QwenworkCn => "https://gateway.qwenwork.cn",
            Site::QwenworkIntl => "https://gateway.qwenwork.ai",
        }
    }

    /// 推理 / 模型目录基址（`/algo/...` 这一族的根，含尾斜杠）。
    pub fn gateway(self) -> &'static str {
        match self {
            Site::QoderCn => "https://gateway.qoder.com.cn/",
            Site::QoderIntl => "https://api3.qoder.sh/",
            Site::QwenworkCn => "https://gateway.qwenwork.cn/",
            Site::QwenworkIntl => "https://gateway.qwenwork.ai/",
        }
    }

    /// **对话链路**实际使用的推理基址。
    ///
    /// 只有 Qoder 国际版按令牌前缀分流（作业令牌 `jt-` 必须打 `api2`，
    /// 打 `api3` 会被判 Login expired）；其余三站都是一台网关。
    pub fn inference_base(self, access_token: &str) -> &'static str {
        match self {
            Site::QoderIntl => {
                if access_token.starts_with("jt-") {
                    "https://api2.qoder.sh/"
                } else {
                    "https://api3.qoder.sh/"
                }
            }
            other => other.gateway(),
        }
    }

    /// 设备授权页的完整地址。
    ///
    /// Qoder 在**门户**上（`qoder.com/device/selectAccounts`）；千问办公在
    /// **gateway** 上（它会 302 到 `qwenwork.cn` 的标准 OAuth 页）。
    pub fn device_login_url(self) -> String {
        format!("{}{}", self.web_origin(), DEVICE_LOGIN_PATH)
    }

    /// 设备流授权的 `client_id`。
    ///
    /// `None` = 历史上不带它也能登录（Qoder 两个站点就是这种，改它会动到
    /// 已发布的登录链路），因此保持原样；千问办公必须带。
    pub fn device_client_id(self) -> Option<&'static str> {
        match self {
            Site::QoderCn | Site::QoderIntl => None,
            // 桌面客户端的；与 Qoder CN 的 CLI 生产 client_id 逐字相同
            Site::QwenworkCn => Some("e883ade2-e6e3-4d6d-adf7-f92ceff5fdcb"),
            // 国际版桌面客户端的（EchoBird `Edition::QwenWork`）
            Site::QwenworkIntl => Some("cc65e5fc-05bd-4f5d-a4e8-0df19aa3d75a"),
        }
    }

    /// 设备流授权的 `redirect_uri`（`None` = 不发这个参数）。
    pub fn device_redirect(self) -> Option<&'static str> {
        match self {
            Site::QoderCn | Site::QoderIntl => None,
            Site::QwenworkCn => Some("qwenwork-cn://"),
            Site::QwenworkIntl => Some("qwenwork://"),
        }
    }

    /// 授权页允许的域名（网关据此拦截把用户引去别处的登录地址）。
    pub fn allowed_login_hosts(self) -> Vec<String> {
        match self {
            Site::QoderCn | Site::QoderIntl => vec![
                "qoder.sh".to_string(),
                "qoder.com".to_string(),
                "qoder.com.cn".to_string(),
            ],
            Site::QwenworkCn => vec![
                "qwenwork.cn".to_string(),
                "gateway.qwenwork.cn".to_string(),
            ],
            Site::QwenworkIntl => vec![
                "qwenwork.ai".to_string(),
                "gateway.qwenwork.ai".to_string(),
            ],
        }
    }

    // ── 登录链路（2026-11-03 复核，见模块头）──────────────────────

    /// 设备授权（`/device/selectAccounts` + `/api/v1/deviceToken/poll`）可用吗。
    ///
    /// 四个站点都可用：Qoder 两站历史如此，千问办公两站在 `nonce` 用标准
    /// UUID（带横线）后同样 302 进登录页。曾经误判成「千问办公入口已停用」，
    /// 根因是当时把横线去掉了，见模块头。
    ///
    /// 目前没有调用方（四个站点都走设备授权，不需要分派）；留着它是因为
    /// 「哪家还支持设备授权」是登录链路的唯一事实来源，加站点时要先看它。
    #[allow(dead_code)]
    pub fn supports_device_login(self) -> bool {
        matches!(self, Site::QoderCn | Site::QoderIntl | Site::QwenworkCn | Site::QwenworkIntl)
    }

    // ── 客户端身份（COSY 头 + 请求体必须一致的那一组）──────────────

    /// `Cosy-Version` / payload 里的 `cosyVersion`。
    pub fn cosy_version(self) -> &'static str {
        match self {
            Site::QoderCn | Site::QoderIntl => COSY_VERSION_QODER,
            // dumate2api：1.1.0 桌面端实测 1.1.52
            Site::QwenworkCn | Site::QwenworkIntl => "1.1.52",
        }
    }

    /// `Cosy-ClientType`。
    pub fn cosy_client_type(self) -> &'static str {
        match self {
            Site::QoderCn | Site::QoderIntl => "5",
            Site::QwenworkCn | Site::QwenworkIntl => "6",
        }
    }

    /// `Cosy-Data-Policy`。
    ///
    /// Qoder 是 `disagree`（既有实现，不动）；千问办公的客户端是 `agree`，
    /// 而 dumate2api 只在 `agree` 下打通了签名链路，故照客户端取值。
    pub fn cosy_data_policy(self) -> &'static str {
        match self {
            Site::QoderCn | Site::QoderIntl => "disagree",
            Site::QwenworkCn | Site::QwenworkIntl => "agree",
        }
    }

    /// `Cosy-Scene`（`None` = 不发这个头）。
    pub fn cosy_scene(self) -> Option<&'static str> {
        if self.is_qwenwork() {
            Some("qwork")
        } else {
            None
        }
    }

    /// `Cosy-Business-Product`（`None` = 不发这个头）。
    pub fn cosy_business_product(self) -> Option<&'static str> {
        if self.is_qwenwork() {
            Some("qoder_work")
        } else {
            None
        }
    }

    /// `Cosy-Business-Type`（`None` = 不发这个头）。
    pub fn cosy_business_type(self) -> Option<&'static str> {
        if self.is_qwenwork() {
            Some("agent")
        } else {
            None
        }
    }

    /// 请求体里的 `session_type`。
    ///
    /// **它必须与 `Cosy-ClientType` 一致**：千问办公的服务端会因为
    /// 「body 说 qodercli、头说 6」这种自相矛盾**直接拒绝**（dumate2api
    /// 的 constants 里写着这条实测结论），这也是本模块必须按站点取值、
    /// 不能照抄 Qoder 那套常量的直接原因。
    pub fn session_type(self) -> &'static str {
        match self {
            Site::QoderCn | Site::QoderIntl => "qodercli",
            Site::QwenworkCn | Site::QwenworkIntl => "qoder_work",
        }
    }

    /// 请求体 `business.product`。
    pub fn business_product(self) -> &'static str {
        match self {
            Site::QoderCn | Site::QoderIntl => "cli",
            Site::QwenworkCn | Site::QwenworkIntl => "qoder_work",
        }
    }

    /// 请求体 `business.version`。
    pub fn business_version(self) -> &'static str {
        match self {
            Site::QoderCn | Site::QoderIntl => "1.0.0",
            Site::QwenworkCn | Site::QwenworkIntl => self.cosy_version(),
        }
    }

    /// 账号类请求的 `User-Agent`（`None` = 沿用既有的那个）。
    pub fn user_agent(self) -> Option<&'static str> {
        if self.is_qwenwork() {
            Some("qoderwork/1.1.0")
        } else {
            None
        }
    }

    // ── 账号接口 ────────────────────────────────────────────────

    /// 用户资料路径。
    pub fn user_info_path(self) -> &'static str {
        match self {
            Site::QoderCn | Site::QoderIntl => USER_INFO_PATH,
            Site::QwenworkCn | Site::QwenworkIntl => ACCOUNT_CONTEXT_PATH,
        }
    }

    /// 额度（账号上下文）路径（`None` = 走 Qoder 的 `sash` 用量接口）。
    pub fn usage_path(self) -> Option<&'static str> {
        if self.is_qwenwork() {
            Some(ACCOUNT_CONTEXT_QUOTA_PATH)
        } else {
            None
        }
    }

    /// 积分池路径（`None` = 这家没有 / 不读）。
    ///
    /// 只有千问办公**国内版**的公有云个人账号有这三档钱包；国际版与 Qoder
    /// 都不读（前者未取证，后者根本没有这个接口）。
    pub fn wallets_path(self) -> Option<&'static str> {
        match self {
            Site::QwenworkCn => Some(WALLETS_PATH),
            _ => None,
        }
    }

    /// 续期请求体里的 `target`（`None` = 只发 `refresh_token`）。
    pub fn refresh_target(self) -> Option<&'static str> {
        match self {
            Site::QwenworkCn | Site::QwenworkIntl => Some("c"),
            Site::QoderCn | Site::QoderIntl => None,
        }
    }

    /// 设备令牌本身是不是对话令牌。
    ///
    /// 千问办公**没有**作业令牌交换（`/api/v1/jobToken/exchange` 不存在），
    /// 设备授权拿到的这对令牌就是对话用的那一对，续期也按设备令牌续。
    pub fn device_only(self) -> bool {
        self.is_qwenwork()
    }

    /// 有没有 PAT（个人访问令牌）交换。
    pub fn supports_pat(self) -> bool {
        self.is_qoder()
    }

    /// 有没有可领取的每日活动。
    pub fn supports_checkin(self) -> bool {
        self.is_qoder()
    }

    // ── 模型目录 ────────────────────────────────────────────────

    /// 目录响应里要读的那个场景（`None` = 走 Qoder 的 `app` + `chat` 合并规则）。
    ///
    /// 千问办公的目录是 `{"qwork": [...]}`，且**把整份目录都放在这一个场景里**
    /// （含上游标了 `enable: false` 的条目），所以还要 [`Self::list_all_models`]。
    pub fn model_scene(self) -> Option<&'static str> {
        if self.is_qwenwork() {
            Some("qwork")
        } else {
            None
        }
    }

    /// 目录里那些 `enable: false` 的条目要不要一起列出来。
    pub fn list_all_models(self) -> bool {
        self.is_qwenwork()
    }

    /// `model_config` 里要按站点补的字段（`None` = 用 Qoder 的那份精简形状）。
    ///
    /// 千问办公的客户端会带上 `display_name` / `format` / `is_vl` 等；
    /// dumate2api 逐字段抄了它（那里的注释写明「与客户端一致」才不被拒）。
    pub fn model_config_style(self) -> ModelConfigStyle {
        if self.is_qwenwork() {
            ModelConfigStyle::Qwenwork
        } else {
            ModelConfigStyle::Qoder
        }
    }

    /// 拿不到远程目录时的静态兜底清单。
    pub fn fallback_models(self) -> Vec<Value> {
        match self {
            Site::QwenworkCn | Site::QwenworkIntl => qwenwork_fallback_models(),
            Site::QoderCn | Site::QoderIntl => Vec::new(),
        }
    }

    /// 上下文窗口上限（千问办公客户端声明的值）。
    ///
    /// 用在「目录条目自己没声明 `max_input_tokens`」的兜底上（见
    /// `models::context_window_of`）：那时按客户端声明值取，而不是退回一个
    /// 更小的默认值 —— 后者会让本可以放下的长对话被判成超长。
    pub fn max_input_tokens(self) -> i64 {
        // 千问办公客户端声明的值；dumate2api 二分实测到 ~1.02M（超过即整轮失败，
        // 不是截断），所以这种声明只能如实写，不能往上抬。
        const QWENWORK_MAX_INPUT: i64 = 1_024_000;
        match self {
            Site::QwenworkCn | Site::QwenworkIntl => QWENWORK_MAX_INPUT,
            Site::QoderCn | Site::QoderIntl => 0,
        }
    }
}

/// `model_config` 的两种形状（见 [`Site::model_config_style`]）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModelConfigStyle {
    /// Qoder：`key` + 几个可选位（`slim_model_config`）。
    Qoder,
    /// 千问办公：客户端那一份更宽的字段集。
    Qwenwork,
}

/// 千问办公的静态兜底清单（`标准` / `高级` / `Qwen3.8-Max`）。
///
/// 与 `models.rs` 的条目形状一致（`upstreamKey` 才是上游 key）。上游
/// 2026-10-31 复核时只有这三个（社区项目提到的 `qwork-auto` / `advanced` /
/// `ultimate` 在当前上游不存在）；远程目录拿得到时以远程为准。
fn qwenwork_fallback_models() -> Vec<Value> {
    const ROWS: &[(&str, &str)] = &[
        ("pro", "高级"),
        ("flash", "标准"),
        ("qwen3.8-max-preview", "Qwen3.8-Max"),
    ];
    ROWS.iter()
        .map(|(key, display)| {
            // 上下文/能力位按上游目录的公开值：三者都不是 VL、也不声明思考档，
            // 上下文按客户端声明（1.02M）。
            serde_json::json!({
                "id": display,
                "name": display,
                "upstreamKey": key,
                "enabled": true,
                "visibility": "shown",
                "reasoning": false,
                "supportsReasoning": false,
                "supportsImages": false,
                "supportsToolCall": true,
                "efforts": [],
                "maxInputTokens": 131_072,
                "maxOutputTokens": super::models::MAX_OUTPUT_TOKENS,
                "isDefault": false,
                "kind": "chat",
                "config": {
                    "key": key,
                    "is_reasoning": false,
                    "is_vl": false,
                    "source": "system",
                    // 与上面的 `maxInputTokens` **同一个值**：目录声明多少，
                    // 发往上游的 `model_config` / `parameters.context_length`
                    // 就声明多少。两处不一致会让「界面上写 128K、请求按 1M 发」
                    // 这种矛盾重新出现（上游按声明值硬拒超长请求）。
                    "max_input_tokens": 131_072,
                },
            })
        })
        .collect()
}

/// Qoder 的 COSY 协议版本（既有实现，不动）。
const COSY_VERSION_QODER: &str = "1.1.38";

/// 设备授权页路径。**四站同名同参**，只有主机名不同。
///
/// `nonce` 必须是标准 UUID（带横线），否则上游整条判非法（见模块头）。
pub const DEVICE_LOGIN_PATH: &str = "/device/selectAccounts";
pub const DEVICE_POLL_PATH: &str = "/api/v1/deviceToken/poll";
pub const EXCHANGE_PATH: &str = "/api/v1/jobToken/exchange";
pub const USER_INFO_PATH: &str = "/api/v1/userinfo";
pub const USAGE_PATH: &str = "/api/v2/quota/usage";


/// 千问办公的账号上下文（用户资料 + 计划 + 额度都在这一份里）。
pub const ACCOUNT_CONTEXT_PATH: &str =
    "/api/v1/adapter/user/account-context?include=user,plan,quota,page,data_sharing";
/// 千问办公的额度读取路径（只要 user/plan/quota 三项 —— 与客户端一致）。
pub const ACCOUNT_CONTEXT_QUOTA_PATH: &str =
    "/api/v1/adapter/user/account-context?include=user,plan,quota";
/// 千问办公的三档积分池（每日 / 每月 / 长期）。
pub const WALLETS_PATH: &str = "/api/v1/adapter/user/wallets";

/// 套餐查询（`GET`，返回 `plan_tier_name` 等）。与额度接口同在 openapi 主机上，
/// 用同一套头；余额面板的「套餐 Pro Trial」一行就来自这里。
pub const PLAN_PATH: &str = "/api/v2/user/plan";

/// 活动/签到接口（`sash` 一族）的路径前缀。
///
/// **与 openapi 同主机、但鉴权口径完全不同**：只要一个 `Bearer <dt->` 加
/// `cosy-clienttype: 10`，**不打 COSY 签名**（见 [`sash_headers`]）。
/// 每日签到的领取走 `GET {CAMPAIGNS_PATH}` 找 `actionType == "CLAIM_BENEFIT"`
/// 的活动、再 `POST {CAMPAIGNS_PATH}/{campaignId}/claim` 领取 —— 详情与证据见
/// `checkin.rs` 的模块头。
pub const CAMPAIGNS_PATH: &str = "/sash/api/v1/me/campaigns";
/// 活动列表的查询串：`forceRefresh=true` 让上游跳过缓存给实时的 `claimStatus`。
pub const CAMPAIGNS_QUERY: &str = "?forceRefresh=true";

/// `cosy-clienttype` 的值：桌面端（QoderWork）形态。
///
/// **这个头在门控返回内容**，不是可有可无的装饰：实测同一个账号
/// （中国版，2026-09-27）用 `Cosy-ClientType: 5`（IDE 形态）打活动接口，
/// 上游返回 `showCampaign:false` + 空活动列表；换成 `10` 才给出完整活动包
/// （含可领取的签到活动）。参考实现（CLIProxyAPI 的 qoder2api 插件、
/// CPA 的 qoder 插件）都用 10。
pub const SASH_CLIENT_TYPE: &str = "10";

/// `user-agent`：抓包确认的客户端标识（参考实现同值）
pub const SASH_USER_AGENT: &str = "Qoder";

/// 活动/签到接口的请求头：**最小束、无签名**。
///
/// 小写头名是照抄抓包结果（HTTP 头名大小写不敏感，但保持一致便于对拍）。
pub fn sash_headers(token: &str) -> Vec<(String, String)> {
    vec![
        ("authorization".to_string(), format!("Bearer {token}")),
        ("cosy-clienttype".to_string(), SASH_CLIENT_TYPE.to_string()),
        ("accept".to_string(), "application/json".to_string()),
        ("accept-language".to_string(), "zh-CN".to_string()),
        ("user-agent".to_string(), SASH_USER_AGENT.to_string()),
    ]
}

/// 设备流凭据（`dt-` / `drt-`）的续期端点，挂在 [`Site::open_api`] 上。
///
/// ── 为什么不是 `{center}/algo/api/v3/user/refresh_token` ──────────
/// 那条「认证中心」端点是给 PAT 换来的作业令牌（`jt-` / `jrt-`）用的，且要求
/// `appcode: cosy` + `Date` + `signature = md5("cosy&<secret>&<date>")` 这套签名头
/// （`center.qoder.sh` 与 `gateway.qoder.com.cn` 同名同签）。我们的设备流凭据走
/// 那条路会**连签名都没有**，被 WAF 直接丢掉 —— 实测两个地区都稳定返回
/// `403 {"errorMessage":"Request discarded","errorCode":"Forbidden"}`，与凭证
/// 有效性、地区选择都无关。补上签名头后同一请求会从 403 变成 400 业务校验，
/// 足以证明 403 来自缺签名而不是令牌本身。
///
/// 设备流凭据的续期在 **openapi 主机**上、且**不需要任何签名**（对照实现里
/// 只带 `content-type` / `accept` 两个头就能刷新成功）。千问办公的续期也在这
/// 一条路径上（dumate2api 实测）。
pub const DEVICE_REFRESH_PATH: &str = "/api/v1/deviceToken/refresh";

/// 作业令牌（`jrt-`，PAT 换来的那一族）的续期端点。
///
/// 与 [`DEVICE_REFRESH_PATH`] 同在 openapi 主机上、同样不要签名，只有路径不同。
/// 两者**不可互换**：把 `drt-` 交给作业令牌端点、或把 `jrt-` 交给设备端点，
/// 上游都按「令牌无效」拒绝。千问办公没有作业令牌，这条对它不适用。
pub const JOB_REFRESH_PATH: &str = "/api/v1/jobToken/refresh";

/// 续期请求的头：**不带任何签名，也不带 Authorization**。
///
/// 两条续期路径的鉴权凭据都在请求体里（`{"refresh_token": "…"}`），带旧令牌
/// 没有意义。多带 `Cosy-*` 也不会让 center 那一族接受（那需要的是 appcode 签名），
/// 因此这里只保留一个可辨识的 UA。
pub fn refresh_headers() -> Vec<(String, String)> {
    vec![("User-Agent".to_string(), "qoder-local-proxy".to_string())]
}

/// 续期请求的头（按站点）。
///
/// 千问办公要带客户端自己的 UA、`Login-Version` 与一个请求 id（dumate2api 的
/// 实测形态）；Qoder 沿用既有的单 UA 形态。
pub fn refresh_headers_for(site: Site) -> Vec<(String, String)> {
    match site.user_agent() {
        None => refresh_headers(),
        Some(agent) => vec![
            ("User-Agent".to_string(), agent.to_string()),
            ("Login-Version".to_string(), "v2".to_string()),
            ("X-Request-Id".to_string(), crate::ids::uuid_v4()),
        ],
    }
}

pub fn open_api_headers(token: Option<&str>) -> Vec<(String, String)> {
    let mut headers = vec![
        ("Cosy-Version".to_string(), "1.0.1".to_string()),
        ("Cosy-ClientType".to_string(), "5".to_string()),
        ("User-Agent".to_string(), "qoder-local-proxy".to_string()),
    ];
    if let Some(token) = token {
        headers.push(("Authorization".to_string(), format!("Bearer {token}")));
    }
    headers
}
