//! TLS configuration (TC-1, TC-5). Clients authenticate with a bearer token in
//! the hello, never with a client certificate, so a proxy that ends TLS and
//! opens a new TLS connection to the server breaks nothing.

use std::sync::{Arc, OnceLock};

use rustls::crypto::{ring, CryptoProvider};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::RootCertStore;

use crate::Error;

/// HTTP/1.1 is the only protocol a WebSocket upgrade runs on here. Offering it
/// keeps a TLS-ending proxy from choosing HTTP/2 towards either side.
const ALPN_HTTP1: &[u8] = b"http/1.1";

fn provider() -> Arc<CryptoProvider> {
    Arc::new(ring::default_provider())
}

fn tls_error(e: impl std::fmt::Display) -> Error {
    Error::Config(format!("TLS: {e}"))
}

/// A server configuration from a PEM certificate chain and a PEM private key,
/// with no client authentication.
pub fn server_config(
    cert_chain_pem: &[u8],
    key_pem: &[u8],
) -> Result<Arc<rustls::ServerConfig>, Error> {
    let certs = CertificateDer::pem_slice_iter(cert_chain_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(tls_error)?;
    if certs.is_empty() {
        return Err(tls_error("no certificate in the PEM chain"));
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(tls_error)?;
    let mut config = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(tls_error)?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(tls_error)?;
    config.alpn_protocols = vec![ALPN_HTTP1.to_vec()];
    Ok(Arc::new(config))
}

/// A client configuration that trusts the Mozilla roots and the PEM
/// certificates in `extra_roots_pem`, such as a lab's own authority.
pub fn client_config(extra_roots_pem: &[&[u8]]) -> Result<Arc<rustls::ClientConfig>, Error> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    for pem in extra_roots_pem {
        for cert in CertificateDer::pem_slice_iter(pem) {
            roots.add(cert.map_err(tls_error)?).map_err(tls_error)?;
        }
    }
    let mut config = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(tls_error)?
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![ALPN_HTTP1.to_vec()];
    Ok(Arc::new(config))
}

pub(crate) fn default_client_config() -> Arc<rustls::ClientConfig> {
    static DEFAULT: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    DEFAULT
        .get_or_init(|| client_config(&[]).expect("the Mozilla roots load"))
        .clone()
}
