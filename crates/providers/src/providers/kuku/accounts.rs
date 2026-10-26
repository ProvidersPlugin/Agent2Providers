//! KukuAI 账号面：三条添加路径 + 网页登录收尾。
//!
//! ── 三条路径（对应网关 `api::accounts` 的 KukuAI 分支）───────
//! ```text
//!   Manual{payload}     粘贴 Cookie（Cookie 头 / Cookie 编辑器 JSON / {BDUSS,STOKEN} 对象）
//!   Desktop             实时读本机客户端 Cookies（%APPDATA%\baidugenflowpro\Network\Cookies）
//!   FileBase64{content} 用户手选的 Cookies SQLite（另一台机器 / 备份），读完即删
//!   FileText            单账号 JSON 凭证快照；原生 Cookies 数据库仍走 FileBase64
//! ```
//! 三条路径的收尾一致：解析 → `enrich_identity` 真实校验（userreport 换会话
//! 三件套 + `uk`）→ 落账号。**验证失败是错误**（添加时就要「现在真的能用」，
//! 与网关同一口径）。
//!
//! ── 为什么 Desktop 也要落快照（与网关的「引用」口径不同）───────
//! 协议文档说 `Desktop` 来源应该产出「指向本机某处的引用」，转发时再实时读
//! —— 那是给「客户端持续维护登录态」的家设计的。KukuAI 的客户端 Cookies 文件
//! 在客户端运行时被 Chromium 独占锁着（读失败是常态），而且它的 Cookie 是
//! **明文 SQLite**（不是加密存储），快照与实时读的值在客户端不重新登录时完全
//! 相同。因此这里落快照：`desktop: true` 只用于界面徽章与「删除时别碰本机
//! 文件」的语义，凭证本体仍在记录里（网关那份 `add_kuku_account` 也是这么做的：
//! 来源标记 `"desktop"`，凭证照存）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use std::path::PathBuf;

use base64::Engine as _;
use serde_json::{Map, Value};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;

use crate::errors::GatewayError;

use super::credentials::{self, KukuCredentials, enrich_identity};

/// 账号名长度上限（与账号存储的 `MAX_NAME_LENGTH` 同量级）
const MAX_NAME_LENGTH: usize = 64;

/// 一条 KukuAI 账号的解析结果（三条路径的共同产出）。
struct Parsed {
    credentials: KukuCredentials,
    /// 来源标记（`manual` / `file` / `desktop`），落进 secret 备查
    source: &'static str,
    /// 是不是「本机客户端实时导入」型（界面徽章 + 删除时不动本机文件）
    desktop: bool,
}

/// 添加账号：三条路径的入口。
pub(super) async fn normalize(
    _ctx: &PluginCtx,
    input: proto::NormalizeInput,
) -> Result<proto::NormalizedAccount, String> {
    let parsed = parse_source(input.source)?;
    // 真实校验：userreport 换会话三件套 + uk（与网关同一口径 —— 添加时就要
    // 「现在真的能用」）。失败即拒绝，不落空记录。
    let verified = enrich_identity(&parsed.credentials, input.proxy.as_ref(), true)
        .await
        .map_err(|error| error.message)?;
    let name = input
        .name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| crate::json::truncate_text(value, MAX_NAME_LENGTH));
    Ok(account_of(&verified, parsed.source, parsed.desktop, name))
}

/// 按来源解析出凭证。
fn parse_source(source: proto::NormalizeSource) -> Result<Parsed, String> {
    match source {
        proto::NormalizeSource::Manual { payload } => {
            let credentials = credentials::credentials_from_payload(&payload)?;
            Ok(Parsed {
                credentials,
                source: "manual",
                desktop: false,
            })
        }
        proto::NormalizeSource::Desktop => {
            let credentials = credentials::read_desktop_credentials()?;
            Ok(Parsed {
                credentials,
                source: "desktop",
                desktop: true,
            })
        }
        proto::NormalizeSource::FileBase64 {
            content,
            file_name,
        } => {
            let credentials = from_b64_file(&content, file_name.as_deref())?;
            Ok(Parsed {
                credentials,
                source: "file",
                desktop: false,
            })
        }
        proto::NormalizeSource::FileText { content, file_name } => {
            let payload: Value = serde_json::from_str(&content).map_err(|_| with_file_name(
                "KukuAI 凭证文件不是合法 JSON，请选择 Cookies 数据库或单账号 JSON 文件".to_string(),
                file_name.as_deref(),
            ))?;
            let credential_payload = payload.get("accessToken").unwrap_or(&payload);
            let credentials = credentials::credentials_from_payload(credential_payload)?;
            Ok(Parsed { credentials, source: "file", desktop: false })
        }
    }
}

