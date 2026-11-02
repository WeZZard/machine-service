//! The wire form of a byte-carrying transfer (`docs/arch/federation.md`
//! FD-4; DC-70, DC-71), shared by the instance and `machctl`.
//!
//! A file travels as its own bytes with `Content-Type:
//! application/octet-stream`. A directory travels as a tar stream of its
//! contents with `Content-Type: application/x-tar`: regular files and
//! directories only, paths relative to the directory, so the receiver
//! recreates the directory under the name it chooses. `X-Content-SHA256`
//! carries the SHA-256 of the body, in lowercase hex, and a receiver keeps
//! nothing whose digest differs.
//!
//! A tar stream that names an absolute path, a `..` component, a link or a
//! device is refused whole: a transfer can only create files and folders
//! inside its own destination.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

/// The header that carries the body's digest.
pub const DIGEST_HEADER: &str = "X-Content-SHA256";
/// The content type of a file's bytes.
pub const FILE_TYPE: &str = "application/octet-stream";
/// The content type of a directory's tar stream.
pub const TAR_TYPE: &str = "application/x-tar";

/// Lowercase hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Copy `reader` to `writer`, answering the byte count and the SHA-256.
pub fn copy_hashing(reader: &mut dyn Read, writer: &mut dyn Write) -> io::Result<(u64, String)> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    let mut total = 0u64;
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        hasher.update(&buffer[..read]);
        writer.write_all(&buffer[..read])?;
        total += read as u64;
    }
    writer.flush()?;
    Ok((total, hex(&hasher.finalize())))
}

/// The byte count and SHA-256 of the file at `path`.
pub fn hash_file(path: &Path) -> io::Result<(u64, String)> {
    let mut file = File::open(path)?;
    copy_hashing(&mut file, &mut io::sink())
}

