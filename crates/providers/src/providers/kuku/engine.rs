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
//! ── 与网关那份的差别（迁移时改的）───────────────────────────
//! 网关版先 `LoadLibraryW` 加载官方客户端的 `genflowengine.dll`、调它导出的
//! `genflow_engine_get_login_auth_param` 构造签名参数体。插件这边：
//!
//!   · `windows` crate 只开了 `Win32_Security_Cryptography`（那是 DPAPI 用的），
//!     加载 DLL 要 `Win32_System_LibraryLoader` + `Win32_System_Registry`；
//!   · 更根本的是**插件不该去翻用户的注册表、不该把 14MB 的原生引擎映射进
//!     宿主进程** —— 那两件事都是「进程级副作用」，与插件「一次请求一个
//!     spawn 的沙箱」相反。
//!
//! 所以这里**只保留换发的 HTTP 部分**，参数体退化成「最简形态」（只有
//! `appid` / `bduss` / `ptoken` / `return_type` / `tpl`，没有 `sig`）。
//! 上游对缺 `sig` 的响应实测是 `errno=-6`（拒绝），**不假装成功** ——
//! 调用方按 `Err` 处理，回退到「用原 STOKEN 继续」（与网关那份「换发失败
//! 只记日志、不阻断」的取向一致）。
//!
//! ── 这条降级的实际影响 ──────────────────────────────────────
//! 只影响「网页登录 / 手粘 Cookie 的账号」，且只影响**业务接口**（对话、
//! 目录、签到）；余额、桌面端导入的账号（客户端 Cookies 里带的就是业务
//! STOKEN）都不受影响。要让这条链在插件里恢复完整能力，需要给插件开
//! `Win32_System_LibraryLoader` + `Win32_System_Registry` 两个 feature
//! 并接受「映射原生 DLL」—— 那是一个需要单独决策的事。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::Value;

use crate::errors::GatewayError;
use crate::http::{control_client, describe_chain};

use super::USER_AGENT;

/// 换发端点（passport 的 v3 登录 API）
const PASSPORT_AUTH_URL: &str = "https://passport.baidu.com/v3/login/api/auth";

/// 用 BDUSS + PTOKEN 换发「genflowpro 作用域」的 STOKEN。
///
/// 成功返回新 STOKEN；失败（缺材料 / 没有引擎 / 被拒）返回带可读原因的错误，
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
    // ── 参数体：没有 sig（见模块头）────────────────────────────
    let body = format!(
        "appid=1&bduss={}&ptoken={}&return_type=1&tpl=genflowpro&tpl_list=genflowpro",
        encode(bduss),
        encode(ptoken)
    );
    // ── 为什么不用 `http::send_raw` ──────────────────────────────
    // 那个函数的 body 是 `Option<&Value>`，会按 JSON 发；passport 只认
    // `application/x-www-form-urlencoded`（发错编码的表现是 400
    // `invalid_request`，与「凭证不对」难以区分）。这里直发一次。
    let response = control_client(None, "direct")
        .post(PASSPORT_AUTH_URL)
        .header("Content-Type", "application/x-www-form-urlencoded;charset=UTF-8")
        .header("Referer", format!("{}/genflowpro", super::BASE_URL))
        .header("Origin", super::BASE_URL)
        .header("User-Agent", USER_AGENT)
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

/// 最小百分号编码（只编表单里有特殊含义的字符）
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.as_bytes() {
        match byte {
            b'&' | b'=' | b'+' | b'%' | b'#' | b' ' => {
                out.push('%');
                out.push_str(&format!("{byte:02X}"));
            }
            0x00..=0x1f | 0x7f => {
                out.push('%');
                out.push_str(&format!("{byte:02X}"));
            }
            _ => out.push(*byte as char),
        }
    }
    out
}
