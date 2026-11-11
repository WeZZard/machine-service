//! A test child's process tree and its lifeline (DC-256).
//!
//! A test hands every child a lifeline: the read end of a pipe whose write
//! end only the test holds, open in the child under one descriptor number
//! that every descendant inherits. When the test is gone, by its end or by
//! Ctrl-C, the pipe reads end-of-file, and a test build that watches it ends
//! every process related to it: its own process group, its descendants, the
//! groups they lead, and every process that still holds the lifeline,
//! which catches a descendant that another session or a reparenting hid.
//! The test's harness ends the same set when it drops a child.

use std::collections::HashSet;

/// One row of the account's process table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    /// The process ID.
    pub pid: i32,
    /// The parent's process ID.
    pub ppid: i32,
    /// The process group ID.
    pub pgid: i32,
    /// The start time, seconds and microseconds.
    pub start: (u64, u64),
}

/// The flavor of `proc_pidfdinfo` that describes a pipe.
#[cfg(target_os = "macos")]
const PROC_PIDFDPIPEINFO: libc::c_int = 6;

/// `struct pipe_fdinfo` of `sys/proc_info.h`: a `proc_fileinfo` (24 bytes),
/// a `vinfo_stat` (136 bytes), the pipe's handle and its peer's, a status
/// and a spare word.
#[cfg(target_os = "macos")]
#[repr(C)]
struct PipeFdInfo {
    fileinfo: [u8; 24],
    stat: [u8; 136],
    handle: u64,
    peer: u64,
    status: i32,
    spare: i32,
}

/// The kernel's handle of the pipe end that `pid` holds as descriptor `fd`.
pub(crate) fn pipe_handle(pid: i32, fd: i32) -> Option<u64> {
    pipe_ends(pid, fd).map(|(handle, _)| handle)
}

/// The handles of the pipe end that `pid` holds as descriptor `fd` and of
/// its other end.
#[cfg(target_os = "macos")]
fn pipe_ends(pid: i32, fd: i32) -> Option<(u64, u64)> {
    // SAFETY: `PipeFdInfo` is plain old data; zeroed is a valid value.
    let mut info: PipeFdInfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<PipeFdInfo>() as libc::c_int;
    // SAFETY: the buffer is a writable `PipeFdInfo` of the size passed.
    let written = unsafe {
        libc::proc_pidfdinfo(
            pid,
            fd,
            PROC_PIDFDPIPEINFO,
            (&mut info as *mut PipeFdInfo).cast(),
            size,
        )
    };
    (written == size && info.handle != 0).then_some((info.handle, info.peer))
}

#[cfg(not(target_os = "macos"))]
fn pipe_ends(_pid: i32, _fd: i32) -> Option<(u64, u64)> {
    None
}

/// Whether `pid` holds the pipe end `handle` under any descriptor.
#[cfg(target_os = "macos")]
fn holds_anywhere(pid: i32, handle: u64) -> bool {
    let mut fds = vec![
        libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0
        };
        4096
    ];
    let bytes = (fds.len() * std::mem::size_of::<libc::proc_fdinfo>()) as libc::c_int;
    // SAFETY: the buffer is writable for `bytes` bytes of `proc_fdinfo`.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr().cast(),
            bytes,
        )
    };
    if written <= 0 {
        return false;
    }
    fds.truncate(written as usize / std::mem::size_of::<libc::proc_fdinfo>());
    fds.iter()
        .filter(|fd| fd.proc_fdtype == libc::PROX_FDTYPE_PIPE as u32)
        .any(|fd| pipe_handle(pid, fd.proc_fd) == Some(handle))
}

#[cfg(not(target_os = "macos"))]
fn holds_anywhere(_pid: i32, _handle: u64) -> bool {
    false
}

/// The executable path of `pid`.
#[cfg(target_os = "macos")]
fn path_of(pid: i32) -> Option<std::path::PathBuf> {
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer is writable for the size passed.
    let length =
        unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if length <= 0 {
        return None;
    }
    buffer.truncate(length as usize);
    Some(std::path::PathBuf::from(
        String::from_utf8_lossy(&buffer).into_owned(),
    ))
}

