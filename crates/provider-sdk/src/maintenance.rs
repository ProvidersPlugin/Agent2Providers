//! Optional, per-RPC maintenance egress. Never shared with model/login requests.
use agent2api_provider_protocol::WireProxy;
use serde::Deserialize;
use serde_json::Value;
use std::cell::Cell;
use std::future::Future;

pub const CAPABILITIES_METHOD: &str = "maintenance.capabilities";
pub const OPERATIONS: &[&str] = &["usage.query", "checkin.claim", "claim", "claim.status"];

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Egress {
    pub version: u32,
    // Required even for direct egress; missing must not silently mean direct.
    pub proxy: Value,
}

impl Egress {
    pub fn decode(value: Value) -> Result<Option<WireProxy>, String> {
        if value.get("proxy").is_none() {
            return Err("Missing maintenance proxy selection".into());
        }
        let e: Self = serde_json::from_value(value).map_err(|_| "Invalid maintenance egress")?;
        if e.version != 1 {
            return Err("Unsupported maintenance egress version".into());
        }
        if e.proxy.is_null() {
            return Ok(None);
        }
        let p: WireProxy =
            serde_json::from_value(e.proxy).map_err(|_| "Invalid maintenance proxy")?;
        if !matches!(p.protocol.as_str(), "http" | "socks5")
            || p.host.is_empty()
            || p.host
                .chars()
                .any(|c| !c.is_ascii_alphanumeric() && !matches!(c, '.' | '-' | ':' | '[' | ']'))
            || p.port.unwrap_or(0) == 0
        {
            return Err("Invalid maintenance proxy".into());
        }
        Ok(Some(p))
    }
}

tokio::task_local! {
    static CURRENT: Option<Option<WireProxy>>;
    static TRANSPORT_FAILED: Cell<bool>;
}
/// Called only for transport errors, never an upstream HTTP/business rejection.
pub fn record_transport_failure() {
    if current().is_some() {
        let _ = TRANSPORT_FAILED.try_with(|value| value.set(true));
    }
}
pub(crate) fn transport_failed() -> bool {
    TRANSPORT_FAILED.try_with(Cell::get).unwrap_or(false)
}
/// None = old host/original behavior; Some(None) = explicitly direct.
pub fn current() -> Option<Option<WireProxy>> {
    CURRENT.try_with(Clone::clone).ok().flatten()
}
pub(crate) async fn scope<F: Future>(selection: Option<Option<WireProxy>>, work: F) -> F::Output {
    CURRENT
        .scope(selection, TRANSPORT_FAILED.scope(Cell::new(false), work))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn validates_without_echoing_secrets() {
        assert!(Egress::decode(json!({"version":1,"proxy":null}))
            .unwrap()
            .is_none());
        for bad in [
            json!({"version":2,"proxy":null}),
            json!({"version":1}),
            json!({"version":1,"proxy":{"protocol":"ftp","host":"secret@example.invalid","port":80}}),
        ] {
            assert!(Egress::decode(bad).is_err());
        }
        let p = Egress::decode(
            json!({"version":1,"proxy":{"protocol":"http","host":"127.0.0.1","port":8080}}),
        )
        .unwrap();
        assert_eq!(p.unwrap().port, Some(8080));
    }
    #[tokio::test]
    async fn isolation_and_no_inheritance() {
        assert!(current().is_none());
        let (a, b) = tokio::join!(
            scope(Some(None), async {
                tokio::task::yield_now().await;
                assert!(tokio::spawn(async { current() }).await.unwrap().is_none());
                current().is_some()
            }),
            scope(None, async {
                tokio::task::yield_now().await;
                current().is_none()
            })
        );
        assert!(a && b);
        assert!(current().is_none());
    }
}
