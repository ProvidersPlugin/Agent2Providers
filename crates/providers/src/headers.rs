//! 入站请求头：插件侧的 `HeaderMap` 等价物。
//!
//! ── 为什么需要这一层 ────────────────────────────────────────
//! 从网关搬来的 `build_chat_request(account, body, client_headers: &HeaderMap)`
//! 里有十几处 `client_headers.get("x-session-id").and_then(|v| v.to_str().ok())`。
//! 插件拿到的是协议里的 `BTreeMap<String, String>`（见 `ChatRequest.headers`）。
//! 直接改成 `map.get("...")` 会让每处都多一层 `Option<&String>` 的类型摩擦，
//! 而且键的大小写语义会变（`HeaderMap` 不区分大小写，`BTreeMap` 区分）。
//!
//! 所以这里做一个**只读、大小写不敏感**的薄包装：搬来的代码把 `HeaderMap`
//! 换成 [`Headers`] 即可，`.get(name)` 的返回类型也做成 `Option<&str>`。
//!
//! ── 为什么不在插件里直接用 `http` crate 的 HeaderMap ──────────
//! 那要给插件加一个依赖，而它解决的问题（多值头、非 UTF-8 字节）在这里都不
//! 存在：协议只传字符串对，多值头在过线前已经被网关合并。少一个依赖，插件
//! 作者的依赖树就少一条要跟版本的链。

use std::collections::BTreeMap;

/// 只读的入站请求头集合（大小写不敏感的查找）。
#[derive(Clone, Debug, Default)]
pub struct Headers {
    inner: BTreeMap<String, String>,
}

impl Headers {
    /// 空集合。
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self::default()
    }

    /// 从协议里的头 map 构造（键统一转小写，与网关侧的 `headers_json` 同口径）。
    pub fn from_map(map: &BTreeMap<String, String>) -> Self {
        Self {
            inner: map
                .iter()
                .map(|(key, value)| (key.to_ascii_lowercase(), value.clone()))
                .collect(),
        }
    }

    /// 取一个头（名字大小写不敏感）。
    #[allow(dead_code)]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.inner
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// 遍历全部头。
    #[allow(dead_code)]
    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.inner.iter()
    }

    /// 是否为空。
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}
