//! 身份与包元数据：provider id、能力位、包清单。
//!
//! ── 为什么 `ProviderId` 是字符串而不是枚举 ────────────────────
//! 网关今天的 `ProviderKind` 是 29 个变体的封闭枚举，被引用 370 次 ——
//! 那是「provider 编译期内置」这个前提的产物。插件架构下，装了什么包是
//! **运行时**才知道的，枚举必须消失。
//!
//! 代价要说清楚：失去穷举 match 的编译期保护。补偿手段是
//! `adapter_for` 返回 `Option`（今天返回 `&'static dyn`，永不失败），
//! 调用方必须处理「没装这个 provider」—— 而这恰好也是插件架构要表达的
//! 真实状态：用户可以只装一部分包。
//!
//! `ProviderId` 是 `String` 的 newtype 而不是裸 `String`：它要落盘
//! （`accounts.data.provider`）、要出现在 HTTP 响应里、要被前端当筛选条件，
//! 用 newtype 至少能防「拿一个 label 当 id 传」这类错误。

use serde::{Deserialize, Serialize};

/// 一个 provider 的标识（落盘契约，存量 id 不能改名）。
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderId(pub String);

impl ProviderId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ProviderId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl From<String> for ProviderId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// 一个 provider 的静态能力位（握手时一次性申报，网关缓存）。
///
/// ── 为什么走握手而不是每请求 RPC ────────────────────────────
/// 这些值是**常量**：转发循环每处理一个请求都要问它们（`sse_model_rewrite`
/// 在每一帧上都要判），走 RPC 会把它们变成热路径上的同步往返。握手时
/// 申报一次、网关缓存，热路径零开销。
///
/// ── 每个位的消费方（填错会怎样）─────────────────────────────
/// 与今天 `ProviderAdapter` 的对应方法逐条对齐，见各字段的文档。判不出来
/// 时一律填 `false` —— 宁可漏掉一个优化，也不要凭猜测改字节。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Capabilities {
    /// 有推理转发能力（今天各家都是 true）。
    ///
    /// 消费方：账号选路的队首判定。填错的后果：无转发能力的家排进队首，
    /// 顶栏显示成「当前登录态」。
    #[serde(default = "yes")]
    pub chat: bool,
    /// 上游是不是**多步会话协议**（建会话 → 工具循环 → 完成）。
    ///
    /// 消费方：网关按它分流到 `conversation` 而不是 `chat`。填错的后果：
    /// 有状态家被当无状态，请求发出去但语义全错。
    #[serde(default)]
    pub stateful: bool,
    /// 能主动续期凭证。
    ///
    /// 消费方：后台凭证维护的入场券。填错的后果：声称支持但刷不动 →
    /// 每轮白跑并灌失败日志。
    #[serde(default)]
    pub refresh: bool,
    /// 有远程模型目录可拉。
    ///
    /// 消费方：目录刷新排期。填错的后果：声称支持却拿不到新清单，
    /// 用户以为坏了。
    #[serde(default)]
    pub model_refresh: bool,
    /// 能查余额 / 积分。
    ///
    /// 消费方：账号卡的「积分」按钮、批量查询的目标集合。填错的后果：
    /// 给了入口但必然失败。
    #[serde(default)]
    pub usage: bool,
    /// 有每日签到（或等价动作）。
    ///
    /// 消费方：签到中心的清单、定时任务。填错的后果：一个点了必然报错的
    /// 复选框，比没有更糟。
    #[serde(default)]
    pub checkin: bool,
    /// 有可领取的活动（新手礼 / 每日任务 / 限时套餐）。
    #[serde(default)]
    pub claims: bool,
    /// 支持「拉起官方登录页、由网关换取凭证」的网页登录。
    #[serde(default)]
    pub web_login: bool,
    /// 下发 SSE 帧时要把 `model` 改写成客户端请求的名字。
    ///
    /// 消费方：`upstream::sse` 的逐帧出口。填错的后果：客户端看到的
    /// 模型名与它请求的不一致（小浣熊上游会回内部名）。
    #[serde(default)]
    pub sse_model_rewrite: bool,
    /// 丢掉「整片只有换行」的 content 分片。
    ///
    /// 消费方：同上的逐帧出口（还要与部署开关相与）。填错的后果：
    /// 把模型真写的换行丢了，或让保活节拍污染正文。
    #[serde(default)]
    pub sse_strip_newline_chunks: bool,
    /// 允许「一个账号都没有时用环境变量凭证 / 默认登录态转发」。
    #[serde(default)]
    pub anonymous_default_session: bool,
    /// 未配代理时跟随系统代理。
    #[serde(default)]
    pub system_proxy_when_unset: bool,
    /// 凭证过期时间是否**不走账号维度**（目录接口无鉴权、清单是全局的）。
    ///
    /// 与 [`Self::model_refresh`] 配对：今天 Cline 覆写 `refresh_uses_account`
    /// 为 false。填错的后果：「获取模型」弹窗显示一个选了也一样的下拉。
    #[serde(default = "yes")]
    pub refresh_uses_account: bool,
    /// 这家认「默认模型」这个概念（客户端没指定 `model` 时网关可注入
    /// `config.defaultModel`）。
    ///
    /// ── 为什么必须有这一位（不是可选优化）──────────────────────
    /// 网关的口径是「默认值命中的 provider 里**有家认这个概念**才注入」，
    /// 都不认就按「未指定」处理、让上游用自己的默认模型。默认值是 false
    /// 的家一旦被误注入了别家的模型名，请求会被路由到 A 家、模型名却是 B 家的
    /// —— 那是一次必然 404 或答非所问的请求。
    ///
    /// 今天覆写为 true 的六家：Accio（两地区）、AtomCode、LobsterAI、Loomy、
    /// Raccoon、Qoder（两地区）。它们迁到插件后必须申报这一位。
    #[serde(default)]
    pub supports_default_model: bool,
    /// 有环境变量 / 默认登录态凭证（没有账号记录也能转发）。
    ///
    /// ── 与 [`Self::anonymous_default_session`] 的分工 ────────────
    /// `anonymous_default_session` 说的是「允许走那条旁路」，
    /// 这一位说的是「**此刻**本机真有那份凭证」。前者是恒定能力（填一次），
    /// 后者取决于运行环境（`%APPDATA%` 里有没有登录态、环境变量有没有设）。
    ///
    /// 消费方：`catalog::provider_available`（模型清单要不要广告这家）。
    /// 判错的方向只能是「少报」—— 多报会让 `/v1/models` 广告一家其实没有
    /// 凭证的家，客户端选中它就是稳定 401。
    ///
    /// ── 插件为什么只能申报静态值 ────────────────────────────────
    /// 插件读不到网关进程的环境变量，也读不到网关那边的桌面端登录态路径；
    /// 它能申报的是「我这家的实现里有这条旁路」。真正要「此刻有没有」的
    /// 场合由 [`crate::methods::gateway::TOKEN_ENSURE`] 的失败来回答 ——
    /// 那比一次静态申报更准。所以这一位在插件侧的实际语义是
    /// 「有旁路实现」，网关消费时按「可能可用」处理。
    #[serde(default)]
    pub env_credentials: bool,
}