#[cfg(not(target_os = "macos"))]
fn path_of(_pid: i32) -> Option<std::path::PathBuf> {
    None
}

/// Whether `row` is a copy of this process that another of its threads
/// has forked and not yet replaced with the child's program: until its
/// `exec` it holds every descriptor this process has, every lifeline
/// included, without being a holder of any.
fn is_own_fork(row: &Row, own: i32) -> bool {
    row.ppid == own
        && match (path_of(row.pid), std::env::current_exe().ok()) {
            (Some(path), Some(exe)) => {
                path == exe || exe.canonicalize().is_ok_and(|exe| exe == path)
            }
            _ => false,
        }
}

/// Whether `row` holds the lifeline whose read end is descriptor `fd` with
/// `handle`, as a process started on it does. The test that owns the
/// lifeline holds its write end too, and is never one: a watcher must never
/// end the test that is still alive.
///
/// Whether `row` is this test's own copy is asked before whether it holds
/// the pipe (DC-304). A copy that calls `exec` between the two looks holds
/// the pipe at the first and shows its new program at the second, so the
/// other order took it for a holder after `exec` had closed the pipe, and
/// `kill`, which `exec` does not tell apart, ended another test's child.
/// In this order, a process judged not a copy is judged on what it holds
/// after that.
///
/// The lifeline counts under any descriptor, not only the one it was handed
/// under: a program may move it, as a launchd job's prelude that opens its
/// own report descriptor does under the fake launchctl (RA-10), and what
/// such a job leaves running holds it nowhere else.
fn holds(row: &Row, own: i32, fd: i32, handle: u64) -> bool {
    if row.pid == own || is_own_fork(row, own) {
        return false;
    }
    let reads = pipe_handle(row.pid, fd) == Some(handle) || holds_anywhere(row.pid, handle);
    // The write end is the test's own: a process that holds it is the test.
    let write = pipe_ends(own, fd).map(|(_, write)| write);
    reads && !write.is_some_and(|write| holds_anywhere(row.pid, write))
}

/// One process's row.
#[cfg(target_os = "macos")]
fn row(pid: i32) -> Option<(Row, u32)> {
    // SAFETY: `proc_bsdinfo` is plain old data; zeroed is a valid value.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: the buffer is a writable `proc_bsdinfo` of the size passed.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (written == size).then_some((
        Row {
            pid,
            ppid: info.pbi_ppid as i32,
            pgid: info.pbi_pgid as i32,
            start: (info.pbi_start_tvsec, info.pbi_start_tvusec),
        },
        info.pbi_uid,
    ))
}

/// The account's own processes.
#[cfg(target_os = "macos")]
pub fn table() -> Vec<Row> {
    // SAFETY: `getuid` only reads process state.
    let uid = unsafe { libc::getuid() };
    let mut capacity = 4096usize;
    let pids: Vec<i32> = loop {
        let mut buffer = vec![0i32; capacity];
        let bytes = (buffer.len() * std::mem::size_of::<i32>()) as libc::c_int;
        // SAFETY: the buffer is writable for `bytes` bytes.
        let count = unsafe { libc::proc_listallpids(buffer.as_mut_ptr().cast(), bytes) };
        if count < 0 {
            return Vec::new();
        }
        if (count as usize) < capacity {
            buffer.truncate(count as usize);
            break buffer;
        }
        capacity *= 2;
    };
    pids.into_iter()
        .filter(|pid| *pid > 1)
        .filter_map(row)
        .filter(|(_, owner)| *owner == uid)
        .map(|(row, _)| row)
        .collect()
}

#[cfg(not(target_os = "macos"))]
pub fn table() -> Vec<Row> {
    Vec::new()
}

