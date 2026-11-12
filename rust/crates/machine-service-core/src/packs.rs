//! Credential env-pack handling, mirroring the Python `pack_dir`, `inject_pack`,
//! and `protect_key_material`.

use std::path::{Path, PathBuf};

use crate::config::{log_to_file, Config};
use crate::error::{OpError, OpResult};
use crate::host::Host;
use crate::ssh::shell_quote;

/// The default credential pack name: the agent pack, for VMs of agent runs.
pub const DEFAULT_PACK: &str = "default";

/// The pack of the lab's ephemeral runner VMs: a separate folder, empty
/// unless a CI need is added on purpose, and never the agent pack (DC-283).
pub const CI_PACK: &str = "ci";

/// Whether `name` is a pack's plain folder name: letters, digits, `.`, `_`
/// and `-`, not starting with `.`, so it never names a folder outside the
/// packs' folder.
pub fn valid_pack_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// Resolve an env pack name to its directory, or `None` for `none`.
pub fn pack_dir(home: &Path, name: &str) -> OpResult<Option<PathBuf>> {
    pack_dir_in(&home.join(".config/vm-credentials"), name)
}

/// Resolve an env pack name to its directory under `packs`, or `None` for
/// `none`. A named pack that is not there is a refusal, never another pack:
/// a request for `ci` on a host without it is refused, not served with
/// `default` (DC-283).
pub fn pack_dir_in(packs: &Path, name: &str) -> OpResult<Option<PathBuf>> {
    if name.is_empty() || name == "none" {
        return Ok(None);
    }
    if !valid_pack_name(name) {
        return Err(OpError::new(format!(
            "credential pack name {name:?} is not a plain folder name"
        )));
    }
    let path = packs.join(name);
    if !path.is_dir() {
        return Err(OpError::new(format!(
            "credential pack not found: {}; no other pack is used in its place",
            path.display()
        )));
    }
    Ok(Some(path))
}

/// Refuse to export or overwrite host lease secrets through the transfer API.
pub fn protect_key_material(state_dir: &Path, path: &Path, inspect_tree: bool) -> OpResult<()> {
    let protected = state_dir
        .join("ssh")
        .canonicalize()
        .unwrap_or_else(|_| state_dir.join("ssh"));
    let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if target == protected
        || protected.starts_with(&target) && target != protected
        || target.starts_with(&protected)
    {
        return Err(OpError::new(
            "Transfer path overlaps private lease SSH material",
        ));
    }
    if inspect_tree && target.is_dir() && contains_symlink(&target) {
        return Err(OpError::new("Recursive upload cannot contain symlinks"));
    }
    Ok(())
}

fn contains_symlink(root: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            return true;
        }
        if metadata.is_dir() && contains_symlink(&path) {
            return true;
        }
    }
    false
}

/// Replicate the host credential injection into a running VM.
///
/// Returns `false` for a retryable injection failure, and raises only for a
/// symlink in the pack, matching the Python behavior.
pub fn inject_pack(
    host: &dyn Host,
    config: &Config,
    ip: &str,
    cfg: &serde_json::Value,
    pack: &Path,
    key_dir: &Path,
) -> OpResult<bool> {
    let ssh_user = cfg
        .get("ssh_user")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let kind = cfg
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("linux");
    let mut files: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(pack) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            if path.is_file() {
                files.push(path);
            }
        }
    }
    files.sort();

    let prepared = host.ssh(
        ip,
        &ssh_user,
        key_dir,
        "rm -rf /tmp/pack && mkdir -m 700 /tmp/pack",
        None,
        30,
    );
    if prepared.as_ref().map(|(rc, _)| *rc) != Some(0) {
        return Ok(false);
    }
    for file in &files {
        protect_key_material(&config.state_dir, file, false)?;
        if std::fs::symlink_metadata(file)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(OpError::new("Env pack files cannot be symlinks"));
        }
        let destination = format!("{ssh_user}@{ip}:/tmp/pack/");
        let result = host.scp(
            ip,
            &ssh_user,
            key_dir,
            &file.to_string_lossy(),
            &destination,
            120,
        );
        if result.as_ref().map(|(rc, _)| *rc) != Some(0) {
            log_to_file(
                &config.log_file,
                &format!(
                    "pack scp failed for {}: {:?}",
                    file.file_name().unwrap_or_default().to_string_lossy(),
                    result
                ),
            );
            return Ok(false);
        }
    }
    if files.is_empty() {
        log_to_file(&config.log_file, "pack has no files — injecting nothing");
    }
    let has_env = files.iter().any(|file| {
        file.file_name()
            .map(|name| name == "env.extra")
            .unwrap_or(false)
    });
    let script = INJECT_SCRIPT;
    let shell = if kind == "macos" { "zsh -s" } else { "bash -s" };
    let out = host.ssh(
        ip,
        &ssh_user,
        key_dir,
        shell,
        Some(script.as_bytes().to_vec()),
        180,
    );
    if out.as_ref().map(|(rc, _)| *rc) != Some(0) {
        log_to_file(&config.log_file, &format!("pack script failed: {out:?}"));
        return Ok(false);
    }
    if has_env {
        let check = host.ssh(
            ip,
            &ssh_user,
            key_dir,
            "test -s ~/.config/zsh/secrets.zsh && echo OK",
            None,
            30,
        );
        let ok = check
            .as_ref()
            .map(|(rc, text)| *rc == 0 && text.contains("OK"))
            .unwrap_or(false);
        if !ok {
            log_to_file(
                &config.log_file,
                &format!("pack verification failed: secrets.zsh empty or missing ({check:?})"),
            );
            return Ok(false);
        }
    }
    Ok(true)
}

