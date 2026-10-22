//! `PluginCtx`：插件回调网关的唯一入口。
//!
//! ── 为什么账号读写要走回调而不是插件自己开库 ──────────────────
//! 账号库在网关侧（单文件 SQLite，参与整库备份 / 恢复、云同步、并发控制）。
//! 插件自己开一份库会让「备份恢复」漏掉插件那份数据 —— 那不可接受。
//! 所以这里是唯一入口，网关保证事务与并发语义。
//!
//! ── 每个方法都是 RPC ────────────────────────────────────────
//! 作者应当把它们当作「可能几百微秒的远程调用」，不要放在逐帧循环里
//! （例如每发一个 SSE chunk 就 `store.get` 一次）。惯例是：一次请求开始时
//! 取一次凭证，之后复用。

use agent2api_provider_protocol as proto;
use serde_json::Value;

/// 账号读写的错误。
///
/// ── 为什么不用 `String` ──────────────────────────────────────
/// 调用方要区分「账号不存在」（正常的业务分支：用户删了账号）与「网关那边
/// 出错了」（要上报）。合并成一个字符串会让前者也走错误日志。
#[derive(Clone, Debug)]
pub enum StoreError {
    /// 账号不存在（或已被删除）。
    NotFound,
    /// 网关拒绝（条件更新时值已变、权限不足等）。
    Rejected(String),
    /// 通信失败（网关重启中、管道断了）。
    Unavailable(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "账号不存在"),
            Self::Rejected(why) => write!(f, "被拒绝：{why}"),
            Self::Unavailable(why) => write!(f, "网关不可用：{why}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// 从网关读到的一条账号。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct StoreAccount {
    /// 账号 id。
    pub id: String,
    /// provider id。
    pub provider: String,
    /// 凭证本体。
    pub secret: Value,
    /// 公开视图。
    pub public: Value,
}

/// 插件侧的回调入口。
///
/// 由 `serve()` 构造并传给每个方法；作者不应自己构造它。
pub struct PluginCtx {
    /// 网关给的运行环境（`init` 时拿到）。
    pub env: proto::HostEnv,
    /// 回调发送端（`serve()` 注入）。
    pub(crate) call: std::sync::Arc<dyn CallGateway>,
}

impl std::fmt::Debug for PluginCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginCtx")
            .field("data_dir", &self.env.data_dir)
            .field("host_version", &self.env.host_version)
            .field("target", &self.env.target)
            .finish()
    }
}

impl PluginCtx {
    /// 读一条账号。`account` 为空串 = 该 provider 的队首账号。
    pub async fn store_get(
        &self,
        provider: &str,
        account: &str,
    ) -> Result<StoreAccount, StoreError> {
        let params = serde_json::json!({ "provider": provider, "account": account });
        let value = self.call.call(proto::methods::plugin::STORE_GET, params).await?;
        serde_json::from_value(value).map_err(|e| StoreError::Unavailable(format!("返回形状不对: {e}")))
    }

    /// 列出一个 provider 的全部账号。
    pub async fn store_list(&self, provider: &str) -> Result<Vec<StoreAccount>, StoreError> {
        let params = serde_json::json!({ "provider": provider });
        let value = self.call.call(proto::methods::plugin::STORE_LIST, params).await?;
        serde_json::from_value(value).map_err(|e| StoreError::Unavailable(format!("返回形状不对: {e}")))
    }

    /// 条件更新凭证（比较再写）。
    ///
    /// `expect` 是「我读到的那份 secret 的指纹」—— 网关只在当前值仍等于它
    /// 时写入。这是防止两个并发刷新互相覆盖的唯一手段（今天各家
    /// `update_*_if_current` 的语义）。
    pub async fn store_update(
        &self,
        account: &str,
        expect: &Value,
        secret: &Value,
    ) -> Result<bool, StoreError> {
        let params = serde_json::json!({
            "account": account,
            "expect": expect,
            "secret": secret,
        });
        let value = self.call.call(proto::methods::plugin::STORE_UPDATE, params).await?;
        Ok(value.get("updated").and_then(Value::as_bool).unwrap_or(false))
    }

    /// 标记账号对某模型限额（网关据此冷却与轮换）。
    pub async fn store_mark_limited(
        &self,
        account: &str,
        model: &str,
        reset_at: Option<i64>,
    ) -> Result<(), StoreError> {
        let params = serde_json::json!({
            "account": account,
            "model": model,
            "reset_at": reset_at,
        });
        self.call.call(proto::methods::plugin::STORE_MARK_LIMITED, params).await?;
        Ok(())
    }

    /// 读一份台账（CodeArts 福利这类按账号的持久状态）。
    pub async fn ledger_get(&self, account: &str, kind: &str) -> Result<Value, StoreError> {
        let params = serde_json::json!({ "account": account, "kind": kind });
        self.call.call(proto::methods::plugin::STORE_LEDGER_GET, params).await
    }

    /// 写一份台账。
    pub async fn ledger_put(
        &self,
        account: &str,
        kind: &str,
        value: &Value,
    ) -> Result<(), StoreError> {
        let params = serde_json::json!({ "account": account, "kind": kind, "value": value });
        self.call.call(proto::methods::plugin::STORE_LEDGER_PUT, params).await?;
        Ok(())
    }

    /// 读一条按家配置（例如 CatPaw 的 token2）。
    pub async fn settings_get(&self, provider: &str, key: &str) -> Result<Value, StoreError> {
        let params = serde_json::json!({ "provider": provider, "key": key });
        self.call.call(proto::methods::plugin::SETTINGS_GET, params).await
    }

    /// 写一条按家配置。
    pub async fn settings_set(
        &self,
        provider: &str,
        key: &str,
        value: &Value,
    ) -> Result<(), StoreError> {
        let params = serde_json::json!({ "provider": provider, "key": key, "value": value });
        self.call.call(proto::methods::plugin::SETTINGS_SET, params).await?;
        Ok(())
    }

    /// 通知界面「某账号的登录态变了」（刷新界面用）。
    pub async fn notify_auth_changed(&self, provider: &str, account: &str) {
        let params = serde_json::json!({
            "kind": "auth_changed",
            "provider": provider,
            "account": account,
        });
        let _ = self.call.call(proto::methods::plugin::NOTIFY, params).await;
    }
}

/// 回调发送端（`serve()` 实现，`PluginCtx` 持有）。
pub trait CallGateway: Send + Sync {
    /// 发一次回调并等应答。
    fn call<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Value, StoreError>> + Send + 'a>,
    >;
}

impl From<StoreError> for String {
    fn from(value: StoreError) -> Self {
        value.to_string()
    }
}
