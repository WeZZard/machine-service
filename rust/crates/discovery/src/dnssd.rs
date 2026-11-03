//! The Mac's own DNS-SD service, `dns_sd.h` in libSystem, which
//! mDNSResponder answers (DC-85). An advertisement lives as long as its
//! [`Advertisement`]; when the process exits, even by a crash, the system
//! withdraws it, so no record outlives its daemon. On other systems every
//! call fails with a reason.

use crate::Found;
use std::time::Duration;

/// Where an advertisement goes and where a browse looks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Every interface: the network.
    Network,
    /// This Mac only, never sent on any network (`LocalOnly`).
    LocalOnly,
    /// A test build's folder, which stands for DNS-SD: an advertisement is
    /// a file in it, found at loopback, and nothing reaches mDNSResponder
    /// (DC-1xx-ISO-2).
    #[cfg(feature = "test-build")]
    Folder(std::path::PathBuf),
}

/// The variable that names a test build's DNS-SD folder.
pub const TEST_VARIABLE: &str = "MACHINE_SERVICE_TEST_BONJOUR";

impl Scope {
    /// Parse the discovery scope. Network discovery is the default; TL-2
    /// tests explicitly restrict both advertisement and browsing to this Mac.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "" | "network" => Ok(Self::Network),
            "local-only" => Ok(Self::LocalOnly),
            other => Err(format!(
                "MACHINE_SERVICE_DISCOVERY_SCOPE must be network or local-only; got {other:?}"
            )),
        }
    }

    /// Read the scope a binary advertises and browses in. A test build's
    /// is always the folder [`TEST_VARIABLE`] names, and without one it has
    /// none: no binary the default suite starts reaches DNS-SD
    /// (DC-1xx-ISO-2). The opt-in live layer reads the setting as a release
    /// does.
    pub fn from_environment() -> Result<Self, String> {
        #[cfg(feature = "test-build")]
        if !service_layout::test_build::is_live() {
            return service_layout::test_build::seam(TEST_VARIABLE)
                .map(Scope::Folder)
                .ok_or_else(|| {
                    format!(
                        "this is a test build, which advertises and browses only in the folder {TEST_VARIABLE} names"
                    )
                });
        }
        match std::env::var("MACHINE_SERVICE_DISCOVERY_SCOPE") {
            Ok(value) => Self::parse(&value),
            Err(std::env::VarError::NotPresent) => Ok(Self::Network),
            Err(std::env::VarError::NotUnicode(_)) => {
                Err("MACHINE_SERVICE_DISCOVERY_SCOPE is not valid UTF-8".into())
            }
        }
    }

    /// Whether what it advertises is found at loopback, so an instance
    /// bound to loopback may advertise in it.
    pub fn is_loopback(&self) -> bool {
        #[cfg(feature = "test-build")]
        if let Scope::Folder(_) = self {
            return true;
        }
        false
    }

    /// A test build never sends an advertisement or a browse on the network.
    fn check(&self) -> Result<(), String> {
        if service_layout::test_build::is_isolated() && *self == Scope::Network {
            return Err(
                "this is a test build, which never advertises or browses on the network".into(),
            );
        }
        Ok(())
    }
}

/// A registered service; dropping it withdraws the advertisement.
pub struct Advertisement {
    /// The name the system registered, which differs from the one asked for
    /// when another service on the network already holds it.
    pub name: String,
    /// Held for its `Drop`, which withdraws the advertisement.
    #[allow(dead_code)]
    held: Held,
}

enum Held {
    #[cfg(target_os = "macos")]
    #[allow(dead_code)]
    Mac(mac::Ref),
    #[cfg(feature = "test-build")]
    File(std::path::PathBuf),
    #[cfg(not(target_os = "macos"))]
    #[allow(dead_code)]
    Nothing,
}

