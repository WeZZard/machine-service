//! What an `internet` guest needs beyond Softnet (DC-14, DC-15, DC-16;
//! `docs/acquisition-contract.md`, "Guest network"):
//!
//! - the guest firewall, a pf anchor that drops the connections each
//!   `internet` guest, listed by address in a table file, opens to this host;
//! - resolvers reached over DNS over TLS, because the gateway's resolver is
//!   this host and a host may intercept plain DNS;
//! - an isolation check before the lease is ready: the guest must fail to
//!   reach a listener on this host through its default gateway, and must
//!   resolve a name to a public address and load it over HTTPS.
//!
//! The functions here build the guest commands and decide from their output;
//! `ops.rs` runs them.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

use crate::ssh::shell_quote;

/// The resolvers an `internet` guest gets when `MACHINE_SERVICE_GUEST_DNS` is unset.
pub const DEFAULT_GUEST_DNS: &str = "1.1.1.1#cloudflare-dns.com,8.8.8.8#dns.google";
/// The environment variable that overrides [`DEFAULT_GUEST_DNS`].
pub const GUEST_DNS_VARIABLE: &str = "MACHINE_SERVICE_GUEST_DNS";
/// The environment variable that overrides the anchor file the service reads.
pub const ANCHOR_FILE_VARIABLE: &str = "MACHINE_SERVICE_GUEST_FIREWALL_ANCHOR";
/// The environment variable that overrides the table file the service writes.
pub const TABLE_FILE_VARIABLE: &str = "MACHINE_SERVICE_GUEST_FIREWALL_TABLE";
/// The environment variable that overrides the macOS DNS-over-TLS forwarder.
pub const FORWARDER_VARIABLE: &str = "MACHINE_SERVICE_DOT_FORWARDER";
/// The forwarder's executable name beside `machine-service` (IN-1).
pub const FORWARDER: &str = service_layout::Executable::DotForwarder.name();
/// A marker in the probe command, so a fake guest can recognise it.
pub const PROBE_MARKER: &str = "vm-service isolation probe";
/// A marker in the egress check, so a fake guest can recognise it.
pub const EGRESS_MARKER: &str = "vm-service egress check";
/// The name the egress check resolves and loads over HTTPS.
pub const EGRESS_HOST: &str = "example.com";
/// How long the guest waits for the gateway to answer, in seconds.
pub const PROBE_TIMEOUT_S: u64 = 3;
/// How long the isolation check retries a probe that the host answers, in
/// seconds, while the LaunchDaemon reloads the table.
pub const PROBE_RETRY_S: u64 = 15;
/// How many times the egress check runs before it fails the lease.
pub const EGRESS_ATTEMPTS: u32 = 4;

/// The path the forwarder is installed at in a macOS guest.
const GUEST_FORWARDER: &str = "/usr/local/libexec/vm-service/vm-service-dot-forwarder";
/// Where the forwarder is uploaded before root installs it.
pub const FORWARDER_UPLOAD: &str = "/tmp/vm-service-dot-forwarder.upload";
/// The forwarder's LaunchDaemon label in a macOS guest.
const GUEST_FORWARDER_LABEL: &str = "com.wezzard.vm-service.dot-forwarder";

// ------------------------------------------------------------------ resolvers

/// A resolver an `internet` guest reaches over DNS over TLS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestResolver {
    /// Its public IPv4 address.
    pub address: Ipv4Addr,
    /// The name its certificate is checked against, when given.
    pub name: Option<String>,
}

impl fmt::Display for GuestResolver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.name {
            Some(name) => write!(formatter, "{}#{name}", self.address),
            None => write!(formatter, "{}", self.address),
        }
    }
}

/// Parse the guest resolvers: `value` is `MACHINE_SERVICE_GUEST_DNS`, a
/// comma-separated list of `a.b.c.d#name` or `a.b.c.d`.
///
/// Softnet drops a guest's IPv6 and every non-global IPv4 destination, so a
/// private, shared, benchmark (`198.18.0.0/15`), loopback, or IPv6 resolver
/// would never answer.
pub fn parse_guest_dns(value: Option<&str>) -> Result<Vec<GuestResolver>, String> {
    let text = match value.map(str::trim) {
        None | Some("") => DEFAULT_GUEST_DNS,
        Some(text) => text,
    };
    let mut resolvers: Vec<GuestResolver> = Vec::new();
    for item in text.split(',').map(str::trim) {
        let (address, name) = match item.split_once('#') {
            Some((address, name)) => (address, Some(name)),
            None => (item, None),
        };
        let address: IpAddr = address.parse().map_err(|_| {
            format!(
                "{GUEST_DNS_VARIABLE} must list a.b.c.d#name or a.b.c.d separated by commas; got {item:?}"
            )
        })?;
        let IpAddr::V4(address) = address else {
            return Err(format!(
                "{GUEST_DNS_VARIABLE}: {address} is IPv6, which Softnet drops; use IPv4 resolvers"
            ));
        };
        if !globally_routable(address) {
            return Err(format!(
                "{GUEST_DNS_VARIABLE}: {address} is not a public address, so Softnet drops it"
            ));
        }
        let name = match name {
            None => None,
            Some(name) if valid_host_name(name) => Some(name.to_string()),
            Some(name) => {
                return Err(format!(
                    "{GUEST_DNS_VARIABLE}: {name:?} is not a host name for {address}"
                ))
            }
        };
        if !resolvers.iter().any(|existing| existing.address == address) {
            resolvers.push(GuestResolver { address, name });
        }
    }
    Ok(resolvers)
}