/// Every process related to `root` in `rows`: `root`, the members of its
/// process group, its descendants, the members of every group one of them
/// leads, and every process `holds` says holds the lifeline.
///
/// `spared` is a process group never taken whole: the caller's own, when
/// the caller is not `root`, so a child left in its test's group can never
/// take the test with it.
pub fn related_in(
    rows: &[Row],
    root: i32,
    spared: Option<i32>,
    holds: &dyn Fn(&Row) -> bool,
) -> Vec<Row> {
    let root_group = rows
        .iter()
        .find(|row| row.pid == root)
        .map(|row| row.pgid)
        .unwrap_or(root);
    let root_group = Some(root_group).filter(|group| Some(*group) != spared);
    let mut members: HashSet<i32> = HashSet::from([root]);
    let mut found: Vec<Row> = Vec::new();
    loop {
        let before = found.len();
        for row in rows {
            if found.iter().any(|seen| seen.pid == row.pid) {
                continue;
            }
            let joins = row.pid == root
                || Some(row.pgid) == root_group
                || members.contains(&row.ppid)
                || (members.contains(&row.pgid) && Some(row.pgid) != spared)
                || holds(row);
            if joins {
                members.insert(row.pid);
                found.push(*row);
            }
        }
        if found.len() == before {
            return found;
        }
    }
}

/// Every living process related to `root` ([`related_in`]); `lifeline`, the
/// descriptor number and the handle of a lifeline's read end, adds every
/// process holding it.
pub fn related(root: i32, lifeline: Option<(i32, u64)>) -> Vec<Row> {
    let rows = table();
    // SAFETY: `getpid` and `getpgrp` only read process state.
    let (own, own_group) = unsafe { (libc::getpid(), libc::getpgrp()) };
    let spared = (own != root).then_some(own_group);
    let holds = |row: &Row| lifeline.is_some_and(|(fd, handle)| holds(row, own, fd, handle));
    related_in(&rows, root, spared, &holds)
}

/// Every process holding the lifeline whose read end is descriptor `fd`
/// with `handle`, except this one.
#[cfg_attr(not(feature = "test-build"), allow(dead_code))]
pub fn holders(fd: i32, handle: u64) -> Vec<Row> {
    // SAFETY: `getpid` only reads process state.
    let own = unsafe { libc::getpid() };
    table()
        .into_iter()
        .filter(|row| holds(row, own, fd, handle))
        .collect()
}

/// Send `SIGKILL` to every row that still names the same process, except
/// this one.
pub fn kill(rows: &[Row]) {
    // SAFETY: `getpid` only reads process state.
    let own = unsafe { libc::getpid() };
    let living: HashSet<(i32, (u64, u64))> =
        table().iter().map(|row| (row.pid, row.start)).collect();
    for row in rows {
        if row.pid > 1 && row.pid != own && living.contains(&(row.pid, row.start)) {
            // SAFETY: the PID was re-read with the same start time just now.
            unsafe { libc::kill(row.pid, libc::SIGKILL) };
        }
    }
}

/// This process's own lifeline, when a test handed it one: its descriptor
/// number and its handle.
pub(crate) fn own_lifeline() -> Option<(i32, u64)> {
    let (fd, handle) = std::env::var(super::LIFELINE_VARIABLE)
        .ok()
        .and_then(|value| parse_lifeline(&value))?;
    // SAFETY: `getpid` only reads process state.
    let own = unsafe { libc::getpid() };
    (pipe_handle(own, fd) == Some(handle)).then_some((fd, handle))
}

/// The processes this one started: its descendants, and every process
/// that holds its lifeline, which a descendant that left the tree still
/// does. Never this process itself.
pub fn descendants() -> HashSet<i32> {
    let rows = table();
    // SAFETY: `getpid` only reads process state.
    let own = unsafe { libc::getpid() };
    let lifeline = own_lifeline();
    let mut found: HashSet<i32> = rows
        .iter()
        .filter(|row| lifeline.is_some_and(|(fd, handle)| holds(row, own, fd, handle)))
        .map(|row| row.pid)
        .collect();
    loop {
        let before = found.len();
        for row in &rows {
            if row.pid != own && (row.ppid == own || found.contains(&row.ppid)) {
                found.insert(row.pid);
            }
        }
        if found.len() == before {
            return found;
        }
    }
}

/// The lifeline's value for a child: its descriptor number and its handle.
#[cfg_attr(not(feature = "test-build"), allow(dead_code))]
pub(crate) fn lifeline_value(fd: i32, handle: u64) -> String {
    format!("{fd}:{handle:x}")
}