fn yes() -> bool {
    true
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            chat: true,
            stateful: false,
            refresh: false,
            model_refresh: false,
            usage: false,
            checkin: false,
            claims: false,
            web_login: false,
            sse_model_rewrite: false,
            sse_strip_newline_chunks: false,
            anonymous_default_session: false,
            system_proxy_when_unset: false,
            refresh_uses_account: true,
            supports_default_model: false,
            env_credentials: false,
        }
    }
}

/// 一个 provider 的完整描述（`provider.info` 与握手的返回）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderInfo {
    /// provider id（落盘契约）。
    pub id: ProviderId,
    /// 界面显示名（随语言无关，前端做 i18n 时按 id 查）。
    pub label: String,
    /// 注册表顺序（前端列表排序用；同值按 id 排）。
    #[serde(default)]
    pub order: i32,
    /// 静态能力位。
    #[serde(default)]
    pub capabilities: Capabilities,
    /// 支持的登录方式（界面据此渲染「添加账号」表单）。
    #[serde(default)]
    pub login_methods: Vec<crate::account::LoginMethod>,
    /// 内置的默认模型清单（没拉过远程目录时用它）。
    ///
    /// ── 为什么由插件给而不是网关内置 ────────────────────────────
    /// 这是「上游还没连上时界面显示什么」的兜底，属于 provider 知识。
    /// 今天散在各家 `models.rs` 里，搬家后自然跟着走。
    #[serde(default)]
    pub default_models: Vec<crate::model::ModelRecord>,
    /// 这个 provider 默认要不要出网代理（多数家跟账号级代理走）。
    #[serde(default)]
    pub needs_proxy: bool,
    /// `finish_reason` 的「内容策略拒答」取值集合（握手时一次性取回）。
    ///
    /// ── 为什么放在握手而不是每次问 ──────────────────────────────
    /// 网关在**每一条流式响应**上都要判它（`upstream::refusal` 的门控），
    /// 走 RPC 会把流式路径变成「每帧一次往返」。这是常量，握手取回即可。
    ///
    /// 语义见网关 `ProviderAdapter::stream_refusal_finishes`：命中的处置是
    /// **不罚账号、换家**（内容问题，换谁都会拒）。
    #[serde(default)]
    pub refusal_finishes: Vec<String>,
    /// 正文里的「本账号配额耗尽」标记（握手时一次性取回）。
    ///
    /// 与 [`Self::refusal_finishes`] 是同一道门控的两个判据，但处置**不同**：
    /// 这条命中要**罚账号**（冷却到正文里的窗口重置时刻）。
    #[serde(default)]
    pub refusal_markers: Vec<String>,
}