fn valid_host_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

/// Whether Softnet passes traffic to `address`: a public IPv4 address.
pub fn globally_routable(address: Ipv4Addr) -> bool {
    let [a, b, _, _] = address.octets();
    !(address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_unspecified()
        || address.is_broadcast()
        || address.is_documentation()
        || address.is_multicast()
        || a == 0
        || a >= 240
        || (a == 100 && (64..128).contains(&b))
        || (a == 198 && (18..20).contains(&b))
        || (a == 192 && b == 0))
}

/// The root script that makes a Linux guest resolve names over DNS over TLS
/// through systemd-resolved.
///
/// A `resolved.conf.d` drop-in makes the resolvers global with
/// `DNSOverTLS=yes` and the routing domain `~.`. The default interface loses
/// its DHCP resolver, the gateway, and stops being the default route for
/// lookups. A DHCP renewal can restore the gateway on the interface; the
/// firewall drops it, so resolved answers from the global resolvers.
/// A guest without systemd-resolved fails, because nothing else in a stock
/// image speaks DNS over TLS.
pub fn linux_resolver_script(resolvers: &[GuestResolver]) -> String {
    let list = resolvers
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "set -e\n\
if ! command -v resolvectl >/dev/null 2>&1 || ! systemctl is-active --quiet systemd-resolved; then\n\
  echo 'the guest has no running systemd-resolved, which DNS over TLS needs' >&2; exit 1\n\
fi\n\
dev=$(ip route show default | awk '{{for (i = 1; i < NF; i++) if ($i == \"dev\") {{print $(i + 1); exit}}}}')\n\
mkdir -p /etc/systemd/resolved.conf.d\n\
printf '[Resolve]\\nDNS={list}\\nDNSOverTLS=yes\\nDomains=~.\\n' > /etc/systemd/resolved.conf.d/90-vm-service-guest-dns.conf\n\
systemctl restart systemd-resolved\n\
if [ -n \"$dev\" ]; then\n\
  resolvectl dns \"$dev\" ''\n\
  resolvectl default-route \"$dev\" false 2>/dev/null || true\n\
fi\n\
resolvectl flush-caches 2>/dev/null || true\n\
resolvectl status --no-pager 2>/dev/null | sed -n '1,12p' || true\n"
    )
}

/// The guest command that receives the forwarder on standard input.
pub fn forwarder_upload_command() -> String {
    format!("cat > {FORWARDER_UPLOAD} && chmod 755 {FORWARDER_UPLOAD}")
}

/// The root script that makes a macOS guest resolve names over DNS over TLS.
///
/// macOS offers no scriptable way to point its resolver at DNS over TLS:
/// encrypted DNS comes from a configuration profile that a person approves
/// in System Settings, or from an app with a network extension. The script
/// installs the uploaded forwarder as a LaunchDaemon on `127.0.0.1:53`, waits
/// until it answers, and points the default route's network service at it.
pub fn macos_resolver_script(resolvers: &[GuestResolver]) -> String {
    let arguments = resolvers
        .iter()
        .map(|resolver| {
            format!("\t\t<string>--upstream</string>\n\t\t<string>{resolver}</string>\n")
        })
        .collect::<String>();
    format!(
        "set -e\n\
install -d -m 755 /usr/local/libexec/vm-service\n\
install -m 755 {FORWARDER_UPLOAD} {GUEST_FORWARDER}\n\
rm -f {FORWARDER_UPLOAD}\n\
plist=/Library/LaunchDaemons/{GUEST_FORWARDER_LABEL}.plist\n\
cat > \"$plist\" <<'PLIST'\n\
<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
\t<key>Label</key>\n\
\t<string>{GUEST_FORWARDER_LABEL}</string>\n\
\t<key>ProgramArguments</key>\n\
\t<array>\n\
\t\t<string>{GUEST_FORWARDER}</string>\n\
\t\t<string>--listen</string>\n\
\t\t<string>127.0.0.1:53</string>\n\
{arguments}\
\t</array>\n\
\t<key>RunAtLoad</key>\n\
\t<true/>\n\
\t<key>KeepAlive</key>\n\
\t<true/>\n\
\t<key>StandardErrorPath</key>\n\
\t<string>/var/log/vm-service-dot-forwarder.log</string>\n\
</dict>\n\
</plist>\n\
PLIST\n\
chown root:wheel \"$plist\"\n\
chmod 644 \"$plist\"\n\
/bin/launchctl bootout system/{GUEST_FORWARDER_LABEL} 2>/dev/null || true\n\
/bin/launchctl bootstrap system \"$plist\"\n\
i=0\n\
until dig +short +time=2 +tries=1 @127.0.0.1 {EGRESS_HOST} A | grep -q '^[0-9]'; do\n\
  i=$((i + 1))\n\
  if [ $i -ge 10 ]; then echo 'the DNS-over-TLS forwarder did not answer' >&2; tail -5 /var/log/vm-service-dot-forwarder.log >&2 || true; exit 1; fi\n\
  sleep 1\n\
done\n\
dev=$(route -n get default 2>/dev/null | awk '/interface:/ {{print $2; exit}}')\n\
svc=$(networksetup -listnetworkserviceorder | awk -v dev=\"$dev\" '/^\\([0-9*]+\\) / {{name=$0; sub(/^\\([0-9*]+\\) /, \"\", name)}} index($0, \"Device: \" dev \")\") {{print name; exit}}')\n\
[ -n \"$svc\" ] || {{ echo \"no network service for interface ${{dev:-?}}\" >&2; exit 1; }}\n\
networksetup -setdnsservers \"$svc\" 127.0.0.1\n\
dscacheutil -flushcache\n\
killall -HUP mDNSResponder 2>/dev/null || true\n\
networksetup -getdnsservers \"$svc\"\n"
    )
}

