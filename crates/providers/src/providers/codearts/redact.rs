//! 上游错误体的**凭据脱敏**：上游有时会把被拒的凭据或请求头原文回显在错误体里，
//! 而这份错误体会一路冒到客户端与请求日志。
//!
//! ── 为什么这不是装饰 ────────────────────────────────────────
//! 一处回显就够把一份 AK/SK/STS、一份 refresh token、一把 DPoP 私钥交给所有能看到
//! 日志的人。参考实现把这一条写成硬要求（`executor.go` 的 `redactUpstreamError`），
//! 这里照搬它的四个要点：
//!
//!   1. **同一份秘密要盖三种拼法**：原文、`url.QueryEscape` 形（表单/query 里回显
//!      会变成 `%2B` 之类）、以及 JSON 字符串转义形（引号/反斜杠会被转义）。
//!      少盖一种就等于没盖。
//!   2. **长的先盖**：完整令牌要先于它的任何子串被替换。我们的秘密之间确实可能
//!      互为前缀（公开 JWK 的 `x` 是私钥 JWK 里 `x` 的同名值），长先短后能避免
//!      "盖了长的又漏了短的"之外的另一种错——把长串切成两半各盖一次。
//!   3. **截断会切在秘密中间**：错误体有 8 KiB 上限（见 `chat.rs`），所以最后一截
//!      可能正好是某个秘密的前缀。此时要按**后缀匹配**把那个残段也盖掉。
//!   4. **盖上之后不再比对原文**：替换成 [`REDACTED`] 后再做下一次 `contains`
//!      判断（参考实现是顺序 replace，不去重扫）。
//!
//! 秘密只在本函数内出现，绝不进返回值以外的任何地方（不打印、不计数）。

use super::credentials::{Credential, OAuthContext};

/// 替换标记。与 `core::debug_traffic` 的常量同一拼写，便于日志里肉眼一致。
pub const REDACTED: &str = "[REDACTED]";

/// 按 [`Credential`] 收集要盖的秘密（含 OAuth 上下文里的 PKCE 与 DPoP）。
///
/// **空串必须剔除**：`str::replace("", …)` 会在每个字符之间插一段替换标记
/// （`"abc".replace("", "X")` 得到 `"XaXbXcX"`），一条空秘密就足以把整条错误
/// 消息毁掉。凭据里空字段很常见（DPoP 的公开 JWK 常常缺省），所以这条不是理论风险。
/// 参考实现同样有 `if secret == "" { continue }`。
fn secrets_of(credential: &Credential) -> Vec<String> {
    let mut secrets = vec![
        credential.access_key_id.clone(),
        credential.secret_access_key.clone(),
        credential.security_token.clone(),
        credential.refresh_token.clone(),
    ];
    if let Some(context) = credential.oauth_context.as_ref() {
        secrets.push(context.pkce_pair.code_verifier.clone());
        secrets.push(context.pkce_pair.code_challenge.clone());
        let private = &context.dpop_key_pair.private_key_jwk;
        secrets.push(private.d.clone());
        secrets.push(private.x.clone());
        secrets.push(private.y.clone());
        let public = &context.dpop_key_pair.public_key_jwk;
        secrets.push(public.x.clone());
        secrets.push(public.y.clone());
    }
    secrets.retain(|secret| !secret.is_empty());
    secrets
}

/// 只从 OAuth 上下文能拿到的秘密（令牌端点那条路上没有完整凭据对象）。
pub(crate) fn secret_material(context: &OAuthContext) -> Vec<String> {
    let mut secrets = vec![
        context.pkce_pair.code_verifier.clone(),
        context.pkce_pair.code_challenge.clone(),
    ];
    let private = &context.dpop_key_pair.private_key_jwk;
    secrets.push(private.d.clone());
    secrets.retain(|secret| !secret.is_empty());
    secrets
}

/// 一份秘密的三种拼法（原文 / URL 转义 / JSON 字符串转义）。
fn spellings(secret: &str) -> Vec<String> {
    let mut out = vec![secret.to_string()];
    out.push(url_form_escape(secret));
    // JSON 字符串体会把引号与反斜杠转义；这里只取引号内部那一截
    if let Ok(encoded) = serde_json::to_string(secret) {
        out.push(encoded.trim_matches('"').to_string());
    }
    out.retain(|value| !value.is_empty());
    out.sort();
    out.dedup();
    out
}

