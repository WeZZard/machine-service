//! Throwaway certificate authorities and TLS configurations for one rig run.
//!
//! A run makes two authorities: the broker's, which signs the stand-in
//! target's certificate and which the edge trusts upstream, and the edge's,
//! which mitmproxy uses to sign the certificate it shows the client and which
//! the client trusts. The client never trusts the broker's authority, so a
//! successful handshake proves that TLS ended at the edge.

use std::sync::Arc;

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};

use crate::RigError;

fn tls(error: impl std::fmt::Display) -> RigError {
    RigError::Tls(error.to_string())
}

/// A certificate authority.
pub struct Authority {
    issuer: Issuer<'static, KeyPair>,
    cert_pem: String,
    cert_der: CertificateDer<'static>,
    key_pem: String,
}

impl std::fmt::Debug for Authority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authority").finish_non_exhaustive()
    }
}

/// A certificate and its private key.
#[derive(Debug, Clone)]
pub struct Leaf {
    /// The certificate, PEM.
    pub cert_pem: String,
    /// The certificate, DER.
    pub cert_der: CertificateDer<'static>,
    /// The private key, PKCS #8 PEM.
    pub key_pem: String,
    key_der: Vec<u8>,
}

impl Authority {
    /// A new authority named `common_name`.
    pub fn new(common_name: &str) -> Result<Authority, RigError> {
        let mut params = CertificateParams::new(Vec::<String>::new()).map_err(tls)?;
        let mut name = DistinguishedName::new();
        name.push(DnType::CommonName, common_name);
        params.distinguished_name = name;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let key = KeyPair::generate().map_err(tls)?;
        let cert = params.self_signed(&key).map_err(tls)?;
        Ok(Authority {
            cert_pem: cert.pem(),
            cert_der: cert.der().clone(),
            key_pem: key.serialize_pem(),
            issuer: Issuer::new(params, key),
        })
    }

    /// An authority made elsewhere, from its private key and certificate,
    /// PEM, and the common name its certificate carries, which a leaf's
    /// issuer must repeat (the broker's authority keeps it in `ca.name`).
    pub fn from_parts(
        common_name: &str,
        key_pem: &str,
        cert_pem: &str,
    ) -> Result<Authority, RigError> {
        let mut params = CertificateParams::new(Vec::<String>::new()).map_err(tls)?;
        let mut name = DistinguishedName::new();
        name.push(DnType::CommonName, common_name);
        params.distinguished_name = name;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let key = KeyPair::from_pem(key_pem).map_err(tls)?;
        let cert_der = parse_pem_certs(cert_pem)?.remove(0);
        Ok(Authority {
            cert_pem: cert_pem.to_string(),
            cert_der,
            key_pem: key_pem.to_string(),
            issuer: Issuer::new(params, key),
        })
    }

    /// The authority's certificate, PEM.
    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// The authority's certificate, DER.
    pub fn cert_der(&self) -> &CertificateDer<'static> {
        &self.cert_der
    }

    /// The private key and certificate in one PEM, as mitmproxy reads its
    /// authority from `mitmproxy-ca.pem`.
    pub fn mitmproxy_pem(&self) -> String {
        format!("{}{}", self.key_pem, self.cert_pem)
    }

    /// A server certificate for `names`, signed by this authority.
    pub fn issue(&self, names: &[&str]) -> Result<Leaf, RigError> {
        let names: Vec<String> = names.iter().map(|name| name.to_string()).collect();
        let mut params = CertificateParams::new(Vec::<String>::new()).map_err(tls)?;
        params.subject_alt_names = names
            .iter()
            .map(|name| match name.parse::<std::net::IpAddr>() {
                Ok(address) => Ok(rcgen::SanType::IpAddress(address)),
                Err(_) => name.clone().try_into().map(rcgen::SanType::DnsName),
            })
            .collect::<Result<_, _>>()
            .map_err(tls)?;
        let mut name = DistinguishedName::new();
        name.push(
            DnType::CommonName,
            names.first().cloned().unwrap_or_default(),
        );
        params.distinguished_name = name;
        let key = KeyPair::generate().map_err(tls)?;
        let cert = params.signed_by(&key, &self.issuer).map_err(tls)?;
        Ok(Leaf {
            cert_pem: cert.pem(),
            cert_der: cert.der().clone(),
            key_pem: key.serialize_pem(),
            key_der: key.serialize_der(),
        })
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A server configuration that presents `leaf`.
pub fn server_config(leaf: &Leaf) -> Result<Arc<ServerConfig>, RigError> {
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf.key_der.clone()));
    let config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(tls)?
        .with_no_client_auth()
        .with_single_cert(vec![leaf.cert_der.clone()], key)
        .map_err(tls)?;
    Ok(Arc::new(config))
}

/// A client configuration that trusts only the authorities in `roots_pem`
/// and presents no client certificate (TC-5).
pub fn client_config(roots_pem: &str) -> Result<Arc<ClientConfig>, RigError> {
    let mut roots = RootCertStore::empty();
    for cert in parse_pem_certs(roots_pem)? {
        roots.add(cert).map_err(tls)?;
    }
    let config = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(tls)?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// The certificates in a PEM bundle.
pub fn parse_pem_certs(text: &str) -> Result<Vec<CertificateDer<'static>>, RigError> {
    use rustls::pki_types::pem::PemObject;
    CertificateDer::pem_slice_iter(text.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(tls)
        .and_then(|certs| {
            if certs.is_empty() {
                Err(RigError::Tls("no certificate in the bundle".into()))
            } else {
                Ok(certs)
            }
        })
}