/// The forwarder executable to upload: `MACHINE_SERVICE_DOT_FORWARDER`, or
/// `vm-service-dot-forwarder-macos`, then `vm-service-dot-forwarder`, beside
/// the running daemon.
pub fn forwarder_path(value: Option<&str>) -> PathBuf {
    if let Some(path) = value.filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    let directory = service_layout::daemon_directory().unwrap_or_default();
    service_layout::guest_build_in(
        &directory,
        service_layout::Executable::DotForwarder,
        "macos",
    )
}

/// Wrap a root `script` for the guest account: `sudo -S` reads `password`
/// from standard input, and `sudo -n` runs without one.
///
/// Returns the remote command and its standard input.
pub fn as_root(script: &str, password: &str) -> (String, Option<Vec<u8>>) {
    if password.is_empty() {
        (format!("sudo -n sh -c {}", shell_quote(script)), None)
    } else {
        (
            format!("sudo -S -p '' sh -c {}", shell_quote(script)),
            Some(format!("{password}\n").into_bytes()),
        )
    }
}

// ------------------------------------------------------------------- firewall

/// The installed guest firewall, as this service uses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestFirewall {
    /// The table file the anchor reads and this service writes.
    pub table: PathBuf,
    /// The host port the anchor lets the guests reach, the broker's
    /// (DC-107); `None` for an anchor installed before it, or without it.
    pub broker_port: Option<u16>,
    /// The anchor's egress section's files (DC-267); `None` for an
    /// anchor installed before it.
    pub egress: Option<guest_firewall::EgressFiles>,
}

/// What an egress guest's acquisition reads from the egress section's files
/// (DC-267).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressPolicy {
    /// The section's files.
    pub files: guest_firewall::EgressFiles,
    /// The name the isolation check loads over HTTPS: the allow file's
    /// `# check:` line.
    pub check_name: String,
    /// The lab hosts' addresses, which Softnet must pass.
    pub lab: Vec<String>,
}

/// Whether `entry`, an address or a network of a pf table file, holds
/// `address`.
pub fn entry_holds(entry: &str, address: Ipv4Addr) -> bool {
    let (base, bits) = match entry.split_once('/') {
        Some((base, bits)) => (base, bits.parse::<u32>().ok()),
        None => (entry, Some(32)),
    };
    let (Ok(base), Some(bits)) = (base.parse::<Ipv4Addr>(), bits) else {
        return false;
    };
    if bits > 32 {
        return false;
    }
    let mask = if bits == 0 {
        0
    } else {
        u32::MAX << (32 - bits)
    };
    u32::from(base) & mask == u32::from(address) & mask
}

/// Check that this host can give an egress guest its fence, or say why
/// not: the installed anchor has the egress section, this service can
/// write its guests' file, and the allow file names a check name and holds
/// every guest resolver, so the guest can still resolve names (DC-16).
pub fn check_egress(
    firewall: &GuestFirewall,
    resolvers: &[GuestResolver],
) -> Result<EgressPolicy, String> {
    let install = guest_firewall::install_command(&installer_path(), &firewall.table);
    let files = firewall.egress.clone().ok_or_else(|| {
        format!(
            "guest firewall outdated: its anchor has no egress section (DC-267); reinstall it \
with: {install}"
        )
    })?;
    let directory = files.guests.parent().unwrap_or_else(|| Path::new("/"));
    if !writable(directory) || (files.guests.exists() && !writable(&files.guests)) {
        return Err(format!(
            "egress guests' table {} is not writable by this machine-service; reinstall the guest \
firewall from this account with: {install}",
            files.guests.display()
        ));
    }
    let allow = std::fs::read_to_string(&files.allow).unwrap_or_default();
    let check_name = guest_firewall::parse_check_name(&allow).ok_or_else(|| {
        format!(
            "the egress allowlist {} names no check name; lab-host writes it (LR-5)",
            files.allow.display()
        )
    })?;
    let entries = guest_firewall::parse_addresses(&allow);
    if let Some(missing) = resolvers.iter().find(|resolver| {
        !entries
            .iter()
            .any(|entry| entry_holds(entry, resolver.address))
    }) {
        return Err(format!(
            "the egress allowlist {} does not hold the guest resolver {}; lab-host writes it (LR-5)",
            files.allow.display(),
            missing.address
        ));
    }
    let lab =
        guest_firewall::parse_addresses(&std::fs::read_to_string(&files.lab).unwrap_or_default())
            .into_iter()
            .filter(|entry| guest_firewall::valid_entry(entry))
            .collect();
    Ok(EgressPolicy {
        files,
        check_name,
        lab,
    })
}