/// `url.QueryEscape` 的等价实现（Go 那边用的是它）。
///
/// 与 `oauth::form_encode` 同口径：空格成 `+`，其余非未保留字符按 `%XX` 大写。
/// 不复用那边的函数是为了让本模块不依赖网络模块（脱敏只跟字符串打交道）。
fn url_form_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// 把 `message` 里出现过的所有凭据盖掉。
///
/// `truncated` 表示这段消息来自**被截断**的错误体（8 KiB 上限或读超时）：为真时
/// 额外处理「末尾正好是某个秘密的前缀」这一情形。
pub fn redact(message: &str, credential: &Credential, truncated: bool) -> String {
    redact_values(message, &secrets_of(credential), truncated)
}

/// 同 [`redact`]，但秘密清单由调用方给。
///
/// 存在的理由：**不是所有秘密都在 `Credential` 里**。令牌端点的错误体可能回显我们
/// 发过去的 `refresh_token` / PKCE verifier，而那条请求的组装处只拿得到
/// `OAuthContext` 与表单值，没有完整凭据对象。与其在那儿放弃脱敏（"反正只是 form 值"），
/// 不如把这一层露出来让调用方把手上有的秘密清单给它。
pub fn redact_values(message: &str, secrets: &[String], truncated: bool) -> String {
    let mut secrets: Vec<String> = secrets
        .iter()
        .filter(|secret| !secret.is_empty())
        .flat_map(|secret| spellings(secret))
        .collect();
    // 长的先盖（要点 2）
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.chars().count()));
    let mut message = message.to_string();
    for secret in secrets {
        message = message.replace(&secret, REDACTED);
        if !truncated {
            continue;
        }
        // 截断残段：末尾若正好是某个秘密的前缀，把那一截也盖掉（要点 3）
        let length = message.chars().count();
        for size in (1..secret.chars().count()).rev() {
            let prefix: String = secret.chars().take(size).collect();
            if message.ends_with(&prefix) {
                let kept: String = message.chars().take(length - size).collect();
                message = format!("{kept}{REDACTED}");
                break;
            }
        }
    }
    message
}

#[cfg(any())]
mod tests {
    use super::*;
    use crate::providers::codearts::credentials::{DpopKeyPair, Jwk, OAuthContext, PkcePair};

    fn credential() -> Credential {
        Credential {
            access_key_id: "HSTA55HR6J15RVX026DM".to_string(),
            secret_access_key: "3JusjYK4SS7SodA7cIp7pNBBK2YUmUzPnbOMPo1z".to_string(),
            security_token: "hQpjbi1ub3J0aC0wABCdefGHIjklMNOpqrsTUVwxyz0123456789".to_string(),
            refresh_token: "eyJ0eXAiOiJKV1QiLCJhbGciOiJSUzI1NiJ9.payload.signature".to_string(),
            oauth_context: Some(OAuthContext {
                pkce_pair: PkcePair {
                    code_verifier: "7743bb196b8892a1cd0f5e6d7c8b9a0f1e2d3c4b5a69788796a5b4c3d2e1f00".to_string(),
                    code_challenge: "ZoSPwv6ZLUIIEVzxQY5kRk9dC8dP0qY1uS2nT3mW4vX".to_string(),
                    ..PkcePair::default()
                },
                dpop_key_pair: DpopKeyPair {
                    private_key_jwk: Jwk {
                        kty: "EC".to_string(),
                        crv: "P-256".to_string(),
                        x: "qsB7uEbLJ8B5H0ZzQmYy2VvTnG7wLxKl9pO3rS4dE5U".to_string(),
                        y: "9vF7bipQlxN_uE6jRk8sT1mW2nV3bC4dF5gH6jK7lM".to_string(),
                        d: "JxI7JSC3NCMx1rD2eF3gH4jK5lM6nO7pQ8rS9tU0vW1X".to_string(),
                    },
                    ..DpopKeyPair::default()
                },
            }),
            ..Credential::default()
        }
    }

    #[test]
    fn every_credential_field_is_removed() {
        let credential = credential();
        let message = format!(
            "upstream returned HTTP 403: {{\"ak\":\"{}\",\"sk\":\"{}\",\"sts\":\"{}\",\"rt\":\"{}\",\"verifier\":\"{}\",\"jwk_d\":\"{}\"}}",
            credential.access_key_id,
            credential.secret_access_key,
            credential.security_token,
            credential.refresh_token,
            credential.oauth_context.as_ref().unwrap().pkce_pair.code_verifier,
            credential.oauth_context.as_ref().unwrap().dpop_key_pair.private_key_jwk.d,
        );
        let redacted = redact(&message, &credential, false);
        for secret in secrets_of(&credential) {
            assert!(
                !redacted.contains(&secret),
                "秘密 {secret:?} 仍然出现在脱敏后的文本里：{redacted}"
            );
        }
        assert!(redacted.contains(REDACTED));
        // 无秘密的那部分原样保留（否则排障就没线索了）
        assert!(redacted.contains("upstream returned HTTP 403"));
    }

