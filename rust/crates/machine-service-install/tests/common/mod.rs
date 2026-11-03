//! Shared fixture for the installer integration tests.
//!
//! The port of `tests/unit/test_console_install.py` runs the built
//! `machine-service-install` binary against an isolated `HOME`, a fake Tart, and a
//! `launchctl` stub, exactly as the Python test runs the shell script. The
//! stub reaches the installer through the test build's seam
//! (`machine_service_core::launchd`), never `PATH`, and a test build without
//! it refuses to run any `launchctl` (DC-242).
//!
//! The isolated `HOME` lies under Cargo's own temporary folder for the
//! tests, never under the account's home, and the installer runs as an
//! isolated test child with a tool table of its own (DC-1xx-ISO-1,
//! DC-1xx-ISO-4, DC-1xx-ISO-7).

#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

/// A `launchctl` stand-in that records calls and keeps the documented sentinel
/// files. It mirrors the Python stub in `test_console_install.py`, and keeps
/// one job sentinel per label: `loaded-job` for machine-service and
/// `loaded-job-<label>` for any other agent, such as the old vm-service.
pub const LAUNCHCTL_STUB: &str = r#"#!/bin/sh
home="$HOME"
printf '%s\n' "$*" >> "$home/launchctl.log"
op="$1"
case "$op" in
  print|bootout) label="${2##*/}" ;;
  bootstrap) label="$(basename "${3:-}" .plist)" ;;
  *) label="$(basename "${2:-}" .plist)" ;;