/// The anchor file the service reads: `MACHINE_SERVICE_GUEST_FIREWALL_ANCHOR`, or
/// the installed default.
pub fn anchor_file(value: Option<&str>) -> PathBuf {
    match value {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => PathBuf::from(guest_firewall::ANCHOR_FILE),
    }
}

/// The table file the service writes: `MACHINE_SERVICE_GUEST_FIREWALL_TABLE`, or
/// the installer's default, which every instance on the host shares.
pub fn table_file(value: Option<&str>) -> PathBuf {
    match value {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => PathBuf::from(guest_firewall::DEFAULT_TABLE_FILE),
    }
}

/// Check that the guest firewall is installed for `table`, or say why an
/// `internet` lease cannot be offered, with the install command.
///
/// The anchor must exist and read its table from `table`, and this service
/// must be able to write that file. Whether pf has the anchor loaded needs
/// root to see; the isolation check catches an anchor that is not in force.
pub fn check_guest_firewall(anchor: &Path, table: &Path) -> Result<GuestFirewall, String> {
    let install = guest_firewall::install_command(&installer_path(), table);
    let text = std::fs::read_to_string(anchor).map_err(|_| {
        format!(
            "guest firewall not installed: {} is missing; network internet needs the pf anchor {} \
(DC-14); install it once with: {install} (no NAT fallback)",
            anchor.display(),
            guest_firewall::ANCHOR,
        )
    })?;
    let installed = guest_firewall::parse_anchor_table(&text).ok_or_else(|| {
        format!(
            "guest firewall outdated: {} has no table of guest addresses (DC-15); reinstall it \
with: {install} (no NAT fallback)",
            anchor.display()
        )
    })?;
    if installed != table {
        return Err(format!(
            "guest firewall mismatch: the anchor reads {}, but this machine-service writes {} \
({TABLE_FILE_VARIABLE}); reinstall it with: {install} (no NAT fallback)",
            installed.display(),
            table.display()
        ));
    }
    if !writable(table) || !writable(table.parent().unwrap_or_else(|| Path::new("/"))) {
        return Err(format!(
            "guest firewall table {} is not writable by this machine-service; reinstall it from this \
account with: {install} (no NAT fallback)",
            table.display()
        ));
    }
    Ok(GuestFirewall {
        table: table.to_path_buf(),
        broker_port: guest_firewall::parse_anchor_broker_port(&text),
        egress: guest_firewall::parse_anchor_egress(&text),
    })
}

fn writable(path: &Path) -> bool {
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    // SAFETY: the path is a valid C string for the duration of the call.
    unsafe { libc::access(c_path.as_ptr(), libc::W_OK) == 0 }
}

/// The installer executable, installed beside the running daemon (IN-1).
pub fn installer_path() -> String {
    service_layout::beside_daemon(service_layout::Executable::GuestFirewall)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| guest_firewall::INSTALLER.to_string())
}

// ---------------------------------------------------------------------- probe

/// The guest command that tries a TCP connection to its default gateway on
/// `port` and prints `gateway=<address> result=<open|refused|timeout|error:...>`.
pub fn probe_command(kind: &str, port: u16) -> String {
    let timeout = PROBE_TIMEOUT_S;
    if kind == "macos" {
        return format!(
            "# {PROBE_MARKER}\n\
gw=$(route -n get default 2>/dev/null | awk '/gateway:/ {{print $2; exit}}')\n\
if [ -z \"$gw\" ]; then echo 'gateway= result=error:no-default-route'; exit 0; fi\n\
out=$(nc -v -z -G {timeout} -w {timeout} \"$gw\" {port} 2>&1); rc=$?\n\
if [ $rc -eq 0 ]; then r=open\n\
elif printf '%s' \"$out\" | grep -qi refused; then r=refused\n\
elif printf '%s' \"$out\" | grep -qi 'timed out'; then r=timeout\n\
else r=error:$rc; fi\n\
echo \"gateway=$gw result=$r\"\n"
        );
    }
    format!(
        "# {PROBE_MARKER}\n\
gw=$(ip route show default | awk '{{print $3; exit}}')\n\
if [ -z \"$gw\" ]; then echo 'gateway= result=error:no-default-route'; exit 0; fi\n\
out=$(timeout {timeout} bash -c \"exec 3<>/dev/tcp/$gw/{port}\" 2>&1); rc=$?\n\
if [ $rc -eq 0 ]; then r=open\n\
elif [ $rc -eq 124 ]; then r=timeout\n\
elif printf '%s' \"$out\" | grep -qi refused; then r=refused\n\
else r=error:$rc; fi\n\
echo \"gateway=$gw result=$r\"\n"
    )
}

/// What the guest saw when it tried the gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeResult {
    /// The connection completed.
    Open,
    /// The host answered with a reset.
    Refused,
    /// Nothing answered within the probe timeout.
    Timeout,
    /// The probe could not run; the text says why.
    Error(String),
}