    /// 表单/query 里回显的形状（空格变 `+`、特殊字符变 `%XX`）也要盖住。
    /// 用一个**确实需要转义**的秘密（含 `+` 与 `/`），否则这条用例什么都没验。
    #[test]
    fn url_escaped_spellings_are_also_removed() {
        let mut credential = credential();
        credential.security_token = "sts+with/special=chars&more".to_string();
        let escaped = url_form_escape(&credential.security_token);
        assert_ne!(escaped, credential.security_token, "这条用例要求转义确实改变了文本");
        let message = format!("body: refresh_token={escaped}&x=1");
        let redacted = redact(&message, &credential, false);
        assert!(!redacted.contains(&escaped), "URL 转义形态没盖住：{redacted}");
        // 原文形态也在同一份文本里被盖掉（上游两种都可能回显）
        let both = format!("raw={} escaped={escaped}", credential.security_token);
        let redacted = redact(&both, &credential, false);
        assert!(!redacted.contains(&credential.security_token) && !redacted.contains(&escaped));
    }

    /// 空字段绝不能变成「空秘密」：`str::replace("", …)` 会在每个字符之间插标记，
    /// 一条空秘密就能把整条错误消息毁掉。凭据里空字段很常见（公开 JWK 常缺省）。
    #[test]
    fn empty_credential_fields_never_act_as_secrets() {
        // 先把这条语言行为钉住 —— 没了它，「过滤空串」看着就像多余的防御
        assert_eq!("XaXbXcX", "abc".replace("", "X"), "空模式会在每个字符边界命中");
        let mut credential = credential();
        // 公开 JWK 缺省（默认为空串）—— 这正是线上常见的形状
        credential.oauth_context.as_mut().unwrap().dpop_key_pair.public_key_jwk = Jwk::default();
        assert!(
            !secrets_of(&credential).iter().any(|secret| secret.is_empty()),
            "空字段不该进秘密清单"
        );
        let message = "upstream returned HTTP 500: plain text with no secrets";
        assert_eq!(message, redact(message, &credential, true), "没有秘密时文本必须原样");
    }

    /// 截断把秘密切成前半个时，残段也要盖掉（要点 3）。
    #[test]
    fn a_truncated_credential_prefix_is_removed_too() {
        let credential = credential();
        let secret = &credential.secret_access_key;
        let head: String = secret.chars().take(12).collect();
        let message = format!("upstream returned HTTP 400: {{\"sk\":\"{head}");
        // 不声明截断：只能盖完整出现的那种（这里盖不掉）
        assert!(redact(&message, &credential, false).contains(&head));
        // 声明截断：残段被盖掉
        let redacted = redact(&message, &credential, true);
        assert!(!redacted.contains(&head), "截断残段没盖掉：{redacted}");
        assert!(redacted.ends_with(REDACTED));
    }

    #[test]
    fn ordinary_text_is_left_alone() {
        let credential = credential();
        let message = "upstream returned HTTP 429: {\"error_code\":\"Gate.429\"}";
        assert_eq!(message, redact(message, &credential, true));
    }

    /// 长的先盖：公开 JWK 的 `x` 与私钥 JWK 的 `x` 可能同值，且 JWK 的 `d` 更长。
    /// 不做「长先短后」时，先用短串替换会把长串切成两半，长串再也匹配不上。
    #[test]
    fn longer_secrets_are_replaced_before_their_substrings() {
        let mut credential = credential();
        let context = credential.oauth_context.as_mut().unwrap();
        // 让短串成为长串的前缀（构造最坏情况）
        context.dpop_key_pair.private_key_jwk.d = "ABCDEFGHIJKLMNOP".to_string();
        context.dpop_key_pair.private_key_jwk.x = "ABCDEFGH".to_string();
        let message = "jwk d=ABCDEFGHIJKLMNOP";
        let redacted = redact(message, &credential, false);
        assert!(!redacted.contains("ABCDEFGHIJKLMNOP"), "长串应当被完整盖掉：{redacted}");
        assert!(!redacted.contains("ABCDEFGH"), "短串也不该残留：{redacted}");
    }
}