/// 一个包提供的 provider（包清单里的一项，比 `ProviderInfo` 轻）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackProvider {
    pub id: ProviderId,
    pub label: String,
    #[serde(default)]
    pub order: i32,
    /// 这一项是否默认启用（用户可以单独关掉一家）。
    #[serde(default = "yes")]
    pub default_enabled: bool,
}

/// 包清单（`pack.json`）。
///
/// ── 这个文件是安装期的**唯一**信任依据 ──────────────────────
/// 网关读它决定：这个包声称提供哪些 provider、协议版本对不对、二进制在哪。
/// 它自己不能自证（谁都能写一份），所以真实信任来自签名与 sha256 ——
/// 见网关侧 `plugin_host::install` 的校验顺序。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackManifest {
    /// 包 id（例如 `agent2api-providers`）。
    pub id: String,
    /// 显示名。
    pub name: String,
    /// 包版本（语义化版本）。
    pub version: String,
    /// 这个包实现的协议版本，必须等于 [`crate::PROTOCOL_VERSION`]。
    pub protocol: u32,
    /// 包提供的 provider 列表。
    pub providers: Vec<PackProvider>,
    /// 入口二进制，按目标三元组索引（`{"x86_64-pc-windows-msvc": "bin/providers.exe"}`）。
    pub entry: std::collections::BTreeMap<String, String>,
    /// 图标目录（可选，相对包根）。
    #[serde(default)]
    pub icons: Option<String>,
    /// 包主页（可选，界面上「查看来源」用）。
    #[serde(default)]
    pub homepage: Option<String>,
    /// 发布者签名（可选；缺失 = 未签名包，允许安装但界面标黄）。
    ///
    /// ── 签的是什么（改动要两边一起改）──────────────────────────
    /// 覆盖的是**内容**，不是 zip 字节 —— zip 的压缩方式与时间戳可以变，
    /// 内容才是要保护的东西。载荷由 [`crate::signing_payload`] 定义，签名工具
    /// （`agent2api-providers/scripts/sign.mjs`）与网关（`plugin_host::install`）共用那一份
    /// 拼装规则，两处写岔就是「所有包都验不过」。
    #[serde(default)]
    pub signature: Option<PackSignature>,
}