/// The probe's output: the guest's default gateway and the result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeOutcome {
    /// The guest's default gateway, when it has one.
    pub gateway: Option<Ipv4Addr>,
    /// What the connection attempt did.
    pub result: ProbeResult,
}

/// Parse the probe's output. Anything unexpected is an error result.
pub fn parse_probe(output: &str) -> ProbeOutcome {
    let line = output
        .lines()
        .rev()
        .find(|line| line.starts_with("gateway="))
        .unwrap_or_default();
    let mut gateway = None;
    let mut result = ProbeResult::Error(format!("unexpected probe output: {:?}", output.trim()));
    for field in line.split_whitespace() {
        if let Some(value) = field.strip_prefix("gateway=") {
            gateway = value.parse().ok();
        } else if let Some(value) = field.strip_prefix("result=") {
            result = match value {
                "open" => ProbeResult::Open,
                "refused" => ProbeResult::Refused,
                "timeout" => ProbeResult::Timeout,
                other => ProbeResult::Error(other.to_string()),
            };
        }
    }
    ProbeOutcome { gateway, result }
}

/// Whether a probe showed the guest isolated: it timed out, with a gateway,
/// and the host's listener saw no connection.
pub fn isolated(outcome: &ProbeOutcome, accepted: bool) -> bool {
    !accepted && outcome.result == ProbeResult::Timeout && outcome.gateway.is_some()
}

/// Whether a probe that failed may pass on a retry: the host answered, which
/// a table reload still in progress explains. An error does not improve.
pub fn worth_retrying(outcome: &ProbeOutcome, accepted: bool) -> bool {
    accepted || matches!(outcome.result, ProbeResult::Open | ProbeResult::Refused)
}

/// The reason a probe failed the acquisition. It names the anchor, the
/// guest's address, the table and the install command.
pub fn probe_failure(
    outcome: &ProbeOutcome,
    accepted: bool,
    port: u16,
    guest: &str,
    table: &Path,
) -> String {
    let install = guest_firewall::install_command(&installer_path(), table);
    let gateway = outcome
        .gateway
        .map(|gateway| gateway.to_string())
        .unwrap_or_else(|| "its default gateway".to_string());
    let what = if accepted {
        "connected to".to_string()
    } else {
        match &outcome.result {
            ProbeResult::Open => "connected to".to_string(),
            ProbeResult::Refused => "was answered by".to_string(),
            ProbeResult::Timeout => "could not find".to_string(),
            ProbeResult::Error(reason) => format!("could not probe ({reason})"),
        }
    };
    format!(
        "guest isolation check failed: the guest {guest} {what} this host at {gateway}:{port} \
after {PROBE_RETRY_S} s; the pf anchor {} is not dropping the addresses in {}; install it with: \
{install} (no NAT fallback)",
        guest_firewall::ANCHOR,
        table.display()
    )
}

// --------------------------------------------------------------------- egress

/// The guest command that resolves [`EGRESS_HOST`] through the system
/// resolver and loads it over HTTPS, printing
/// `egress name=<host> address=<a.b.c.d|none> https=<status>`.
pub fn egress_command(kind: &str) -> String {
    egress_command_for(kind, EGRESS_HOST)
}

/// The egress check's guest command for `host`, a name the caller chose
/// from its own configuration, never a client's.
pub fn egress_command_for(kind: &str, host: &str) -> String {
    let host = shell_quote(host);
    let lookup = if kind == "macos" {
        format!("dscacheutil -q host -a name {host} | awk '/^ip_address:/ {{print $2; exit}}'")
    } else {
        format!("getent ahostsv4 {host} | awk 'NR == 1 {{print $1}}'")
    };
    format!(
        "# {EGRESS_MARKER}\n\
a=$({lookup})\n\
c=$(curl -sS -o /dev/null -w '%{{http_code}}' -m 20 https://{host}/ 2>/dev/null || true)\n\
echo \"egress name={host} address=${{a:-none}} https=${{c:-000}}\"\n"
    )
}

/// Decide an egress guest's denied check (DC-267): HTTPS to
/// [`EGRESS_HOST`], which no allowlist holds, must not answer at all. The
/// name may still resolve, since the guest's resolvers are allowed.
pub fn decide_denied(egress: Option<&Egress>) -> Result<(), String> {
    let Some(egress) = egress else {
        return Err(format!(
            "guest egress fence check failed: no result for {EGRESS_HOST}"
        ));
    };
    if egress.https == "000" {
        Ok(())
    } else {
        Err(format!(
            "guest egress fence check failed: https://{EGRESS_HOST}/ at {} answered {} in an egress \
guest, which the allowlist does not hold",
            egress.address, egress.https
        ))
    }
}

/// What the egress check saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Egress {
    /// The address the name resolved to, as printed.
    pub address: String,
    /// The HTTPS status, as printed.
    pub https: String,
}

/// Parse the egress check's output.
pub fn parse_egress(output: &str) -> Option<Egress> {
    let line = output
        .lines()
        .rev()
        .find(|line| line.starts_with("egress "))?;
    let mut address = None;
    let mut https = None;
    for field in line.split_whitespace() {
        if let Some(value) = field.strip_prefix("address=") {
            address = Some(value.to_string());
        } else if let Some(value) = field.strip_prefix("https=") {
            https = Some(value.to_string());
        }
    }
    Some(Egress {
        address: address?,
        https: https?,
    })
}

