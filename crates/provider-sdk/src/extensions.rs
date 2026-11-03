//! 不改变协议版本的可选扩展；旧宿主与旧插件可继续使用原有方法。

use std::future::Future;

pub const ERROR_POLICY_METHOD: &str = "error.policy";

tokio::task_local! {
    static DIRECT_PROXY_OVERRIDE: bool;
}

pub fn direct_proxy_override() -> bool {
    DIRECT_PROXY_OVERRIDE
        .try_with(|direct| *direct)
        .unwrap_or(false)
}

pub(crate) async fn with_direct_proxy_override<F: Future>(direct: bool, work: F) -> F::Output {
    DIRECT_PROXY_OVERRIDE.scope(direct, work).await
}