/// Whether `digest` is a well-formed SHA-256 in hex.
pub fn valid_digest(digest: &str) -> bool {
    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Write the tar stream of `directory`'s contents to `out`. A symlink or
/// any entry that is neither a file nor a folder is refused.
pub fn pack(directory: &Path, out: &Path) -> Result<(), String> {
    let file = File::create(out).map_err(|error| format!("{}: {error}", out.display()))?;
    let mut writer = pack_into(directory, io::BufWriter::new(file))?;
    writer
        .flush()
        .map_err(|error| format!("{}: {error}", out.display()))
}

/// Write the tar stream of `directory`'s contents to `writer`, and return
/// the writer. Every entry is judged before the first byte is written, so a
/// folder that holds a symlink or a device is refused with nothing sent.
pub fn pack_into<W: Write>(directory: &Path, writer: W) -> Result<W, String> {
    check_tree(directory, Path::new(""))?;
    let mut builder = tar::Builder::new(writer);
    builder.follow_symlinks(false);
    add_tree(&mut builder, directory, Path::new(""))?;
    builder
        .into_inner()
        .map_err(|error| format!("{}: {error}", directory.display()))
}

fn sorted_entries(folder: &Path) -> Result<Vec<fs::DirEntry>, String> {
    let mut entries: Vec<_> = fs::read_dir(folder)
        .map_err(|error| format!("{}: {error}", folder.display()))?
        .collect::<Result<_, _>>()
        .map_err(|error| format!("{}: {error}", folder.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries)
}

fn check_tree(root: &Path, relative: &Path) -> Result<(), String> {
    for entry in sorted_entries(&root.join(relative))? {
        let path = entry.path();
        let kind = fs::symlink_metadata(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?
            .file_type();
        if kind.is_dir() {
            check_tree(root, &relative.join(entry.file_name()))?;
        } else if !kind.is_file() {
            return Err(format!(
                "{} is not a file or a folder; a transfer carries only those",
                path.display()
            ));
        }
    }
    Ok(())
}

fn add_tree<W: Write>(
    builder: &mut tar::Builder<W>,
    root: &Path,
    relative: &Path,
) -> Result<(), String> {
    for entry in sorted_entries(&root.join(relative))? {
        let path = entry.path();
        let name = relative.join(entry.file_name());
        let kind = fs::symlink_metadata(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?
            .file_type();
        if kind.is_dir() {
            builder
                .append_dir(&name, &path)
                .map_err(|error| format!("{}: {error}", path.display()))?;
            add_tree(builder, root, &name)?;
        } else if kind.is_file() {
            builder
                .append_path_with_name(&path, &name)
                .map_err(|error| format!("{}: {error}", path.display()))?;
        } else {
            return Err(format!(
                "{} is not a file or a folder; a transfer carries only those",
                path.display()
            ));
        }
    }
    Ok(())
}

/// Where a folder copied as `name` lands at `path`, as `scp -r` places it:
/// inside `path` when `path` is an existing folder, else at `path` itself.
pub fn tree_destination(path: &Path, name: &str) -> Result<PathBuf, String> {
    if !path.is_dir() {
        return Ok(path.to_path_buf());
    }
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err("a folder destination needs a plain folder name".into());
    }
    Ok(path.join(name))
}

/// Put the folder `staged` at `destination` in one step: a rename when
/// nothing is there, else an atomic exchange with the folder that is there,
/// which is then removed. A reader sees the old folder or the new one,
/// never a mix. `staged` must be on the file system of `destination`; a destination
/// that is not a folder is refused.
pub fn place_tree(staged: &Path, destination: &Path) -> io::Result<()> {
    match fs::symlink_metadata(destination) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::rename(staged, destination),
        Err(error) => Err(error),
        Ok(metadata) if metadata.is_dir() => {
            exchange(staged, destination)?;
            // `staged` now holds the folder that was replaced.
            fs::remove_dir_all(staged)
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists and is not a folder", destination.display()),
        )),
    }
}

fn c_path(path: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path holds a NUL byte"))
}

/// Swap two paths atomically.
#[cfg(target_os = "macos")]
fn exchange(a: &Path, b: &Path) -> io::Result<()> {
    let (a, b) = (c_path(a)?, c_path(b)?);
    // SAFETY: both arguments are NUL-terminated paths that outlive the call.
    let rc = unsafe { libc::renamex_np(a.as_ptr(), b.as_ptr(), libc::RENAME_SWAP) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Swap two paths atomically.
#[cfg(target_os = "linux")]
fn exchange(a: &Path, b: &Path) -> io::Result<()> {
    const RENAME_EXCHANGE: libc::c_uint = 1 << 1;
    let (a, b) = (c_path(a)?, c_path(b)?);
    // SAFETY: both arguments are NUL-terminated paths that outlive the call;
    // `renameat2` is called through `syscall` because not every C library
    // the static build links declares it.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            a.as_ptr(),
            libc::AT_FDCWD,
            b.as_ptr(),
            RENAME_EXCHANGE,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn exchange(_a: &Path, _b: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no atomic exchange on this system",
    ))
}

/// Whether a tar entry's path stays inside its destination.
fn contained(path: &Path) -> bool {
    path.components()
        .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

/// Recreate the directory a tar stream at `archive` carries as `into`,
/// which must not exist. Nothing is left behind when the stream is refused.
pub fn unpack(archive: &Path, into: &Path) -> Result<(), String> {
    // A first pass judges every entry before anything is written.
    {
        let file = File::open(archive).map_err(|error| error.to_string())?;
        let mut reader = tar::Archive::new(file);
        for entry in reader.entries().map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| format!("bad tar stream: {error}"))?;
            let path: PathBuf = entry
                .path()
                .map_err(|error| format!("bad tar path: {error}"))?
                .into_owned();
            if !contained(&path) {
                return Err(format!(
                    "tar entry {} leaves its destination",
                    path.display()
                ));
            }
            let kind = entry.header().entry_type();
            if !(kind.is_file() || kind.is_dir()) {
                return Err(format!(
                    "tar entry {} is not a file or a folder",
                    path.display()
                ));
            }
        }
    }
    fs::create_dir(into).map_err(|error| format!("{}: {error}", into.display()))?;
    let file = File::open(archive).map_err(|error| error.to_string())?;
    let mut reader = tar::Archive::new(file);
    reader.set_preserve_permissions(true);
    reader.set_overwrite(false);
    let result = (|| -> Result<(), String> {
        for entry in reader.entries().map_err(|error| error.to_string())? {
            let mut entry = entry.map_err(|error| format!("bad tar stream: {error}"))?;
            entry
                .unpack_in(into)
                .map_err(|error| format!("cannot unpack: {error}"))?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(into);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FD-4 (TL-1): a folder survives a tar round trip with its files,
    /// nested folders and modes, and the digest is the body's.
    #[test]
    fn fd4_folder_round_trip() {
        let scratch = tempfile::tempdir().unwrap();
        let source = scratch.path().join("source");
        fs::create_dir_all(source.join("nested/deeper")).unwrap();
        fs::write(source.join("a.txt"), "alpha").unwrap();
        fs::write(source.join("nested/deeper/b.bin"), [0u8, 1, 2, 255]).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(source.join("a.txt"), fs::Permissions::from_mode(0o750)).unwrap();
        let archive = scratch.path().join("body.tar");
        pack(&source, &archive).unwrap();
        let (bytes, digest) = hash_file(&archive).unwrap();
        assert!(bytes > 0 && valid_digest(&digest));
        let copy = scratch.path().join("copy");
        unpack(&archive, &copy).unwrap();
        assert_eq!(fs::read_to_string(copy.join("a.txt")).unwrap(), "alpha");
        assert_eq!(
            fs::read(copy.join("nested/deeper/b.bin")).unwrap(),
            [0u8, 1, 2, 255]
        );
        let mode = fs::metadata(copy.join("a.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o750);
    }

    /// FD-4 (TL-1): a symlink is never packed, and a stream that leaves its
    /// destination or carries a link is refused with nothing written.
    #[test]
    fn fd4_links_and_escapes_are_refused() {
        let scratch = tempfile::tempdir().unwrap();
        let source = scratch.path().join("source");
        fs::create_dir_all(&source).unwrap();
        std::os::unix::fs::symlink("/etc/hosts", source.join("link")).unwrap();
        let error = pack(&source, &scratch.path().join("out.tar")).unwrap_err();
        assert!(error.contains("not a file or a folder"), "{error}");

        for (name, kind) in [
            ("../escape.txt", tar::EntryType::Regular),
            ("/abs.txt", tar::EntryType::Regular),
            ("link", tar::EntryType::Symlink),
        ] {
            let archive = scratch.path().join("evil.tar");
            {
                let mut builder = tar::Builder::new(File::create(&archive).unwrap());
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(kind);
                header.set_size(if kind == tar::EntryType::Regular {
                    3
                } else {
                    0
                });
                header.set_mode(0o644);
                // `set_path` refuses `..`, so the raw name is written.
                let bytes = name.as_bytes();
                header.as_old_mut().name[..bytes.len()].copy_from_slice(bytes);
                if kind == tar::EntryType::Symlink {
                    header.set_link_name("/etc/hosts").unwrap();
                }
                header.set_cksum();
                let data: &[u8] = if kind == tar::EntryType::Regular {
                    b"bad"
                } else {
                    b""
                };
                builder.append(&header, data).unwrap();
                builder.finish().unwrap();
            }
            let into = scratch.path().join("into");
            let error = unpack(&archive, &into).unwrap_err();
            assert!(!into.exists(), "{name}: {error}");
            assert!(!scratch.path().join("escape.txt").exists());
        }
    }

    /// A folder placed over an existing one replaces it whole in one step;
    /// one placed where nothing is lands there; a file is never replaced.
    #[test]
    fn a_tree_is_placed_by_rename_or_by_exchange() {
        let scratch = tempfile::tempdir().unwrap();
        let destination = scratch.path().join("tree");
        let staged = scratch.path().join(".tree.staged");
        fs::create_dir(&staged).unwrap();
        fs::write(staged.join("new.txt"), "new").unwrap();
        place_tree(&staged, &destination).unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("new.txt")).unwrap(),
            "new"
        );
        assert!(!staged.exists());

        fs::create_dir(&staged).unwrap();
        fs::write(staged.join("newer.txt"), "newer").unwrap();
        place_tree(&staged, &destination).unwrap();
        assert!(!destination.join("new.txt").exists(), "replaced whole");
        assert_eq!(
            fs::read_to_string(destination.join("newer.txt")).unwrap(),
            "newer"
        );
        assert!(!staged.exists(), "the replaced folder is removed");

        let file = scratch.path().join("file");
        fs::write(&file, "x").unwrap();
        fs::create_dir(&staged).unwrap();
        assert!(place_tree(&staged, &file).is_err());
        assert_eq!(fs::read_to_string(&file).unwrap(), "x");

        assert_eq!(
            tree_destination(scratch.path(), "copy").unwrap(),
            scratch.path().join("copy")
        );
        assert_eq!(tree_destination(&file, "copy").unwrap(), file);
        assert!(tree_destination(scratch.path(), "../x").is_err());
    }

    /// The streaming pack refuses a symlink before it writes a byte.
    #[test]
    fn pack_into_judges_every_entry_first() {
        let scratch = tempfile::tempdir().unwrap();
        let source = scratch.path().join("source");
        fs::create_dir_all(source.join("a")).unwrap();
        fs::write(source.join("a/file"), "x").unwrap();
        std::os::unix::fs::symlink("/etc/hosts", source.join("z-link")).unwrap();
        let mut out = Vec::new();
        assert!(pack_into(&source, &mut out).is_err());
        assert!(out.is_empty());
    }

    #[test]
    fn digests_are_hex_sha256() {
        let (bytes, digest) = copy_hashing(&mut &b"abc"[..], &mut io::sink()).unwrap();
        assert_eq!(bytes, 3);
        assert_eq!(
            digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(valid_digest(&digest));
        assert!(!valid_digest("xyz"));
    }
}