fn parse_lifeline(value: &str) -> Option<(i32, u64)> {
    let (fd, handle) = value.split_once(':')?;
    Some((fd.parse().ok()?, u64::from_str_radix(handle, 16).ok()?))
}

/// Watch the lifeline the test handed this process, if it is the one the
/// variable describes; when it closes, end every related process and exit.
#[cfg_attr(not(feature = "test-build"), allow(dead_code))]
pub(crate) fn watch_lifeline() {
    // A number that no longer names the test's pipe is not a lifeline.
    let Some((fd, handle)) = own_lifeline() else {
        return;
    };
    // SAFETY: `getpid` only reads process state.
    let own = unsafe { libc::getpid() };
    let spawned = std::thread::Builder::new()
        .name("test-lifeline".to_string())
        .spawn(move || {
            let mut byte = [0u8; 1];
            loop {
                // SAFETY: the buffer is writable for one byte; `fd` is the
                // lifeline checked above.
                let read = unsafe { libc::read(fd, byte.as_mut_ptr().cast(), 1) };
                if read > 0 {
                    continue;
                }
                if read < 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                    continue;
                }
                break;
            }
            kill(&related(own, Some((fd, handle))));
            // SAFETY: `_exit` ends the process without running destructors,
            // as a test that is gone expects.
            unsafe { libc::_exit(137) };
        });
    let _ = spawned;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: i32, ppid: i32, pgid: i32) -> Row {
        Row {
            pid,
            ppid,
            pgid,
            start: (1, 0),
        }
    }

    #[test]
    fn the_tree_follows_parents_groups_and_holders() {
        let rows = [
            row(10, 5, 10),  // the root, leading its group
            row(11, 10, 10), // its child
            row(12, 1, 10),  // a group member reparented to launchd
            row(20, 12, 20), // a group led by that member's child
            row(21, 1, 20),  // a member of it, reparented
            row(30, 1, 30),  // a holder of the lifeline elsewhere
            row(31, 30, 31), // its child in a session of its own
            row(5, 1, 5),    // the test itself
            row(40, 5, 40),  // another test child
            row(41, 1, 99),  // an unrelated process
        ];
        let holds = |row: &Row| row.pid == 30;
        let mut pids: Vec<i32> = related_in(&rows, 10, Some(5), &holds)
            .iter()
            .map(|row| row.pid)
            .collect();
        pids.sort();
        assert_eq!(pids, vec![10, 11, 12, 20, 21, 30, 31]);
    }

    #[test]
    fn a_member_that_is_not_the_leader_takes_its_group() {
        let rows = [
            row(10, 1, 10),
            row(11, 10, 10),
            row(12, 1, 10),
            row(13, 1, 13),
        ];
        let none = |_: &Row| false;
        let mut pids: Vec<i32> = related_in(&rows, 12, None, &none)
            .iter()
            .map(|row| row.pid)
            .collect();
        pids.sort();
        assert_eq!(pids, vec![10, 11, 12]);
        // A child left in its test's group never takes the test's group.
        let rows = [row(5, 1, 5), row(6, 5, 5), row(10, 5, 5), row(11, 10, 5)];
        let mut pids: Vec<i32> = related_in(&rows, 10, Some(5), &none)
            .iter()
            .map(|row| row.pid)
            .collect();
        pids.sort();
        assert_eq!(pids, vec![10, 11]);
    }

    #[test]
    fn a_lifeline_value_round_trips() {
        assert_eq!(
            parse_lifeline(&lifeline_value(87, 0xdead_beef)),
            Some((87, 0xdead_beef))
        );
        assert_eq!(parse_lifeline("x"), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_pipe_handle_is_read_for_this_process() {
        use std::os::fd::AsRawFd;
        let (reader, writer) = std::io::pipe().unwrap();
        let own = std::process::id() as i32;
        let read = pipe_handle(own, reader.as_raw_fd()).unwrap();
        let write = pipe_handle(own, writer.as_raw_fd()).unwrap();
        assert_ne!(read, write);
        assert!(table().iter().any(|row| row.pid == own));
    }
}
