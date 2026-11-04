//! Explicit maintenance egress must fail closed, never use an account/system proxy.
pub fn strict_client(
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
) -> Result<reqwest::Client, reqwest::Error> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .use_rustls_tls()
        .timeout(std::time::Duration::from_secs(30));
    if let Some(p) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(p.to_url())?);
    }
    builder.build()
}
pub fn client(legacy_proxy: &str) -> Result<reqwest::Client, String> {
    match agent2api_provider_sdk::maintenance::current() {
        None => Ok(crate::http::control_client(None, legacy_proxy)),
        Some(proxy) => strict_client(proxy.as_ref())
            .map_err(|_| "Cannot create maintenance HTTP client".into()),
    }
}