/// Decide the egress check: the name must resolve to a public address, and
/// HTTPS must answer with a 2xx or 3xx status.
pub fn decide_egress(egress: Option<&Egress>) -> Result<(), String> {
    decide_egress_for(EGRESS_HOST, egress)
}

/// [`decide_egress`] for the name `EGRESS_HOST` stands for.
pub fn decide_egress_for(host: &str, egress: Option<&Egress>) -> Result<(), String> {
    let Some(egress) = egress else {
        return Err(format!("guest egress check failed: no result for {host}"));
    };
    let address: Option<Ipv4Addr> = egress.address.parse().ok();
    match address {
        Some(address) if globally_routable(address) => {}
        _ => {
            return Err(format!(
                "guest egress check failed: {host} resolved to {} in the guest, not a public \
address; the guest's DNS over TLS is not answering",
                egress.address
            ))
        }
    }
    match egress.https.parse::<u16>() {
        Ok(status) if (200..400).contains(&status) => Ok(()),
        _ => Err(format!(
            "guest egress check failed: https://{host}/ at {} answered {} in the guest",
            egress.address, egress.https
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_allowlist_entry_holds_its_addresses() {
        let address = |text: &str| text.parse::<Ipv4Addr>().unwrap();
        assert!(entry_holds("1.1.1.1", address("1.1.1.1")));
        assert!(!entry_holds("1.1.1.1", address("1.0.0.1")));
        assert!(entry_holds("140.82.112.0/20", address("140.82.127.255")));
        assert!(!entry_holds("140.82.112.0/20", address("140.82.128.0")));
        assert!(entry_holds("0.0.0.0/0", address("8.8.8.8")));
        assert!(!entry_holds("1.1.1.1/33", address("1.1.1.1")));
        assert!(!entry_holds("example.com", address("1.1.1.1")));
    }

    #[test]
    fn the_egress_check_loads_the_name_it_is_given() {
        let command = egress_command_for("linux", "api.github.com");
        assert!(command.contains(EGRESS_MARKER));
        assert!(command.contains("https://api.github.com/"));
        assert!(decide_egress_for(
            "api.github.com",
            Some(&Egress {
                address: "140.82.112.6".into(),
                https: "200".into()
            })
        )
        .is_ok());
        assert!(decide_denied(Some(&Egress {
            address: "104.20.23.154".into(),
            https: "000".into()
        }))
        .is_ok());
        let open = decide_denied(Some(&Egress {
            address: "104.20.23.154".into(),
            https: "200".into(),
        }));
        assert!(open.unwrap_err().contains("fence check failed"));
        assert!(decide_denied(None).is_err());
    }

    fn gateway() -> Option<Ipv4Addr> {
        Some("192.168.2.1".parse().unwrap())
    }

    fn resolvers() -> Vec<GuestResolver> {
        parse_guest_dns(None).unwrap()
    }

    #[test]
    fn guest_dns_defaults_to_two_named_public_resolvers() {
        let defaults = resolvers();
        assert_eq!(
            defaults.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec!["1.1.1.1#cloudflare-dns.com", "8.8.8.8#dns.google"]
        );
        assert_eq!(parse_guest_dns(Some("")).unwrap(), defaults);
        assert_eq!(
            parse_guest_dns(Some(" 9.9.9.9#dns.quad9.net , 9.9.9.9,149.112.112.112"))
                .unwrap()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec!["9.9.9.9#dns.quad9.net", "149.112.112.112"]
        );
    }

    #[test]
    fn guest_dns_refuses_what_softnet_drops_or_tls_cannot_check() {
        for value in [
            "192.168.2.1",
            "10.0.0.53",
            "172.16.0.1",
            "127.0.0.53",
            "198.18.0.2",
            "100.100.100.100",
            "169.254.0.1",
            "0.0.0.0",
            "2606:4700:4700::1111",
            "1.1.1.1,dns.google",
            "1.1.1.1,",
            "1.1.1.1#",
            "1.1.1.1#bad name",
        ] {
            let error = parse_guest_dns(Some(value)).unwrap_err();
            assert!(error.starts_with(GUEST_DNS_VARIABLE), "{value}: {error}");
        }
    }

    #[test]
    fn the_linux_resolver_uses_dns_over_tls_in_systemd_resolved() {
        let script = linux_resolver_script(&resolvers());
        assert!(script.contains(
            "printf '[Resolve]\\nDNS=1.1.1.1#cloudflare-dns.com 8.8.8.8#dns.google\\nDNSOverTLS=yes\\nDomains=~.\\n' > /etc/systemd/resolved.conf.d/90-vm-service-guest-dns.conf"
        ));
        assert!(script.contains("systemctl restart systemd-resolved"));
        assert!(script.contains("resolvectl dns \"$dev\" ''"));
        assert!(script.contains("resolvectl default-route \"$dev\" false"));
        assert!(script.contains("which DNS over TLS needs' >&2; exit 1"));
        assert!(!script.contains("/etc/resolv.conf"));
    }

    #[test]
    fn the_macos_resolver_installs_the_forwarder_and_points_dns_at_it() {
        let script = macos_resolver_script(&resolvers());
        assert!(script.contains(
            "install -m 755 /tmp/vm-service-dot-forwarder.upload /usr/local/libexec/vm-service/vm-service-dot-forwarder"
        ));
        assert!(script.contains("<string>127.0.0.1:53</string>"));
        assert!(script.contains(
            "\t\t<string>--upstream</string>\n\t\t<string>1.1.1.1#cloudflare-dns.com</string>\n\t\t<string>--upstream</string>\n\t\t<string>8.8.8.8#dns.google</string>\n"
        ));
        assert!(script.contains("/bin/launchctl bootstrap system \"$plist\""));
        assert!(script.contains("dig +short +time=2 +tries=1 @127.0.0.1 example.com A"));
        assert!(script.contains("networksetup -setdnsservers \"$svc\" 127.0.0.1\n"));
        assert_eq!(
            forwarder_upload_command(),
            "cat > /tmp/vm-service-dot-forwarder.upload && chmod 755 /tmp/vm-service-dot-forwarder.upload"
        );
    }

    /// The generated scripts are valid `sh`, and the guest plist is valid.
    #[test]
    fn the_scripts_parse_as_shell() {
        for script in [
            linux_resolver_script(&resolvers()),
            macos_resolver_script(&resolvers()),
            probe_command("linux", 40000),
            probe_command("macos", 40000),
            egress_command("linux"),
            egress_command("macos"),
        ] {
            let status = std::process::Command::new("/bin/sh")
                .args(["-n", "-c", &script])
                .status()
                .unwrap();
            assert!(status.success(), "sh -n rejected:\n{script}");
        }
        if Path::new("/usr/bin/plutil").exists() {
            let script = macos_resolver_script(&resolvers());
            let start = script.find("<?xml").unwrap();
            let end = script.find("</plist>").unwrap() + "</plist>".len();
            let file = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(file.path(), &script[start..end]).unwrap();
            let lint = std::process::Command::new("/usr/bin/plutil")
                .arg("-lint")
                .arg(file.path())
                .output()
                .unwrap();
            assert!(
                lint.status.success(),
                "{}",
                String::from_utf8_lossy(&lint.stdout)
            );
        }
    }

    #[test]
    fn root_scripts_use_sudo_with_or_without_a_password() {
        let (command, stdin) = as_root("echo hi", "admin");
        assert_eq!(command, "sudo -S -p '' sh -c 'echo hi'");
        assert_eq!(stdin, Some(b"admin\n".to_vec()));
        let (command, stdin) = as_root("echo hi", "");
        assert_eq!(command, "sudo -n sh -c 'echo hi'");
        assert!(stdin.is_none());
    }

    #[test]
    fn probe_commands_target_the_default_gateway() {
        let linux = probe_command("linux", 40001);
        assert!(linux.contains(PROBE_MARKER));
        assert!(linux.contains("ip route show default"));
        assert!(linux.contains("timeout 3 bash -c \"exec 3<>/dev/tcp/$gw/40001\""));
        let macos = probe_command("macos", 40001);
        assert!(macos.contains("route -n get default"));
        assert!(macos.contains("nc -v -z -G 3 -w 3 \"$gw\" 40001"));
    }

    #[test]
    fn probe_output_is_parsed() {
        assert_eq!(
            parse_probe("noise\ngateway=192.168.2.1 result=timeout\n"),
            ProbeOutcome {
                gateway: gateway(),
                result: ProbeResult::Timeout
            }
        );
        assert_eq!(
            parse_probe("gateway=192.168.2.1 result=open").result,
            ProbeResult::Open
        );
        assert_eq!(
            parse_probe("gateway=192.168.2.1 result=refused").result,
            ProbeResult::Refused
        );
        assert_eq!(
            parse_probe("gateway= result=error:no-default-route"),
            ProbeOutcome {
                gateway: None,
                result: ProbeResult::Error("error:no-default-route".to_string())
            }
        );
        assert!(matches!(
            parse_probe("ran: something").result,
            ProbeResult::Error(_)
        ));
    }

    #[test]
    fn only_a_silent_gateway_is_isolated_and_only_an_answer_is_retried() {
        let timeout = ProbeOutcome {
            gateway: gateway(),
            result: ProbeResult::Timeout,
        };
        let open = ProbeOutcome {
            gateway: gateway(),
            result: ProbeResult::Open,
        };
        let refused = ProbeOutcome {
            gateway: gateway(),
            result: ProbeResult::Refused,
        };
        let error = ProbeOutcome {
            gateway: gateway(),
            result: ProbeResult::Error("x".into()),
        };
        let no_gateway = ProbeOutcome {
            gateway: None,
            result: ProbeResult::Timeout,
        };
        assert!(isolated(&timeout, false));
        assert!(!isolated(&timeout, true));
        assert!(!isolated(&open, false));
        assert!(!isolated(&refused, false));
        assert!(!isolated(&error, false));
        assert!(!isolated(&no_gateway, false));
        assert!(worth_retrying(&open, false));
        assert!(worth_retrying(&refused, false));
        assert!(worth_retrying(&timeout, true));
        assert!(!worth_retrying(&error, false));
        assert!(!worth_retrying(&no_gateway, false));
    }

    #[test]
    fn a_probe_failure_names_the_guest_the_anchor_and_the_install_command() {
        let open = ProbeOutcome {
            gateway: gateway(),
            result: ProbeResult::Open,
        };
        let message = probe_failure(
            &open,
            false,
            40000,
            "192.168.2.9",
            Path::new(guest_firewall::DEFAULT_TABLE_FILE),
        );
        assert!(message.starts_with("guest isolation check failed: the guest 192.168.2.9 connected to this host at 192.168.2.1:40000 after 15 s"), "{message}");
        assert!(message.contains("com.apple/250.vm-service"), "{message}");
        assert!(
            message.contains("/var/db/com.wezzard.vm-service/internet-guests"),
            "{message}"
        );
        assert!(
            message.contains("vm-service-guest-firewall install (no NAT fallback)"),
            "{message}"
        );
    }

    #[test]
    fn egress_commands_resolve_through_the_system_resolver_and_load_https() {
        let linux = egress_command("linux");
        assert!(linux.contains(EGRESS_MARKER));
        assert!(linux.contains("getent ahostsv4 example.com"));
        assert!(
            linux.contains("curl -sS -o /dev/null -w '%{http_code}' -m 20 https://example.com/")
        );
        assert!(egress_command("macos").contains("dscacheutil -q host -a name example.com"));
    }

    #[test]
    fn egress_needs_a_public_address_and_a_good_status() {
        let parse = |text: &str| parse_egress(text);
        assert!(decide_egress(
            parse("egress name=example.com address=104.20.23.154 https=200").as_ref()
        )
        .is_ok());
        assert!(decide_egress(
            parse("egress name=example.com address=104.20.23.154 https=301").as_ref()
        )
        .is_ok());
        for (text, needle) in [
            (
                "egress name=example.com address=198.18.35.15 https=000",
                "not a public address",
            ),
            (
                "egress name=example.com address=none https=000",
                "not a public address",
            ),
            (
                "egress name=example.com address=192.168.2.1 https=200",
                "not a public address",
            ),
            (
                "egress name=example.com address=104.20.23.154 https=000",
                "answered 000",
            ),
            (
                "egress name=example.com address=104.20.23.154 https=503",
                "answered 503",
            ),
            ("ran: something", "no result"),
        ] {
            let error = decide_egress(parse(text).as_ref()).unwrap_err();
            assert!(
                error.starts_with("guest egress check failed"),
                "{text}: {error}"
            );
            assert!(error.contains(needle), "{text}: {error}");
        }
    }

    #[test]
    fn the_guest_firewall_must_read_the_table_this_service_writes() {
        let directory = tempfile::tempdir().unwrap();
        let table = directory.path().join("internet-guests");
        std::fs::write(&table, "").unwrap();
        let anchor = directory.path().join("anchor");

        let error = check_guest_firewall(&anchor, &table).unwrap_err();
        assert!(error.starts_with("guest firewall not installed"), "{error}");
        assert!(
            error.contains("vm-service-guest-firewall install --table"),
            "{error}"
        );

        std::fs::write(&anchor, "softnet_guests = \"192.168.2.0/24\"\n").unwrap();
        assert!(check_guest_firewall(&anchor, &table)
            .unwrap_err()
            .starts_with("guest firewall outdated"));

        std::fs::write(
            &anchor,
            guest_firewall::anchor_rules(Path::new("/elsewhere/guests"), Some(6241)),
        )
        .unwrap();
        let error = check_guest_firewall(&anchor, &table).unwrap_err();
        assert!(
            error.starts_with("guest firewall mismatch: the anchor reads /elsewhere/guests"),
            "{error}"
        );

        std::fs::write(&anchor, guest_firewall::anchor_rules(&table, None)).unwrap();
        assert_eq!(
            check_guest_firewall(&anchor, &table).unwrap(),
            GuestFirewall {
                table: table.clone(),
                broker_port: None,
                egress: Some(guest_firewall::EgressFiles::beside(&table)),
            }
        );
        std::fs::write(&anchor, guest_firewall::anchor_rules(&table, Some(6241))).unwrap();
        assert_eq!(
            check_guest_firewall(&anchor, &table).unwrap().broker_port,
            Some(6241)
        );

        std::fs::remove_file(&table).unwrap();
        assert!(check_guest_firewall(&anchor, &table)
            .unwrap_err()
            .contains("is not writable by this machine-service"));
    }

    #[test]
    fn paths_can_be_overridden() {
        assert_eq!(
            anchor_file(None),
            PathBuf::from("/etc/pf.anchors/com.wezzard.vm-service")
        );
        assert_eq!(anchor_file(Some("/tmp/a")), PathBuf::from("/tmp/a"));
        assert_eq!(
            table_file(None),
            PathBuf::from("/var/db/com.wezzard.vm-service/internet-guests")
        );
        assert_eq!(
            table_file(Some("")),
            PathBuf::from("/var/db/com.wezzard.vm-service/internet-guests")
        );
        assert_eq!(table_file(Some("/tmp/t")), PathBuf::from("/tmp/t"));
        assert_eq!(forwarder_path(Some("/tmp/f")), PathBuf::from("/tmp/f"));
    }
}