esac
if [ "$label" = com.wezzard.machine-service ]; then job="$home/loaded-job"; else job="$home/loaded-job-$label"; fi
case "$op" in
  print)
    if [ -e "$home/print-fails" ]; then
      echo 'permission denied' >&2
      exit 1
    fi
    if [ -e "$home/user-domain" ]; then expected="user/"; else expected="gui/"; fi
    target="${2:-}"
    case "$target" in
      gui/[0-9]*/*) ;;
      gui/[0-9]*)
        if [ -e "$home/no-gui" ]; then echo "Could not find domain" >&2; exit 113; fi
        echo "domain = gui"; exit 0 ;;
    esac
    case "$target" in
      "$expected"*) ;;
      *) echo "Could not find service \"$label\" in domain" >&2; exit 113 ;;
    esac
    if [ ! -e "$job" ]; then
      echo "Could not find service \"$label\" in domain" >&2
      exit 113
    fi
    echo "path = $home/Library/LaunchAgents/$label.plist"
    content="$(cat "$job")"
    if [ -n "$content" ]; then echo "pid = $content"; fi
    ;;
  unload)
    if [ -e "$home/unload-fails" ]; then exit 1; fi
    if [ ! -e "$home/linger-job" ]; then rm -f "$job"; fi
    ;;
  load)
    : > "$job"
    ;;
  bootstrap)
    if [ -e "$home/bootstrap-fails" ]; then echo "Bootstrap failed: 5" >&2; exit 5; fi
    cp "$3" "$home/bootstrapped-$label.plist"
    : > "$job"
    ;;
  bootout)
    rm -f "$job"
    ;;
  *)
    exit 99
    ;;
esac
exit 0
"#;

/// A relay agent release in `folder`, as `scripts/build-relay-agent.sh`
/// makes it without a Developer ID identity: the macOS archive holds a copy
/// of a system executable signed ad hoc with the release identifier
/// (DC-135), the Linux one a stand-in, and the version is the pre-release
/// an ad hoc build names itself (DC-224).
pub fn write_release(folder: &Path, signed: bool) {
    write_release_of(folder, "9.9.9-adhoc", signed);
}

/// [`write_release`] with the version `version`.
pub fn write_release_of(folder: &Path, version: &str, signed: bool) {
    use service_layout::release;
    let _ = fs::remove_dir_all(folder);
    fs::create_dir_all(folder).unwrap();
    let mut sums = String::new();
    for kind in ["macos", "linux"] {
        let platform = release::platform_for(kind);
        let stage = tempfile::tempdir().unwrap();
        let member = stage.path().join(release::MEMBER);
        if kind == "macos" {
            fs::copy("/usr/bin/true", &member).unwrap();
            if signed {
                let status = Command::new("/usr/bin/codesign")
                    .args([
                        "--force",
                        "--sign",
                        "-",
                        "--identifier",
                        "com.wezzard.relay-agent",
                    ])
                    .arg(&member)
                    .output()
                    .unwrap()
                    .status;
                assert!(status.success());
            }
        } else {
            fs::write(&member, "ELF stand-in\n").unwrap();
        }
        let file = format!("relay-agent-{version}-{platform}.tar.gz");
        let status = Command::new("/usr/bin/tar")
            .arg("-C")
            .arg(stage.path())
            .arg("-czf")
            .arg(folder.join(&file))
            .arg(release::MEMBER)
            .status()
            .unwrap();
        assert!(status.success());
        let bytes = fs::read(folder.join(&file)).unwrap();
        sums.push_str(&format!("{}  {file}\n", release::sha256_hex(&bytes)));
    }
    fs::write(folder.join(release::SUMS), sums).unwrap();
}

/// The macOS executable a release in `folder` holds.
pub fn release_executable(folder: &Path) -> Vec<u8> {
    service_layout::release::Release::open(folder)
        .unwrap()
        .executable(&service_layout::release::platform_for("macos"))
        .unwrap()
}

pub fn write_exec(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

pub struct Harness {
    pub tmp: TempDir,
    pub home: PathBuf,
    pub bin: PathBuf,
    pub log: PathBuf,
    pub tartlog: PathBuf,
    pub tart: PathBuf,
    pub plist: PathBuf,
    pub config: PathBuf,
    pub viewer: PathBuf,
    pub installer: PathBuf,
    /// The `launchctl` stub, which the installer is given through the test
    /// build's seam and never finds on `PATH`.
    pub launchctl: PathBuf,
    pub extra_env: Vec<(String, String)>,
    /// The fake Tart is neither on `PATH` nor named by `TART` (AC-38).
    pub no_tart: bool,
}

impl Harness {
    pub fn new() -> Self {
        // The build's own temporary folder, resolved, since the installer
        // refuses a linked parent folder (DC-1xx-ISO-7).
        let parent = Path::new(env!("CARGO_TARGET_TMPDIR"));
        fs::create_dir_all(parent).unwrap();
        let tmp = tempfile::Builder::new()
            .prefix("machine-service-install-test-")
            .tempdir_in(parent.canonicalize().unwrap())
            .unwrap();
        let home = tmp.path().to_path_buf();
        let bin = home.join("bin");
        fs::create_dir_all(&bin).unwrap();

        let log = home.join("launchctl.log");
        let tartlog = home.join("tart.log");
        let tart = bin.join("tart");
        write_exec(
            &tart,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$HOME/tart.log\"\n[ \"$1\" = --version ] || exit 99\nprintf '2.32.1\\n'\n",
        );
        // The stub lies off `PATH`: the installer is given it through the
        // test build's seam only (DC-242).
        let launchctl = home.join("launchd/launchctl");
        fs::create_dir_all(launchctl.parent().unwrap()).unwrap();
        write_exec(&launchctl, LAUNCHCTL_STUB);

        let installer = bin.join("machine-service-install");
        fs::copy(env!("CARGO_BIN_EXE_machine-service-install"), &installer).unwrap();
        fs::set_permissions(&installer, fs::Permissions::from_mode(0o755)).unwrap();

        // Every executable of the installation layout, each with its own
        // content so a copy can be told from its neighbours (IN-1).
        for name in service_layout::required(true) {
            if bin.join(&name).exists() {
                continue;
            }
            write_exec(&bin.join(&name), &format!("#!/bin/sh\n# {name}\nexit 0\n"));
        }
        write_release(&bin.join(service_layout::release::FOLDER), true);
        let viewer = bin.join("vncviewer");
        write_exec(&viewer, "#!/bin/sh\nexit 99\n");

        let config = home.join("console & <trusted>.json");
        let body = format!(
            "{{\"schemaVersion\":1,\"enabled\":true,\"linux_viewer\":\"{}\"}}",
            viewer.display()
        );
        fs::write(&config, body).unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();

        let plist = home.join("Library/LaunchAgents/com.wezzard.machine-service.plist");

        Self {
            tmp,
            home,
            bin,
            log,
            tartlog,
            tart,
            plist,
            config,
            viewer,
            installer,
            launchctl,
            extra_env: Vec::new(),
            no_tart: false,
        }
    }

    /// Make this a Mac without Tart: no `tart` on `PATH` and no `TART`.
    ///
    /// The installer reads `PATH` from the LaunchAgent it writes, whose
    /// default includes `/opt/homebrew/bin`, where a developer Mac may have
    /// Tart; an installed agent whose `PATH` names only system folders
    /// stands in for a Mac without Tart there.
    pub fn remove_tart(&mut self) {
        let _ = fs::remove_file(&self.tart);
        self.no_tart = true;
        let value = serde_json::json!({
            "EnvironmentVariables": {"PATH": format!("{}:/usr/bin:/bin", self.bin.display())},
        });
        fs::create_dir_all(self.plist.parent().unwrap()).unwrap();
        fs::write(&self.plist, machine_service_install::plist::dumps(&value)).unwrap();
    }

    /// Remove the virtual-machine helpers from the staging folder.
    pub fn remove_vm_helpers(&self) {
        let core = service_layout::required(false);
        for name in service_layout::required(true) {
            if !core.contains(&name) {
                fs::remove_file(self.bin.join(name)).unwrap();
            }
        }
    }

    pub fn set_env(&mut self, key: &str, value: impl Into<String>) {
        let value = value.into();
        self.extra_env.retain(|(existing, _)| existing != key);
        self.extra_env.push((key.to_string(), value));
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.run_given(args, true)
    }

    /// Run the installer named no `launchctl` through the test build's seam
    /// (DC-242).
    pub fn run_without_launchctl(&self, args: &[&str]) -> Output {
        self.run_given(args, false)
    }

    /// The installer's tool table (DC-1xx-ISO-4): the read-only system
    /// tools and the fake Tart, when it has one.
    pub fn tools(&self) -> PathBuf {
        use service_layout::test_build::{link_system_tools, READ_ONLY_TOOLS};
        let tools = self.home.join("tools");
        let _ = fs::remove_dir_all(&tools);
        link_system_tools(&tools, &READ_ONLY_TOOLS).unwrap();
        if self.tart.exists() {
            std::os::unix::fs::symlink(&self.tart, tools.join("tart")).unwrap();
        }
        tools
    }

    fn run_given(&self, args: &[&str], named: bool) -> Output {
        let mut command = service_layout::test_build::harness::command(&self.installer);
        command.args(args);
        command.env("HOME", &self.home);
        command.env(service_layout::tools::TEST_VARIABLE, self.tools());
        command.env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()));
        if !self.no_tart {
            command.env("TART", &self.tart);
        }
        command.env("PYTHONDONTWRITEBYTECODE", "1");
        if named {
            command.env(
                machine_service_core::launchd::TEST_VARIABLE,
                &self.launchctl,
            );
        }
        for (key, value) in &self.extra_env {
            command.env(key, value);
        }
        command.output().unwrap()
    }

    pub fn assert_refused(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            !output.status.success(),
            "expected a refusal for {args:?}, got success: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    pub fn saved(&self) -> serde_json::Value {
        let data = fs::read(&self.plist).unwrap();
        machine_service_install::plist::parse(&data).unwrap()
    }

    pub fn save_old(&self, environment: &[(&str, &str)]) {
        let mut env = serde_json::Map::new();
        for (key, value) in environment {
            env.insert(
                (*key).to_string(),
                serde_json::Value::String((*value).to_string()),
            );
        }
        let value = serde_json::json!({
            "EnvironmentVariables": serde_json::Value::Object(env),
            "ProgramArguments": ["/usr/bin/python3", "/old/path/vm-service"],
        });
        fs::create_dir_all(self.plist.parent().unwrap()).unwrap();
        fs::write(&self.plist, machine_service_install::plist::dumps(&value)).unwrap();
        fs::write(self.home.join("loaded-job"), "").unwrap();
    }

    /// Install an old vm-service agent with `environment`, running from the
    /// old default install folder, and mark its job loaded.
    pub fn save_legacy(&self, environment: &[(&str, &str)]) -> PathBuf {
        let mut env = serde_json::Map::new();
        for (key, value) in environment {
            env.insert(
                (*key).to_string(),
                serde_json::Value::String((*value).to_string()),
            );
        }
        let program = self.home.join(".local/libexec/vm-service/vm-service");
        let value = serde_json::json!({
            "Label": "com.wezzard.vm-service",
            "EnvironmentVariables": serde_json::Value::Object(env),
            "ProgramArguments": [program.to_str().unwrap()],
            "RunAtLoad": true,
            "KeepAlive": true,
            "StandardOutPath": "/tmp/com.wezzard.vm-service.out",
            "StandardErrorPath": "/tmp/com.wezzard.vm-service.err",
        });
        let plist = self
            .home
            .join("Library/LaunchAgents/com.wezzard.vm-service.plist");
        fs::create_dir_all(plist.parent().unwrap()).unwrap();
        fs::write(&plist, machine_service_install::plist::dumps(&value)).unwrap();
        fs::write(self.home.join("loaded-job-com.wezzard.vm-service"), "").unwrap();
        plist
    }

    pub fn config_path(&self) -> String {
        self.config.to_string_lossy().into_owned()
    }

    pub fn home_str(&self) -> &str {
        self.home.to_str().unwrap()
    }
}