/// Remote command that removes a temporary pack directory.
pub fn pack_cleanup_command() -> String {
    format!("rm -rf -- {}", shell_quote("/tmp/pack"))
}

/// The guest-side injection script, byte-identical to the Python literal.
const INJECT_SCRIPT: &str = r#"set -e -u
export PATH="/opt/homebrew/bin:$HOME/.local/bin:$PATH"
cd /tmp/pack
mkdir -p ~/.config/zsh
: > /tmp/secrets.zsh
if [[ -f env.extra ]]; then cat env.extra >> /tmp/secrets.zsh; fi
if [[ -s /tmp/secrets.zsh ]]; then install -m 600 /tmp/secrets.zsh ~/.config/zsh/secrets.zsh; fi
rm -f /tmp/secrets.zsh
if [[ -s ~/.config/zsh/secrets.zsh ]] && command -v zsh >/dev/null; then
  line='[ -f "$HOME/.config/zsh/secrets.zsh" ] && source "$HOME/.config/zsh/secrets.zsh"'
  grep -qxF "$line" ~/.zshenv 2>/dev/null || printf '%s\n' "$line" >> ~/.zshenv
fi
if [[ -f git-identity ]]; then
  name=$(sed 's/ <.*//' git-identity); email=$(sed 's/.*<\(.*\)>.*/\1/' git-identity)
  git config --global user.name "$name"; git config --global user.email "$email"
fi
if [[ -f home.tgz ]]; then tar -xzf home.tgz -C "$HOME"; fi
cd /; rm -rf /tmp/pack
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_named_pack_is_refused_and_never_replaced_by_default() {
        let folder = tempfile::tempdir().unwrap();
        let packs = folder.path();
        std::fs::create_dir_all(packs.join(DEFAULT_PACK)).unwrap();
        let error = pack_dir_in(packs, CI_PACK).unwrap_err().to_string();
        assert!(error.contains("credential pack not found"), "{error}");
        assert!(error.contains("no other pack"), "{error}");
        std::fs::create_dir_all(packs.join(CI_PACK)).unwrap();
        assert_eq!(
            pack_dir_in(packs, CI_PACK).unwrap(),
            Some(packs.join(CI_PACK))
        );
        assert_eq!(pack_dir_in(packs, "none").unwrap(), None);
        assert_eq!(pack_dir_in(packs, "").unwrap(), None);
    }

    #[test]
    fn a_pack_name_never_leaves_the_packs_folder() {
        let folder = tempfile::tempdir().unwrap();
        let packs = folder.path().join("vm-credentials");
        std::fs::create_dir_all(packs.join(DEFAULT_PACK)).unwrap();
        for name in [
            "../vm-credentials/default",
            "/etc",
            "..",
            ".hidden",
            "a/b",
            "ci ",
        ] {
            let error = pack_dir_in(&packs, name).unwrap_err().to_string();
            assert!(error.contains("not a plain folder name"), "{name}: {error}");
        }
        for name in ["default", "ci", "agent_2", "v1.2-x"] {
            assert!(valid_pack_name(name), "{name}");
        }
    }
}
