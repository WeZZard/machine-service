//! The broker's TLS: a per-instance authority that signs a certificate for
//! whatever host name a client asks for, and for the host's own addresses
//! when it asks for none (`docs/arch/relay-protocol.md` RP-1).
//!
//! A client pins the authority by its fingerprint, which discovery publishes,
//! so the broker is reached by any of its names (TC-6) and through a proxy
//! that ends TLS and trusts the authority upstream (TC-5).

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::net::IpAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use sha2::{Digest, Sha256};

const CA_CERT: &str = "ca.pem";
const CA_KEY: &str = "ca.key";
const CA_NAME: &str = "ca.name";
/// The most leaf certificates kept in memory.
const CACHE_LIMIT: usize = 256;

/// The broker's certificate authority.
pub struct Authority {
    issuer: Issuer<'static, KeyPair>,
    cert_pem: String,
    sha256: String,
}

impl std::fmt::Debug for Authority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authority")
            .field("sha256", &self.sha256)
            .finish_non_exhaustive()
    }
}

fn ca_params(common_name: &str) -> Result<CertificateParams, String> {
    let mut params = CertificateParams::new(Vec::<String>::new()).map_err(|e| e.to_string())?;
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, common_name);
    params.distinguished_name = name;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    Ok(params)
}

/// The lower-case hexadecimal SHA-256 of the first certificate in `pem`, as
/// `relay_ca_sha256` names an authority.
pub fn pem_sha256(pem: &str) -> Option<String> {
    use rustls::pki_types::pem::PemObject;
    let der = CertificateDer::pem_slice_iter(pem.as_bytes())
        .next()?
        .ok()?;
    Some(hex(&Sha256::digest(der.as_ref())))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn write_private(path: &Path, contents: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    file.write_all(contents.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

impl Authority {
    /// A new authority named `common_name`.
    pub fn generate(common_name: &str) -> Result<Authority, String> {
        let params = ca_params(common_name)?;
        let key = KeyPair::generate().map_err(|e| e.to_string())?;
        let cert = params.self_signed(&key).map_err(|e| e.to_string())?;
        Ok(Authority {
            sha256: hex(&Sha256::digest(cert.der().as_ref())),
            cert_pem: cert.pem(),
            issuer: Issuer::new(params, key),
        })
    }

    /// The authority kept in `dir`, made there on first use. The key file is
    /// mode 600 in a mode-700 folder. The issuer is rebuilt from the kept key
    /// and name, which give the same subject the certificate carries.
    pub fn load_or_create(dir: &Path) -> Result<Authority, String> {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        let paths: [PathBuf; 3] = [CA_CERT, CA_KEY, CA_NAME].map(|f| dir.join(f));
        if paths.iter().all(|p| p.exists()) {
            let read =
                |p: &PathBuf| fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()));
            let cert_pem = read(&paths[0])?;
            let key = KeyPair::from_pem(&read(&paths[1])?).map_err(|e| e.to_string())?;
            let name = read(&paths[2])?.trim().to_string();
            let sha256 = pem_sha256(&cert_pem)
                .ok_or_else(|| format!("{} holds no certificate", paths[0].display()))?;
            return Ok(Authority {
                issuer: Issuer::new(ca_params(&name)?, key),
                cert_pem,
                sha256,
            });
        }
        let name = format!(
            "machine-service relay broker {}",
            hex(&rand::random::<[u8; 8]>())
        );
        let params = ca_params(&name)?;
        let key = KeyPair::generate().map_err(|e| e.to_string())?;
        let cert = params.self_signed(&key).map_err(|e| e.to_string())?;
        write_private(&paths[1], &key.serialize_pem())?;
        write_private(&paths[2], &format!("{name}\n"))?;
        write_private(&paths[0], &cert.pem())?;
        Ok(Authority {
            sha256: hex(&Sha256::digest(cert.der().as_ref())),
            cert_pem: cert.pem(),
            issuer: Issuer::new(params, key),
        })
    }

    /// The authority's certificate, PEM.
    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// The SHA-256 of the certificate's DER, lower-case hexadecimal.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// A server certificate for host names and addresses, signed by this
    /// authority.
    pub fn issue(&self, names: &[String]) -> Result<CertifiedKey, String> {
        let mut params = CertificateParams::new(Vec::<String>::new()).map_err(|e| e.to_string())?;
        params.subject_alt_names = names
            .iter()
            .map(|name| match name.parse::<IpAddr>() {
                Ok(ip) => Ok(SanType::IpAddress(ip)),
                Err(_) => name
                    .clone()
                    .try_into()
                    .map(SanType::DnsName)
                    .map_err(|e: rcgen::Error| e.to_string()),
            })
            .collect::<Result<_, _>>()?;
        let mut dn = DistinguishedName::new();
        dn.push(
            DnType::CommonName,
            names.first().cloned().unwrap_or_default(),
        );
        params.distinguished_name = dn;
        let key = KeyPair::generate().map_err(|e| e.to_string())?;
        let cert = params
            .signed_by(&key, &self.issuer)
            .map_err(|e| e.to_string())?;
        let der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        let signer =
            rustls::crypto::ring::sign::any_supported_type(&der).map_err(|e| e.to_string())?;
        Ok(CertifiedKey::new(vec![cert.der().clone()], signer))
    }
}

/// Picks the certificate for each handshake.
struct Resolver {
    authority: Arc<Authority>,
    default_names: Box<dyn Fn() -> Vec<String> + Send + Sync>,
    cache: Mutex<HashMap<String, Arc<CertifiedKey>>>,
}

impl std::fmt::Debug for Resolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolver").finish_non_exhaustive()
    }
}

impl Resolver {
    fn names_for(&self, hello: &ClientHello<'_>) -> Vec<String> {
        match hello.server_name() {
            Some(name) if !name.is_empty() => vec![name.to_ascii_lowercase()],
            _ => {
                let mut names = (self.default_names)();
                names.sort();
                names.dedup();
                names
            }
        }
    }
}

impl ResolvesServerCert for Resolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let names = self.names_for(&hello);
        let key = names.join(",");
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(found) = cache.get(&key) {
            return Some(found.clone());
        }
        let issued = Arc::new(self.authority.issue(&names).ok()?);
        if cache.len() >= CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(key, issued.clone());
        Some(issued)
    }
}

/// A server configuration that presents certificates from `authority`: one
/// for the TLS server name a client sends, or, without one, one for the
/// names `default_names` gives at that moment, such as the host's own
/// addresses. Only ALPN `http/1.1` is offered, as the remote session needs.
pub fn server_config(
    authority: Arc<Authority>,
    default_names: impl Fn() -> Vec<String> + Send + Sync + 'static,
) -> Result<Arc<rustls::ServerConfig>, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(Resolver {
            authority,
            default_names: Box::new(default_names),
            cache: Mutex::new(HashMap::new()),
        }));
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// The names a client without a TLS server name reaches this host by: its
/// loopback and interface addresses, `localhost` and its host name.
pub fn host_names() -> Vec<String> {
    let mut names: Vec<String> = crate::net::local_addresses()
        .into_iter()
        .map(|ip| ip.to_string())
        .collect();
    names.push("localhost".into());
    names.extend(crate::net::host_name());
    names.extend(crate::net::network_host_name());
    names
}