/// 包清单里的签名块。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackSignature {
    /// 签名算法。当前只认 `ed25519`（写别的值一律拒绝，不静默跳过）。
    pub algorithm: String,
    /// 密钥 id：网关据此在**内置公钥表**里找对应那把（支持轮换）。
    #[serde(rename = "keyId")]
    pub key_id: String,
    /// 签名本体（base64 的 64 字节 ed25519 签名）。
    pub value: String,
    /// 这份签名覆盖的**目标平台**（`x86_64-pc-windows-msvc` 这类）。
    ///
    /// 一个包可能带多个平台的二进制，而签名只覆盖其中一个（见
    /// [`signing_payload`] 的 `binary` 项）。网关只验与自己平台一致的那份 ——
    /// 缺失或对不上都按「没有适用于本平台的签名」处理（允许安装但标未签名）。
    #[serde(default)]
    pub target: Option<String>,
}

/// 验签载荷：**包内容**的规范化摘要，不是 zip 字节。
///
/// ── 为什么不直接签 zip ─────────────────────────────────────
/// zip 里带着时间戳、压缩算法、条目顺序，同一份内容用不同工具打出来字节不同。
/// 签 zip 意味着「只有用我们那个脚本打的包才验得过」，而签名要保护的是**内容**
/// （哪个包、哪个版本、哪几家的实现、哪个二进制）。
///
/// ── 载荷怎么拼（改动必须两边同步）───────────────────────────
/// ```text
/// agent2api-pack-v1\n
/// id=<id>\n
/// version=<version>\n
/// protocol=<protocol>\n
/// target=<target>\n
/// providers=<id>:<order>:<default_enabled>,…\n     ← 按 id 字典序
/// entry=<target>=<path>,…\n                        ← 按 target 字典序
/// binary=<sha256 of the entry binary for the signed target>\n
/// ```
/// 末尾不留空行。`binary` 那一项是**关键**：没有它，签名只覆盖元数据，
/// 攻击者可以把 `bin/` 里的二进制换掉而签名照样成立。
///
/// `target` 也在载荷里：一个包可能带多个平台的二进制，把平台写进被签内容，
/// 「给 Windows 包签的名」就不能被挪去顶替 Linux 包（两边的 `binary` 不同，
/// 但显式写出来比依赖这一点更清楚）。
///
/// `binary_sha256` 由调用方传入（签名工具算它自己的二进制；网关算包里那个）。
/// 多平台包每个平台的二进制不同 —— 所以签名是**按 target 分开的**：
/// 网关只验自己这个 target 的二进制，`signature.target` 记的就是它。
pub fn signing_payload(manifest: &PackManifest, target: &str, binary_sha256: &str) -> String {
    let mut providers: Vec<String> = manifest
        .providers
        .iter()
        .map(|item| {
            format!(
                "{}:{}:{}",
                item.id.0,
                item.order,
                if item.default_enabled { 1 } else { 0 }
            )
        })
        .collect();
    providers.sort();
    let entries: Vec<String> = manifest
        .entry
        .iter()
        .map(|(target, path)| format!("{target}={path}"))
        .collect();
    format!(
        "agent2api-pack-v1\nid={}\nversion={}\nprotocol={}\ntarget={}\nproviders={}\nentry={}\nbinary={}\n",
        manifest.id,
        manifest.version,
        manifest.protocol,
        target,
        providers.join(","),
        entries.join(","),
        binary_sha256,
    )
}
