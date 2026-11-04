//! KukuAI 的「业务会话令牌」换发：BDUSS + PTOKEN → genflowpro 作用域的 STOKEN。
//!
//! ── 为什么需要换发 ──────────────────────────────────────────
//! KukuAI 的业务接口（`userreport` / `sendmsg` / `getchatcontent`）认的是
//! **按产品签发**的 STOKEN，而网页登录 Cookie 里那份是**通行证级**的 ——
//! 无论带多少 Cookie 都过不了校验（`errno=-6`）。症状很有欺骗性：余额接口
//! 只看 BDUSS，所以「余额正常、模型拉不下」。
//!
//! 换发材料是 `PTOKEN`（`credentials::ptoken_of`），端点：
//! `POST https://passport.baidu.com/v3/login/api/auth`
//!
//! ── 签名：纯算法，不加载 DLL ────────────────────────────────
//! 官方客户端用 `genflowengine.dll` 的 `genflow_engine_get_login_auth_param`
//! 构造参数体，但它本质只是「六个字段拼串 + 内置密钥 + 一次 MD5」：
//!
//!   sig  = md5("appid=1&bduss=<BDUSS>&ptoken=<PTOKEN>&return_type=1&\
//!              tpl=genflowpro&tpl_list=genflowpro|netdisk&sign_key=<KEY>")
//!   body = "appid=1&bduss=<BDUSS>&ptoken=<PTOKEN>&return_type=1&\
//!           tpl=genflowpro&tpl_list=genflowpro|netdisk&sig=<sig>"
//!
//! 密钥与公式由 `IKUNHEIHIE/kuku2api`（0BSD）用真实 DLL 对两组合成输入
//! 验证过、再以真实抓包复算过，见 `build_auth_param`。所以这里**不需要**
//! `Win32_System_LibraryLoader` / `Win32_System_Registry`，也不用把 14MB 的
//! 原生引擎映射进宿主进程 —— 与插件「一次请求一个 spawn 的沙箱」一致，
//! Windows / Linux 都能换发。
//!
//! 缺 `sig` / 错 `sig` 上游回 `errno=110003`（另一种表现是 `-6`），
//! **不假装成功** —— 调用方按 `Err` 处理，回退到「用原 STOKEN 继续」
//! （与网关那份「换发失败只记日志、不阻断」的取向一致）。
//!
//! ── 影响面 ──────────────────────────────────────────────────
//! 只影响「网页登录 / 手粘 Cookie」的账号（其 Cookie 里那份是通行证级
//! STOKEN）；桌面端导入的账号 Cookies 里带的就是业务级 STOKEN，本不需要
//! 换发。`ENGINE_SIGN_KEY` 是编译进官方客户端的常量，上游若更换需同步更新。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use md5::{Digest as _, Md5};
use serde_json::Value;

use crate::errors::GatewayError;
use crate::http::{control_client, describe_chain};

/// 换发端点（passport 的 v3 登录 API）
const PASSPORT_AUTH_URL: &str = "https://passport.baidu.com/v3/login/api/auth";

/// 官方引擎内置的签名密钥，编译进 `genflowengine.dll`（kuku2api 用真实 DLL
/// 与真实抓包双重验证）。上游更换时这里要跟着更新。
const ENGINE_SIGN_KEY: &str = "34868b316de55815273b2616954b0286";

/// 换发这一步对「客户端引擎 UA」敏感 —— 浏览器 UA 会被判成非客户端。
/// 取值与抓包到的 `/v3/login/api/auth` 请求逐字一致。
const ENGINE_UA: &str = "genflow;1.6.5;PC;PC-Windows;10.0.26200;GenFlowPro";

/// 构造换发用的签名参数体（见模块头）。
///
/// 签名输入与发送体必须是**同一份、未做百分号编码**的字段串：服务端按收到的
/// `tpl_list` / `sign_key` 等字段逐字复算 MD5，两者不一致就回 `errno=110003`。
/// BDUSS 是 base64 风格、PTOKEN 是十六进制，都不含需要转义的字符，故与官方
/// 客户端一样直接拼串（`|` 在值里也不需要转义）。
fn build_auth_param(bduss: &str, ptoken: &str) -> String {
    let signed = format!(
        "appid=1&bduss={bduss}&ptoken={ptoken}&return_type=1&tpl=genflowpro&\
         tpl_list=genflowpro|netdisk&sign_key={ENGINE_SIGN_KEY}"
    );
    let mut hasher = Md5::new();
    hasher.update(signed.as_bytes());
    let signature = format!("{:x}", hasher.finalize());
    format!(
        "appid=1&bduss={bduss}&ptoken={ptoken}&return_type=1&tpl=genflowpro&\
         tpl_list=genflowpro|netdisk&sig={signature}"
    )
}

/// 用 BDUSS + PTOKEN 换发「genflowpro 作用域」的 STOKEN。
///
/// 成功返回新 STOKEN；失败（缺材料 / 被拒 / 网络异常）返回带可读原因的错误，
/// 由调用方决定回退方式（本项目的口径是「只记日志，用原凭证继续」）。
pub async fn exchange_genflowpro_stoken(
    bduss: &str,
    ptoken: &str,
) -> Result<String, GatewayError> {
    if bduss.trim().is_empty() || ptoken.trim().is_empty() {
        return Err(GatewayError::with_status(
            400,
            "换发 KukuAI 会话令牌需要 BDUSS 与 PTOKEN（登录 Cookie 里缺 PTOKEN）",
        ));
    }
    // ── 参数体：六字段 + 内置密钥算出的 MD5 签名（见模块头）──────
    let body = build_auth_param(bduss, ptoken);
    // ── 为什么不用 `http::send_raw` ──────────────────────────────
    // 那个函数的 body 是 `Option<&Value>`，会按 JSON 发；passport 只认
    // `application/x-www-form-urlencoded`（发错编码的表现是 400
    // `invalid_request`，与「凭证不对」难以区分）。这里直发一次。
    let response = control_client(None, "direct")
        .post(PASSPORT_AUTH_URL)
        .header("Content-Type", "application/x-www-form-urlencoded;charset=UTF-8")
        .header("Referer", format!("{}/genflowpro", super::BASE_URL))
        .header("Origin", super::BASE_URL)
        .header("User-Agent", ENGINE_UA)
        .body(body)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|error| {
            GatewayError::with_status(
                502,
                format!("换发 KukuAI 会话令牌失败：{}", describe_chain(&error)),
            )
        })?;
    let status = response.status().as_u16();
    let value: Value = response.json().await.map_err(|error| {
        GatewayError::with_status(502, format!("换发 KukuAI 会话令牌响应异常：{error}"))
    })?;
    if !(200..300).contains(&status) {
        return Err(GatewayError::with_status(
            502,
            format!("换发 KukuAI 会话令牌失败（HTTP {status}）"),
        ));
    }
    let errno = value.get("errno").and_then(Value::as_i64).unwrap_or(-1);
    if errno != 0 {
        let message = value
            .get("errmsg")
            .or_else(|| value.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("未知错误");
        return Err(GatewayError::with_status(
            502,
            format!("换发 KukuAI 会话令牌被拒（errno={errno}：{message}）"),
        ));
    }
    let stoken = value
        .pointer("/stoken_list/genflowpro")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if stoken.is_empty() {
        return Err(GatewayError::with_status(
            502,
            "换发 KukuAI 会话令牌成功但响应缺少 genflowpro 令牌",
        ));
    }
    Ok(stoken)
}