impl Drop for Held {
    fn drop(&mut self) {
        #[cfg(feature = "test-build")]
        if let Held::File(path) = self {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl std::fmt::Debug for Advertisement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Advertisement")
            .field("name", &self.name)
            .finish()
    }
}

/// Register `name` of `service_type` on `port` with `txt`, and wait up to
/// `timeout` for the system to confirm it.
pub fn register(
    name: &str,
    service_type: &str,
    port: u16,
    txt: &[(String, String)],
    scope: Scope,
    timeout: Duration,
) -> Result<Advertisement, String> {
    scope.check()?;
    #[cfg(feature = "test-build")]
    if let Scope::Folder(folder) = &scope {
        let _ = timeout;
        return folder::register(folder, name, service_type, port, txt);
    }
    #[cfg(target_os = "macos")]
    {
        let record = crate::encode_txt(txt)?;
        let (reference, name) = mac::register(name, service_type, port, &record, scope, timeout)?;
        Ok(Advertisement {
            name,
            held: Held::Mac(reference),
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (name, service_type, port, txt, scope, timeout);
        Err("Bonjour advertisement needs macOS's DNS-SD service".to_string())
    }
}

/// Browse `service_type` in `local.` for `window`, then resolve each
/// instance's host, port, TXT record and IPv4 address within `resolve`.
pub fn browse(
    service_type: &str,
    scope: Scope,
    window: Duration,
    resolve: Duration,
) -> Result<Vec<Found>, String> {
    scope.check()?;
    #[cfg(feature = "test-build")]
    if let Scope::Folder(folder) = &scope {
        let _ = resolve;
        return Ok(folder::browse(folder, service_type, window));
    }
    #[cfg(target_os = "macos")]
    {
        mac::browse(service_type, scope, window, resolve)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (service_type, scope, window, resolve);
        Err("Bonjour browsing needs macOS's DNS-SD service".to_string())
    }
}

/// A test build's DNS-SD: one JSON file per advertisement, found at
/// loopback. A browse gathers for its whole window, as DNS-SD's does.
#[cfg(feature = "test-build")]
mod folder {
    use super::{Advertisement, Held};
    use crate::Found;
    use std::path::Path;
    use std::time::Duration;

    fn file_name(service_type: &str, name: &str) -> String {
        let safe: String = format!("{service_type}.{name}")
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        format!("{safe}.advertisement")
    }

    pub fn register(
        folder: &Path,
        name: &str,
        service_type: &str,
        port: u16,
        txt: &[(String, String)],
    ) -> Result<Advertisement, String> {
        std::fs::create_dir_all(folder)
            .map_err(|error| format!("{}: {error}", folder.display()))?;
        let mut lines = vec![name.to_string(), service_type.to_string(), port.to_string()];
        lines.extend(txt.iter().map(|(key, value)| format!("{key}={value}")));
        let path = folder.join(file_name(service_type, name));
        let staged = path.with_extension("staged");
        std::fs::write(&staged, lines.join("\n")).map_err(|error| error.to_string())?;
        std::fs::rename(&staged, &path).map_err(|error| error.to_string())?;
        Ok(Advertisement {
            name: name.to_string(),
            held: Held::File(path),
        })
    }

    pub fn browse(folder: &Path, service_type: &str, window: Duration) -> Vec<Found> {
        std::thread::sleep(window);
        let Ok(entries) = std::fs::read_dir(folder) else {
            return Vec::new();
        };
        let mut found: Vec<Found> = entries
            .flatten()
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|e| e == "advertisement")
            })
            .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
            .filter_map(|text| {
                let mut lines = text.split('\n');
                let name = lines.next()?.to_string();
                (lines.next()? == service_type).then_some(())?;
                let port = lines.next()?.parse().ok()?;
                let txt = lines
                    .map(|line| {
                        let (key, value) = line.split_once('=').unwrap_or((line, ""));
                        (key.to_ascii_lowercase(), value.to_string())
                    })
                    .collect();
                Some(Found {
                    name,
                    host: "localhost.".to_string(),
                    address: Some(std::net::Ipv4Addr::LOCALHOST),
                    port,
                    txt,
                })
            })
            .collect();
        found.sort_by(|a, b| a.name.cmp(&b.name));
        found
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use super::Scope;
    use crate::{decode_txt, Found};
    use std::ffi::{c_char, c_void, CStr, CString};
    use std::time::{Duration, Instant};

    type DnsServiceRef = *mut c_void;
    type Flags = u32;
    type ErrorType = i32;

    const NO_ERROR: ErrorType = 0;
    const FLAG_MORE_COMING: Flags = 0x1;
    const FLAG_ADD: Flags = 0x2;
    const INTERFACE_ANY: u32 = 0;
    const INTERFACE_LOCAL_ONLY: u32 = u32::MAX;
    const PROTOCOL_IPV4: u32 = 0x01;

    type RegisterReply = extern "C" fn(
        DnsServiceRef,
        Flags,
        ErrorType,
        *const c_char,
        *const c_char,
        *const c_char,
        *mut c_void,
    );
    type BrowseReply = extern "C" fn(
        DnsServiceRef,
        Flags,
        u32,
        ErrorType,
        *const c_char,
        *const c_char,
        *const c_char,
        *mut c_void,
    );
    type ResolveReply = extern "C" fn(
        DnsServiceRef,
        Flags,
        u32,
        ErrorType,
        *const c_char,
        *const c_char,
        u16,
        u16,
        *const u8,
        *mut c_void,
    );
    type AddrInfoReply = extern "C" fn(
        DnsServiceRef,
        Flags,
        u32,
        ErrorType,
        *const c_char,
        *const libc::sockaddr,
        u32,
        *mut c_void,
    );

    // libSystem, which every macOS process links.
    extern "C" {
        fn DNSServiceRegister(
            reference: *mut DnsServiceRef,
            flags: Flags,
            interface: u32,
            name: *const c_char,
            service_type: *const c_char,
            domain: *const c_char,
            host: *const c_char,
            port: u16,
            txt_length: u16,
            txt: *const c_void,
            reply: RegisterReply,
            context: *mut c_void,
        ) -> ErrorType;
        fn DNSServiceBrowse(
            reference: *mut DnsServiceRef,
            flags: Flags,
            interface: u32,
            service_type: *const c_char,
            domain: *const c_char,
            reply: BrowseReply,
            context: *mut c_void,
        ) -> ErrorType;
        fn DNSServiceResolve(
            reference: *mut DnsServiceRef,
            flags: Flags,
            interface: u32,
            name: *const c_char,
            service_type: *const c_char,
            domain: *const c_char,
            reply: ResolveReply,
            context: *mut c_void,
        ) -> ErrorType;
        fn DNSServiceGetAddrInfo(
            reference: *mut DnsServiceRef,
            flags: Flags,
            interface: u32,
            protocol: u32,
            host: *const c_char,
            reply: AddrInfoReply,
            context: *mut c_void,
        ) -> ErrorType;
        fn DNSServiceRefSockFD(reference: DnsServiceRef) -> i32;
        fn DNSServiceProcessResult(reference: DnsServiceRef) -> ErrorType;
        fn DNSServiceRefDeallocate(reference: DnsServiceRef);
    }

    /// An owned `DNSServiceRef`, deallocated on drop.
    pub struct Ref(DnsServiceRef);
    // The reference is only used by its owner; mDNSResponder's client
    // library allows a reference to move between threads.
    unsafe impl Send for Ref {}
    unsafe impl Sync for Ref {}
    impl Drop for Ref {
        fn drop(&mut self) {
            unsafe { DNSServiceRefDeallocate(self.0) }
        }
    }

    fn interface(scope: Scope) -> u32 {
        match scope {
            Scope::LocalOnly => INTERFACE_LOCAL_ONLY,
            _ => INTERFACE_ANY,
        }
    }

    fn c_string(value: &str) -> Result<CString, String> {
        CString::new(value).map_err(|_| format!("{value:?} contains a NUL byte"))
    }

    fn text(value: *const c_char) -> String {
        if value.is_null() {
            return String::new();
        }
        unsafe { CStr::from_ptr(value) }
            .to_string_lossy()
            .into_owned()
    }

    fn check(call: &str, error: ErrorType) -> Result<(), String> {
        if error == NO_ERROR {
            Ok(())
        } else {
            Err(format!("{call} failed with DNS-SD error {error}"))
        }
    }

    /// Wait for the reference's socket and hand each reply to its callback,
    /// until `done` says so or `deadline` passes. True when `done` said so.
    fn pump(reference: &Ref, deadline: Instant, done: &dyn Fn() -> bool) -> Result<bool, String> {
        let fd = unsafe { DNSServiceRefSockFD(reference.0) };
        if fd < 0 {
            return Err("DNSServiceRefSockFD returned no socket".to_string());
        }
        while !done() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(false);
            }
            let mut poll = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let millis = i32::try_from(left.as_millis()).unwrap_or(i32::MAX).max(1);
            let ready = unsafe { libc::poll(&mut poll, 1, millis) };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(format!("poll failed: {error}"));
            }
            if ready > 0 {
                check("DNSServiceProcessResult", unsafe {
                    DNSServiceProcessResult(reference.0)
                })?;
            }
        }
        Ok(true)
    }

    struct Registered {
        result: Option<Result<String, ErrorType>>,
    }

    extern "C" fn on_register(
        _reference: DnsServiceRef,
        _flags: Flags,
        error: ErrorType,
        name: *const c_char,
        _service_type: *const c_char,
        _domain: *const c_char,
        context: *mut c_void,
    ) {
        let state = unsafe { &mut *(context as *mut Registered) };
        state.result = Some(if error == NO_ERROR {
            Ok(text(name))
        } else {
            Err(error)
        });
    }

    pub fn register(
        name: &str,
        service_type: &str,
        port: u16,
        txt: &[u8],
        scope: Scope,
        timeout: Duration,
    ) -> Result<(Ref, String), String> {
        let (c_name, c_type) = (c_string(name)?, c_string(service_type)?);
        let txt_length = u16::try_from(txt.len()).map_err(|_| "TXT record too long".to_string())?;
        // The callback writes here; it lives on the heap until the
        // registration has answered or been deallocated.
        let state = Box::into_raw(Box::new(Registered { result: None }));
        let mut raw: DnsServiceRef = std::ptr::null_mut();
        let error = unsafe {
            DNSServiceRegister(
                &mut raw,
                0,
                interface(scope),
                c_name.as_ptr(),
                c_type.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                port.to_be(),
                txt_length,
                txt.as_ptr() as *const c_void,
                on_register,
                state as *mut c_void,
            )
        };
        if error != NO_ERROR {
            drop(unsafe { Box::from_raw(state) });
            return Err(format!(
                "DNSServiceRegister failed with DNS-SD error {error}"
            ));
        }
        let reference = Ref(raw);
        let answered = pump(&reference, Instant::now() + timeout, &|| unsafe {
            (*state).result.is_some()
        });
        // Later replies, such as a rename after a conflict, are not read, so
        // the state is never written again once taken here.
        let result = unsafe { Box::from_raw(state) }.result;
        match (answered, result) {
            (Ok(true), Some(Ok(registered))) => Ok((reference, registered)),
            (Ok(true), Some(Err(error))) => Err(format!(
                "the system refused to register {name} with DNS-SD error {error}"
            )),
            (Err(error), _) => Err(error),
            _ => Err(format!(
                "the system did not confirm the registration of {name} within {} s",
                timeout.as_secs_f32()
            )),
        }
    }

    struct Browsed {
        names: Vec<(String, String, u32)>,
        error: Option<ErrorType>,
    }

    extern "C" fn on_browse(
        _reference: DnsServiceRef,
        flags: Flags,
        interface: u32,
        error: ErrorType,
        name: *const c_char,
        service_type: *const c_char,
        domain: *const c_char,
        context: *mut c_void,
    ) {
        let state = unsafe { &mut *(context as *mut Browsed) };
        if error != NO_ERROR {
            state.error = Some(error);
            return;
        }
        let entry = (
            text(name),
            format!("{}|{}", text(service_type), text(domain)),
            interface,
        );
        if flags & FLAG_ADD != 0 {
            if !state.names.iter().any(|(n, _, _)| *n == entry.0) {
                state.names.push(entry);
            }
        } else {
            state.names.retain(|(n, _, _)| *n != entry.0);
        }
    }

    struct Resolved {
        found: Option<(String, u16, Vec<u8>)>,
        error: Option<ErrorType>,
    }

    extern "C" fn on_resolve(
        _reference: DnsServiceRef,
        _flags: Flags,
        _interface: u32,
        error: ErrorType,
        _full_name: *const c_char,
        host: *const c_char,
        port: u16,
        txt_length: u16,
        txt: *const u8,
        context: *mut c_void,
    ) {
        let state = unsafe { &mut *(context as *mut Resolved) };
        if error != NO_ERROR {
            state.error = Some(error);
            return;
        }
        let record = if txt.is_null() {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(txt, usize::from(txt_length)) }.to_vec()
        };
        state.found = Some((text(host), u16::from_be(port), record));
    }

    struct Addressed {
        address: Option<std::net::Ipv4Addr>,
        done: bool,
    }

    extern "C" fn on_address(
        _reference: DnsServiceRef,
        flags: Flags,
        _interface: u32,
        error: ErrorType,
        _host: *const c_char,
        address: *const libc::sockaddr,
        _ttl: u32,
        context: *mut c_void,
    ) {
        let state = unsafe { &mut *(context as *mut Addressed) };
        if error == NO_ERROR && !address.is_null() && flags & FLAG_ADD != 0 {
            let family = unsafe { (*address).sa_family };
            if i32::from(family) == libc::AF_INET {
                let inet = unsafe { &*(address as *const libc::sockaddr_in) };
                state
                    .address
                    .get_or_insert(std::net::Ipv4Addr::from(u32::from_be(inet.sin_addr.s_addr)));
            }
        }
        if flags & FLAG_MORE_COMING == 0 {
            state.done = true;
        }
    }

    fn resolve_one(
        name: &str,
        service_type: &str,
        domain: &str,
        interface: u32,
        deadline: Instant,
    ) -> Result<Found, String> {
        let (c_name, c_type, c_domain) =
            (c_string(name)?, c_string(service_type)?, c_string(domain)?);
        let mut state = Resolved {
            found: None,
            error: None,
        };
        let mut raw: DnsServiceRef = std::ptr::null_mut();
        check("DNSServiceResolve", unsafe {
            DNSServiceResolve(
                &mut raw,
                0,
                interface,
                c_name.as_ptr(),
                c_type.as_ptr(),
                c_domain.as_ptr(),
                on_resolve,
                &mut state as *mut Resolved as *mut c_void,
            )
        })?;
        let reference = Ref(raw);
        let state_ptr = &state as *const Resolved;
        pump(&reference, deadline, &|| unsafe {
            (*state_ptr).found.is_some() || (*state_ptr).error.is_some()
        })?;
        drop(reference);
        if let Some(error) = state.error {
            return Err(format!("resolving {name} failed with DNS-SD error {error}"));
        }
        let (host, port, record) = state
            .found
            .ok_or_else(|| format!("{name} did not resolve in time"))?;
        Ok(Found {
            name: name.to_string(),
            address: address_of(&host, deadline),
            host,
            port,
            txt: decode_txt(&record),
        })
    }

    fn address_of(host: &str, deadline: Instant) -> Option<std::net::Ipv4Addr> {
        let c_host = c_string(host).ok()?;
        let mut state = Addressed {
            address: None,
            done: false,
        };
        let mut raw: DnsServiceRef = std::ptr::null_mut();
        let error = unsafe {
            DNSServiceGetAddrInfo(
                &mut raw,
                0,
                INTERFACE_ANY,
                PROTOCOL_IPV4,
                c_host.as_ptr(),
                on_address,
                &mut state as *mut Addressed as *mut c_void,
            )
        };
        if error != NO_ERROR {
            return None;
        }
        let reference = Ref(raw);
        let state_ptr = &state as *const Addressed;
        let _ = pump(&reference, deadline, &|| unsafe { (*state_ptr).done });
        drop(reference);
        state.address
    }

    pub fn browse(
        service_type: &str,
        scope: Scope,
        window: Duration,
        resolve: Duration,
    ) -> Result<Vec<Found>, String> {
        let c_type = c_string(service_type)?;
        let mut state = Browsed {
            names: Vec::new(),
            error: None,
        };
        let mut raw: DnsServiceRef = std::ptr::null_mut();
        check("DNSServiceBrowse", unsafe {
            DNSServiceBrowse(
                &mut raw,
                0,
                interface(scope),
                c_type.as_ptr(),
                c"local.".as_ptr(),
                on_browse,
                &mut state as *mut Browsed as *mut c_void,
            )
        })?;
        let reference = Ref(raw);
        // A browse never ends by itself: it gathers for the whole window.
        pump(&reference, Instant::now() + window, &|| false)?;
        drop(reference);
        if let Some(error) = state.error {
            return Err(format!(
                "browsing {service_type} failed with DNS-SD error {error}"
            ));
        }
        let deadline = Instant::now() + resolve;
        let mut found = Vec::new();
        for (name, kind, interface) in state.names {
            let (resolved_type, domain) = kind.split_once('|').unwrap_or((service_type, "local."));
            if let Ok(instance) = resolve_one(&name, resolved_type, domain, interface, deadline) {
                found.push(instance);
            }
        }
        found.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(found)
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// TL-2: isolated discovery uses the same scope for the daemon and CLI.
    #[test]
    fn local_only_scope_is_explicit_and_network_is_default() {
        assert_eq!(Scope::parse(""), Ok(Scope::Network));
        assert_eq!(Scope::parse("network"), Ok(Scope::Network));
        assert_eq!(Scope::parse("local-only"), Ok(Scope::LocalOnly));
        assert!(Scope::parse("local")
            .unwrap_err()
            .contains("MACHINE_SERVICE_DISCOVERY_SCOPE"));
    }

    /// A registration on this Mac only is found by a browse on this Mac only,
    /// with its port and TXT record, and is gone once withdrawn. Nothing is
    /// sent on any network.
    #[test]
    fn a_local_only_advertisement_is_browsed_resolved_and_withdrawn() {
        let service_type = "_machine-svc-test._tcp";
        let name = format!("discovery-test-{}", std::process::id());
        let txt = crate::txt_entries(&name, "host-id", &["durable"]);
        let advertisement = register(
            &name,
            service_type,
            46240,
            &txt,
            Scope::LocalOnly,
            Duration::from_secs(5),
        )
        .expect("register");
        assert_eq!(advertisement.name, name);
        let found = browse(
            service_type,
            Scope::LocalOnly,
            Duration::from_millis(1500),
            Duration::from_secs(3),
        )
        .expect("browse");
        let mine = found
            .iter()
            .find(|found| found.name == name)
            .unwrap_or_else(|| panic!("{name} not found in {found:?}"));
        assert_eq!(mine.port, 46240);
        assert_eq!(mine.instance(), name);
        assert_eq!(mine.kinds(), vec!["durable".to_string()]);
        drop(advertisement);
        let after = browse(
            service_type,
            Scope::LocalOnly,
            Duration::from_millis(1500),
            Duration::from_secs(1),
        )
        .expect("browse after withdrawal");
        assert!(after.iter().all(|found| found.name != name), "{after:?}");
    }

    /// A test build's folder stands for DNS-SD: an advertisement is found
    /// at loopback with its TXT record and is gone once withdrawn; the
    /// network scope is refused.
    #[cfg(feature = "test-build")]
    #[test]
    fn a_test_build_advertises_only_in_its_folder() {
        let folder = tempfile::tempdir().unwrap();
        let scope = Scope::Folder(folder.path().to_path_buf());
        assert!(scope.is_loopback());
        let txt = crate::txt_entries("Mac-T", "host-id", &["durable"]);
        let advertisement = register(
            "Mac-T",
            "_t._tcp",
            46241,
            &txt,
            scope.clone(),
            Duration::ZERO,
        )
        .unwrap();
        let found = browse("_t._tcp", scope.clone(), Duration::ZERO, Duration::ZERO).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].base_url(), "http://127.0.0.1:46241");
        assert_eq!(found[0].machine_id(), Some("host-id"));
        assert!(
            browse("_other._tcp", scope.clone(), Duration::ZERO, Duration::ZERO)
                .unwrap()
                .is_empty()
        );
        drop(advertisement);
        assert!(browse("_t._tcp", scope, Duration::ZERO, Duration::ZERO)
            .unwrap()
            .is_empty());
        for refused in [
            register("x", "_t._tcp", 1, &[], Scope::Network, Duration::ZERO).map(|_| ()),
            browse("_t._tcp", Scope::Network, Duration::ZERO, Duration::ZERO).map(|_| ()),
        ] {
            assert!(refused.unwrap_err().contains("test build"));
        }
    }
}
