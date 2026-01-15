use std::sync::OnceLock;

use rustls::ClientConfig;
use rustls_platform_verifier::ConfigVerifierExt;

static TLS_CONFIG: OnceLock<rustls::ClientConfig> = OnceLock::new();

pub fn tls_config() -> ClientConfig {
    TLS_CONFIG
        .get_or_init(|| {
            // Use ring as the crypto provider
            rustls::crypto::ring::default_provider()
                .install_default()
                .ok();

            ClientConfig::with_platform_verifier()
        })
        .clone()
}