/// 文件导入：base64 → 临时文件 → 与 `Desktop` 同一条读取链。
///
/// ── 为什么要落临时文件（而不是直接从内存解 SQLite）──────────
/// 读写判定（rusqlite 打开 + 查 `cookies` 表）只有
/// `credentials::read_credentials_from_path` 这一份、按**路径**工作，让文件
/// 导入与本机实时导入复用同一条读取链，口径不会分叉。
/// 临时文件的生命周期只覆盖这一次读取：**无论成败立即删除**（成功时凭证已
/// 读到内存，失败时内容也不该留在盘上）。
fn from_b64_file(encoded: &str, file_name: Option<&str>) -> Result<KukuCredentials, String> {
    let engine = base64::engine::general_purpose::STANDARD;
    let bytes = engine.decode(encoded.trim()).map_err(|error| {
        with_file_name(
            format!("凭证文件不是合法的 base64（{error}）；请重新选择文件后再试"),
            file_name,
        )
    })?;
    let path = temp_cookies_path();
    if let Err(error) = std::fs::write(&path, &bytes) {
        // 写入失败也可能留下半截文件：顺手清掉（清不掉不阻断，只是垃圾）
        let _ = std::fs::remove_file(&path);
        return Err(with_file_name(
            format!(
                "写入临时文件失败（{}）：{error}；请检查系统临时目录是否可写后重试",
                path.display()
            ),
            file_name,
        ));
    }
    let read_result = credentials::read_credentials_from_path(&path);
    // 读后即删：这一行覆盖后面所有分支（含解析失败）
    let _ = std::fs::remove_file(&path);
    read_result.map_err(|error| with_file_name(error.file_import_message(), file_name))
}

/// 失败文案带上原文件名（与网关 `describe_file_import_error` 同款）。
fn with_file_name(message: String, file_name: Option<&str>) -> String {
    match file_name.map(str::trim) {
        Some(name) if !name.is_empty() => format!("{message}（文件：{name}）"),
        _ => message,
    }
}

/// 临时 Cookies 文件路径（系统临时目录 + 进程号 + 毫秒）。
///
/// 只要求并发写不撞名，不承担安全语义（文件读完即删）。
fn temp_cookies_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "a2a-kuku-cookies-{}-{}",
        std::process::id(),
        crate::http::now_ms()
    ))
}

/// 校验后的凭证 → 可落库的账号。
fn account_of(
    credentials: &KukuCredentials,
    source: &str,
    desktop: bool,
    explicit_name: Option<String>,
) -> proto::NormalizedAccount {
    let cookie = credentials.cookie_header();
    let nickname = credentials.nickname.trim().to_string();
    let name = explicit_name
        .or_else(|| {
            if nickname.is_empty() {
                None
            } else {
                Some(nickname.clone())
            }
        })
        .unwrap_or_else(|| {
            let uid = credentials.uid.trim();
            if uid.is_empty() {
                "KukuAI 账号".to_string()
            } else {
                format!("库库 AI {uid}")
            }
        });
    let mut secret = Map::new();
    // `accessToken` 存的是**整段 Cookie 头**（网关那份账号记录的同一口径，
    // 见 `kuku_accounts::add_kuku_account`）：`snapshot_for` 按它反解三件套。
    secret.insert("accessToken".to_string(), Value::String(cookie.clone()));
    secret.insert("uid".to_string(), Value::String(credentials.uid.clone()));
    secret.insert(
        "loginName".to_string(),
        Value::String(nickname.clone()),
    );
    secret.insert("source".to_string(), Value::String(source.to_string()));
    secret.insert(
        "tokenTail".to_string(),
        Value::String(credentials.token_tail()),
    );
    secret.insert("name".to_string(), Value::String(name.clone()));
    let secret = Value::Object(secret);
    let public = account_view(&secret);
    let identity = identity_of(&secret);
    proto::NormalizedAccount {
        public,
        secret,
        identity,
        desktop,
    }
}

