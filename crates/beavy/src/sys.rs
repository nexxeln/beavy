//! Linux process plumbing shared by the host check and worktrees: forking a child
//! for namespace work, entering a user namespace, and reading capabilities.

use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

const CAP_SYS_ADMIN: u32 = 21;

/// Steps a namespace operation can fail at, reported with the errno.
pub(crate) const STAGE_UNSHARE: u8 = 1;
pub(crate) const STAGE_MAP: u8 = 2;
pub(crate) const STAGE_CHDIR: u8 = 3;
pub(crate) const STAGE_PRIVATE: u8 = 4;
pub(crate) const STAGE_MOUNT: u8 = 5;
pub(crate) const STAGE_JOIN: u8 = 6;

pub(crate) fn errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Whether this process has CAP_SYS_ADMIN in its user namespace.
pub(crate) fn has_cap_sys_admin() -> bool {
    let Ok(status) = fs::read_to_string("/proc/self/status") else {
        return false;
    };
    status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
        .is_some_and(|caps| caps & (1 << CAP_SYS_ADMIN) != 0)
}

/// Files and contents for entering a new user and mount namespace, prepared before
/// fork so the child only makes syscalls.
pub(crate) struct IdMaps {
    setgroups: CString,
    uid_map: CString,
    gid_map: CString,
    deny: Vec<u8>,
    uid_line: Vec<u8>,
    gid_line: Vec<u8>,
}

impl IdMaps {
    /// Map this process's IDs to root in the namespace.
    pub(crate) fn to_root() -> Self {
        Self::mapping(|_| 0)
    }

    /// Map this process's IDs to themselves, so a command in the namespace sees
    /// the caller's real user and group.
    pub(crate) fn identity() -> Self {
        Self::mapping(|id| id)
    }

    fn mapping(inside: impl Fn(u32) -> u32) -> Self {
        // SAFETY: geteuid and getegid have no preconditions.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        IdMaps {
            setgroups: c"/proc/self/setgroups".to_owned(),
            uid_map: c"/proc/self/uid_map".to_owned(),
            gid_map: c"/proc/self/gid_map".to_owned(),
            deny: b"deny".to_vec(),
            uid_line: format!("{} {uid} 1", inside(uid)).into_bytes(),
            gid_line: format!("{} {gid} 1", inside(gid)).into_bytes(),
        }
    }

    /// Enter a new user and mount namespace. Async-signal-safe calls only.
    pub(crate) fn enter(&self) -> Result<(), (u8, i32)> {
        // SAFETY: unshare takes flags only.
        if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) } != 0 {
            return Err((STAGE_UNSHARE, errno()));
        }
        write_file(&self.setgroups, &self.deny).map_err(|e| (STAGE_MAP, e))?;
        write_file(&self.uid_map, &self.uid_line).map_err(|e| (STAGE_MAP, e))?;
        write_file(&self.gid_map, &self.gid_line).map_err(|e| (STAGE_MAP, e))?;
        Ok(())
    }
}

fn write_file(path: &CString, contents: &[u8]) -> Result<(), i32> {
    // SAFETY: path is NUL-terminated; the descriptor is closed before returning.
    unsafe {
        let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
        if fd < 0 {
            return Err(errno());
        }
        let written = libc::write(fd, contents.as_ptr().cast(), contents.len());
        let result = if written == contents.len() as isize {
            Ok(())
        } else {
            Err(errno())
        };
        libc::close(fd);
        result
    }
}

pub(crate) enum ChildResult {
    Succeeded,
    Failed { stage: u8, err: io::Error },
    Abnormal(String),
}

/// Run `work` in a forked child so namespace changes never touch this process.
/// `work` must make only async-signal-safe calls: the parent may have other threads.
pub(crate) fn in_child(work: impl FnOnce() -> Result<(), (u8, i32)>) -> ChildResult {
    let mut fds = [0; 2];
    // SAFETY: fds has room for two descriptors.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return ChildResult::Abnormal(format!(
            "Couldn't create a pipe: {}",
            io::Error::last_os_error()
        ));
    }
    // SAFETY: pipe2 returned two open descriptors that nothing else owns.
    let (read_end, write_end) =
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };

    // SAFETY: the child calls only async-signal-safe functions, then _exit.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return ChildResult::Abnormal(format!("Couldn't fork: {}", io::Error::last_os_error()));
    }
    if pid == 0 {
        let code = match work() {
            Ok(()) => 0,
            Err((stage, err)) => {
                let mut message = [0u8; 5];
                message[0] = stage;
                message[1..].copy_from_slice(&err.to_ne_bytes());
                // SAFETY: the write end is open and message is 5 bytes.
                unsafe { libc::write(write_end.as_raw_fd(), message.as_ptr().cast(), 5) };
                1
            }
        };
        // SAFETY: _exit ends the child without running the parent's destructors.
        unsafe { libc::_exit(code) };
    }

    drop(write_end);
    let mut message = Vec::new();
    let _ = File::from(read_end).read_to_end(&mut message);
    let mut status = 0;
    loop {
        // SAFETY: pid is our child and status is a valid out pointer.
        let rc = unsafe { libc::waitpid(pid, &mut status, 0) };
        if rc == pid {
            break;
        }
        if rc < 0 && errno() != libc::EINTR {
            return ChildResult::Abnormal(format!(
                "Couldn't wait for the child process: {}",
                io::Error::last_os_error()
            ));
        }
    }
    if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
        return ChildResult::Succeeded;
    }
    if let [stage, a, b, c, d] = message[..] {
        return ChildResult::Failed {
            stage,
            err: io::Error::from_raw_os_error(i32::from_ne_bytes([a, b, c, d])),
        };
    }
    if libc::WIFSIGNALED(status) {
        return ChildResult::Abnormal(format!(
            "The child process was stopped by signal {}",
            libc::WTERMSIG(status)
        ));
    }
    ChildResult::Abnormal("The child process exited without a result".into())
}
