use std::sync::{Arc, OnceLock};
use std::time::Duration;

fn tls() -> rustls::ClientConfig {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("the ring provider supports the default TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth()
}

pub fn builder() -> reqwest::ClientBuilder {
    let b = reqwest::Client::builder()
        .tls_backend_preconfigured(tls())
        .user_agent(format!("agentbench/{}", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .pool_idle_timeout(Duration::from_secs(30))
        .tcp_keepalive(Duration::from_secs(15))
        .tcp_keepalive_interval(Duration::from_secs(5))
        .tcp_keepalive_retries(3);
    #[cfg(target_os = "linux")]
    let b = b.tcp_user_timeout(Duration::from_secs(30));
    b
}

pub fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| builder().build().expect("an http client"))
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    #[ignore = "reaches the internet"]
    async fn https_works_with_the_bundled_roots() {
        let r = super::client().get("https://yalr.v0l.io/v1/models").send().await.unwrap();
        assert!(r.status().as_u16() < 500, "{}", r.status());
    }
}