/// 账号公开视图（界面账号页读的那几个键）。
pub(super) fn account_view(secret: &Value) -> Value {
    let cookie = crate::store::pick(secret, &["accessToken"]);
    let uid = crate::store::pick(secret, &["uid"]);
    let name = {
        let explicit = crate::store::pick(secret, &["name"]);
        if explicit.is_empty() {
            if uid.is_empty() {
                "KukuAI 账号".to_string()
            } else {
                format!("库库 AI {uid}")
            }
        } else {
            explicit
        }
    };
    serde_json::json!({
        "name": name,
        "uid": uid,
        "loginName": crate::store::pick(secret, &["loginName"]),
        "tokenTail": crate::store::pick(secret, &["tokenTail"]),
        // BDUSS 是 Cookie 登录态：没有 refreshToken，也没有过期时间
        "hasSession": !cookie.is_empty(),
        "canRefresh": false,
    })
}

/// 账号的稳定身份：`kuku-<uid>`；没有 uid 时退回 Cookie 尾
/// （与网关 `add_kuku_account` 的 `user_key` 同一口径，去重结果一致）。
pub(super) fn identity_of(secret: &Value) -> String {
    let uid = crate::store::pick(secret, &["uid"]);
    if !uid.is_empty() {
        return format!("kuku-{uid}");
    }
    let tail = crate::store::pick(secret, &["tokenTail"]);
    if !tail.is_empty() {
        return format!("kuku-{tail}");
    }
    String::new()
}

/// 网页登录收尾：壳侧交回的 Cookie 串 → 校验 → 账号。
///
/// ── 复核失败**不阻断**登录，但**带警告**────────────────────
/// 复核（`enrich_identity`）内部会先换发 genflowpro 作用域 STOKEN
/// （`engine.rs`，需要本机客户端引擎 + Cookie 里的 PTOKEN）。走到警告分支的
/// 情形：没找到客户端引擎 / Cookie 缺 PTOKEN 换不了，且原凭证的 STOKEN 又
/// 过不了业务校验。用户在窗口里刚登录成功，凭证本身是有效的（BDUSS 有效），
/// 因此失败时**仍返回账号**（uid 为空，去重按 cookie 尾兜底）——转发与模型
/// 刷新会在需要时重新拿三件套并再次尝试换发（`session::refresh_triple` 的
/// 自愈路径）；同时把警告放进 `message`，由前端告知补救办法。
pub(super) async fn complete_login(cookie: &str) -> Result<proto::LoginPoll, GatewayError> {
    let (bduss, stoken, extras) = credentials::parse_cookie_parts(cookie);
    if bduss.len() < 8 {
        return Ok(proto::LoginPoll::Failed {
            message: "登录窗口没有捕获到有效的 BDUSS（百度通行证登录态），请重新发起登录"
                .to_string(),
        });
    }
    let parsed = KukuCredentials {
        bduss,
        stoken,
        extras,
        uid: String::new(),
        nickname: String::new(),
    };
    match enrich_identity(&parsed, None, true).await {
        Ok(enriched) => Ok(proto::LoginPoll::Done {
            account: account_of(&enriched, "web", false, None),
        }),
        Err(error) => {
            crate::logging::log(
                "[Login]",
                &format!(
                    "⚠️ KukuAI 登录态已交回但复核未通过（{}）；先按有效凭证落库，\
                     uid 留空待后续刷新补齐（复核包含换发业务会话令牌，\
                     需要本机客户端引擎与 Cookie 里的 PTOKEN）",
                    error.message
                ),
            );
            // 警告走 `Done` 的账号 message：协议里 `Done` 没有 warning 字段，
            // 而 `public.name` 之外的提示位只有账号视图 —— 把警告拼进
            // `public.warning`，前端按既有键名读（与网关 `payload.warning` 同义）。
            let mut account = account_of(&parsed, "web", false, None);
            if let Some(object) = account.public.as_object_mut() {
                object.insert(
                    "warning".to_string(),
                    Value::String(
                        "换发 KukuAI 业务会话令牌失败（未找到库库 AI 客户端引擎，或登录 \
                         Cookie 里缺 PTOKEN）：账号已添加，但模型刷新可能报「未登录」。\
                         请安装 / 修复库库 AI 客户端后重新添加，或重新粘贴完整 Cookie"
                            .to_string(),
                    ),
                );
            }
            Ok(proto::LoginPoll::Done { account })
        }
    }
}
