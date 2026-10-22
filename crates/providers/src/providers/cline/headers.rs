//! Cline 上游请求头的默认值与覆盖合并（设置页「Cline 客户端头」的后端）。
//!
//! ── 这套头是什么 ────────────────────────────────────────────
//! Cline 上游按请求头判定「调用方是不是 Cline 自家产品」：`X-CLIENT-TYPE:
//! cline-sdk` 是**硬门槛**（缺了免费池一律 403，见 `adapter.rs` 模块头），
//! 其余的头（版本号、平台标识）让请求**更贴近官方 CLI 的形态** —— 上游的
//! 行为可能随版本收紧，带上没有代价。默认值来自对官方客户端通信的实测
//! （workbuddy2api / cline-proxy 两个参考实现相互印证），版本号沿用本项目
//! 适配器既有的 3.0.62（较新的实测口径）。
//!
//! ── 覆盖语义（与配置键 `clineUpstreamHeaders` 的分工）────────
//! 默认值是**代码里的一等公民**（上游收紧时跟着版本走），配置里只存用户
//! 改过的键（见 `config::KEY_CLINE_UPSTREAM_HEADERS`）：
//!   - 覆盖值**非空** → 按键覆盖默认值（也可新增默认清单里没有的自定义头）；
//!   - 覆盖值**为空串** → 这个头**不发**（显式删除）—— 于是「回到默认」
//!     （在界面删掉这一行）与「禁用某个头」（值留空）都是一次 PUT 能表达的事。
//!
//! 合并发生在**每次构造上游请求**时（`adapter::build_chat_request`）：
//! 覆盖表逐请求读快照，改完设置下一个请求立即生效，不重启进程。

use std::collections::BTreeMap;

use super::CLIENT_TYPE;

/// 默认请求头（键 → 值）。`Authorization` / `Content-Type` / `Accept` /
/// `X-Task-ID` 不在其中：前两个是协议必需（适配器固定构造），`X-Task-ID`
/// 每请求动态生成（与 body 的 `session_id` 同值），都不该被用户覆盖
/// （这套「固定头」清单同时也是管理接口拒绝覆盖的依据，见
/// `api::cline_headers::FIXED_HEADERS`）。
///
/// `X-CLIENT-TYPE` 引用 `adapter::CLIENT_TYPE`（不是再抄一份字面量）：这个值
/// 是免费池的硬门槛，两处各写一份的话，改一处不会带动另一处。
pub const DEFAULT_HEADERS: &[(&str, &str)] = &[
    ("User-Agent", "Cline/3.0.62"),
    ("HTTP-Referer", "https://cline.bot"),
    ("X-Title", "Cline"),
    ("X-IS-MULTIROOT", "false"),
    ("X-CLIENT-TYPE", CLIENT_TYPE),
    ("X-CLIENT-VERSION", "3.0.62"),
    ("X-PLATFORM", "terminal"),
    ("X-PLATFORM-VERSION", "3.0.62"),
    ("X-CORE-VERSION", "0.0.70"),
];

/// 生效请求头：默认值 ∪ 用户覆盖（非空覆盖 / 空串删除）。
///
/// 输出保序（默认清单的顺序优先，用户新增的自定义头按字典序跟在后面），
/// 调用方拿到的就是「该发哪些头」的最终形态。
///
/// ── 覆盖表从哪来（迁移时改的）───────────────────────────────
/// 网关读进程内的 `config::cline_upstream_overrides()`（设置页写进
/// `config.json` 的 `clineUpstreamHeaders`）。插件没有那份进程配置，改成
/// 由调用方传入 —— 适配器每次构造请求时用 `ctx.settings_get("cline-*",
/// "upstreamHeaders")` 读一次快照（网关侧就是 `plugin.<provider>.<key>`
/// 这条路径，与内置家的配置物理隔离）。
///
/// 传 `None` = 还没读到（或网关没给）：按「没有覆盖」处理，即全部默认值。
/// 这个方向是安全的：用户改过的头在下一个请求就会生效，而不会因为一次
/// 读失败把请求发成畸形。
pub fn effective_headers(overrides: Option<&BTreeMap<String, String>>) -> Vec<(String, String)> {
    static EMPTY: BTreeMap<String, String> = BTreeMap::new();
    merge_headers(overrides.unwrap_or(&EMPTY))
}

/// 把网关给的 JSON 覆盖表转成 `BTreeMap`（非对象 / 非字符串值一律忽略）。
///
/// 校验由网关的 `api::cline_headers` 负责（固定头、头名字符集、头值控制字符
/// 都在那里拦）；插件这一侧只做形状收敛 —— 万一有非法项漏进来，
/// `reqwest` 的 `header()` 会让**每一条**请求构造失败，那比丢弃一个头糟得多。
pub fn overrides_from_json(value: &serde_json::Value) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Some(object) = value.as_object() {
        for (key, value) in object {
            if let Some(text) = value.as_str() {
                out.insert(key.clone(), text.to_string());
            }
        }
    }
    out
}

/// 合并的纯函数形态（[`effective_headers`] 的本体，单测直接喂覆盖表）。
///
/// 键的比对**大小写不敏感**（HTTP 头名本来就不区分大小写）：覆盖表里写
/// `user-agent` 要能覆盖默认的 `User-Agent`，而且**不能**两行都出现。
/// 这与适配器侧的 `upsert_header` 同一口径 —— 两边不一致的话，
/// 这里返回的「生效值」就不是适配器真正发出去的那份（管理接口的
/// `effective` 字段就是这么声称的）。
fn merge_headers(overrides: &BTreeMap<String, String>) -> Vec<(String, String)> {
    // 按头名（大小写不敏感）取覆盖值
    let override_for = |name: &str| -> Option<&String> {
        overrides
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
    };
    let mut headers: Vec<(String, String)> = Vec::with_capacity(DEFAULT_HEADERS.len());
    for (key, default_value) in DEFAULT_HEADERS {
        // 空串覆盖 = 显式不发这个头；其余按键覆盖
        match override_for(key) {
            Some(value) if value.is_empty() => continue,
            Some(value) => headers.push(((*key).to_string(), value.clone())),
            None => headers.push(((*key).to_string(), (*default_value).to_string())),
        }
    }
    // 默认清单（大小写不敏感）之外的自定义头：原样带上（空值同样表示不发）
    for (key, value) in overrides {
        if DEFAULT_HEADERS
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case(key))
            || value.is_empty()
        {
            continue;
        }
        headers.push((key.clone(), value.clone()));
    }
    headers
}
