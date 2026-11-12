//! overlayfs: the mount options for a worktree, the privilege mode that makes
//! mounting possible, the mount table, and what to tell the caller when the kernel
//! refuses.
//!
//! Every mount passes `userxattr`, so overlayfs keeps its whiteouts, opaque
//! directories, and other metadata in `user.overlay.*` attributes whether it runs
//! as root or in a user namespace. That keeps layers interchangeable between the
//! two modes, and it turns off redirects and metacopy, so every layer holds whole
//! files and stands on its own.

use std::ffi::{CString, OsString};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use rustix::process::Gid;
use serde::Serialize;

use super::owner::{self, Identity};
use crate::error::{Error, Next, Reason};
use crate::sys::{
    ChildResult, IdMaps, STAGE_CHDIR, STAGE_JOIN, STAGE_MAP, STAGE_MOUNT, STAGE_PRIVATE,
    STAGE_UNSHARE, STAGE_USER, errno, has_cap_sys_admin, in_child,
};

/// How this process gets the right to mount overlayfs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Privilege {
    /// Root with CAP_SYS_ADMIN: mounts in a private mount namespace, or for every
    /// process with a persistent mount.
    Root,
    /// An unprivileged user and mount namespace, with the caller's IDs mapped to
    /// themselves.
    Userns,
}

impl Privilege {
    pub fn detect() -> Self {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 && has_cap_sys_admin() {
            Privilege::Root
        } else {
            Privilege::Userns
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Privilege::Root => "root",
            Privilege::Userns => "userns",
        }
    }
}

/// The worktree backend and how this process uses it.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Backend {
    /// Always `overlayfs` for now.
    pub name: &'static str,
    pub privilege: Privilege,
}

impl Backend {
    pub fn detect() -> Self {
        Backend {
            name: "overlayfs",
            privilege: Privilege::detect(),
        }
    }
}

/// The longest mount option string that every kernel accepts: one page, less the
/// terminating NUL.
const MAX_OPTIONS: usize = 4095;

/// Mount options for a stack of layers, bottom first, under an upper and a work
/// directory. Every path is relative to the store root, which keeps the string
/// far below the one-page limit; the mounting process must have the store root as
/// its working directory.
pub(crate) fn options(layers: &[String], upper: &Path, work: &Path) -> Result<CString, Error> {
    let lower: Vec<String> = layers
        .iter()
        .rev()
        .map(|layer| format!("layers/{layer}"))
        .collect();
    let text = format!(
        "lowerdir={},upperdir={},workdir={},userxattr,index=off",
        lower.join(":"),
        upper.display(),
        work.display()
    );
    if text.len() > MAX_OPTIONS {
        return Err(Error::new(
            Reason::InvalidStore,
            format!(
                "Can't mount a stack of {} layers. Its mount options are longer than one page.",
                layers.len()
            ),
        ));
    }
    CString::new(text).map_err(|err| Error::new(Reason::InternalError, err.to_string()))
}

pub(crate) fn c_path(path: &Path) -> Result<CString, Error> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        Error::new(
            Reason::InvalidArguments,
            format!("Can't use {}. It contains a NUL byte.", path.display()),
        )
    })
}

/// One line of a mount table.
#[derive(Debug, Clone)]
pub(crate) struct Mount {
    pub point: PathBuf,
    pub fstype: String,
    pub source: OsString,
}

/// The mount table of this process, or of another process by pid.
pub(crate) fn mounts(pid: Option<u32>) -> io::Result<Vec<Mount>> {
    let path = match pid {
        Some(pid) => format!("/proc/{pid}/mountinfo"),
        None => "/proc/self/mountinfo".to_string(),
    };
    let table = fs::read(path)?;
    let mut mounts = Vec::new();
    for line in table.split(|b| *b == b'\n') {
        let fields: Vec<&[u8]> = line.split(|b| *b == b' ').collect();
        let Some(dash) = fields.iter().position(|field| *field == b"-") else {
            continue;
        };
        let (Some(point), Some(fstype), Some(source)) =
            (fields.get(4), fields.get(dash + 1), fields.get(dash + 2))
        else {
            continue;
        };
        mounts.push(Mount {
            point: PathBuf::from(OsString::from_vec(unescape(point))),
            fstype: String::from_utf8_lossy(fstype).into_owned(),
            source: OsString::from_vec(unescape(source)),
        });
    }
    Ok(mounts)
}

/// Undo the octal escapes the kernel writes for spaces, tabs, newlines, and
/// backslashes in mount table fields.
fn unescape(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut i = 0;
    while i < field.len() {
        if field[i] == b'\\'
            && let Some(octal) = field.get(i + 1..i + 4)
            && octal.iter().all(|b| (b'0'..=b'7').contains(b))
        {
            out.push(
                octal
                    .iter()
                    .fold(0u8, |n, b| n.wrapping_mul(8) + (b - b'0')),
            );
            i += 4;
            continue;
        }
        out.push(field[i]);
        i += 1;
    }
    out
}

/// The overlay mounts whose source is `source`, a worktree directory.
pub(crate) fn mounts_of(table: &[Mount], source: &Path) -> Vec<PathBuf> {
    table
        .iter()
        .filter(|mount| mount.fstype == "overlay" && Path::new(&mount.source) == source)
        .map(|mount| mount.point.clone())
        .collect()
}

/// What an overlay mount needs, prepared before any fork.
pub(crate) struct Spec {
    /// The store root, the working directory for the relative paths in `options`.
    pub store: CString,
    /// The worktree directory, recorded as the mount's source so the mount table
    /// says which worktree a mount belongs to.
    pub source: CString,
    pub options: CString,
}

impl Spec {
    /// Mount on `target`. overlayfs works in its layers with the credentials it
    /// was mounted with, and needs the file capabilities that acting as the
    /// store's owner set aside, so they're raised again first; the owner's IDs
    /// stay, so what overlayfs creates belongs to the owner. Async-signal-safe
    /// calls only.
    fn mount(&self, target: &CString) -> Result<(), (u8, i32)> {
        let failed = |err: rustix::io::Errno| (STAGE_MOUNT, err.raw_os_error());
        let mut sets = rustix::thread::capabilities(None).map_err(failed)?;
        sets.effective = sets.permitted;
        rustix::thread::set_capabilities(None, sets).map_err(failed)?;
        // SAFETY: every pointer is a NUL-terminated string prepared before fork.
        unsafe {
            if libc::chdir(self.store.as_ptr()) != 0 {
                return Err((STAGE_CHDIR, errno()));
            }
            if libc::mount(
                self.source.as_ptr(),
                target.as_ptr(),
                c"overlay".as_ptr(),
                0,
                self.options.as_ptr().cast(),
            ) != 0
            {
                return Err((STAGE_MOUNT, errno()));
            }
        }
        Ok(())
    }
}

/// Enter a private mount namespace: a new user namespace too unless this process
/// is root. Mounts made after this never reach the caller's namespace, and mounts
/// the host makes later still reach this one. Async-signal-safe calls only.
fn private_namespace(privilege: Privilege, maps: &IdMaps) -> Result<(), (u8, i32)> {
    match privilege {
        // SAFETY: unshare takes flags only.
        Privilege::Root => {
            if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
                return Err((STAGE_UNSHARE, errno()));
            }
        }
        Privilege::Userns => maps.enter()?,
    }
    // SAFETY: the target is a NUL-terminated string; the other pointers may be null.
    let rc = unsafe {
        libc::mount(
            std::ptr::null(),
            c"/".as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_SLAVE,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err((STAGE_PRIVATE, errno()));
    }
    Ok(())
}

/// Mount and unmount in a throwaway namespace, proving the kernel accepts this
/// stack before beavy records anything that depends on it.
pub(crate) fn probe(spec: &Spec, target: &Path, privilege: Privilege) -> Result<(), Failure> {
    let target = c_path(target).map_err(Failure::Other)?;
    let maps = IdMaps::identity();
    let result = in_child(|| {
        private_namespace(privilege, &maps)?;
        spec.mount(&target)?;
        // SAFETY: target is NUL-terminated.
        unsafe { libc::umount2(target.as_ptr(), libc::MNT_DETACH) };
        Ok(())
    });
    child_result(result)
}

/// Mount for every process in this mount namespace. Needs root.
pub(crate) fn mount_persistent(spec: &Spec, target: &Path) -> Result<(), Failure> {
    let target = c_path(target).map_err(Failure::Other)?;
    child_result(in_child(|| spec.mount(&target)))
}

/// Unmount a persistent mount. `Ok(false)` means it wasn't mounted.
pub(crate) fn unmount(target: &Path) -> io::Result<bool> {
    let target = CString::new(target.as_os_str().as_bytes()).map_err(io::Error::other)?;
    // SAFETY: target is NUL-terminated.
    if unsafe { libc::umount2(target.as_ptr(), 0) } == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EINVAL | libc::ENOENT) => Ok(false),
        _ => Err(err),
    }
}

/// Detach a mount nobody should be using, such as one left by a removed worktree.
pub(crate) fn detach(target: &Path) {
    if let Ok(target) = CString::new(target.as_os_str().as_bytes()) {
        // SAFETY: target is NUL-terminated.
        unsafe { libc::umount2(target.as_ptr(), libc::MNT_DETACH) };
    }
}

fn child_result(result: ChildResult) -> Result<(), Failure> {
    match result {
        ChildResult::Succeeded => Ok(()),
        ChildResult::Failed { stage, err } => Err(Failure::Step { stage, err }),
        ChildResult::Abnormal(reason) => Err(Failure::Other(Error::new(
            Reason::IoError,
            format!("Couldn't mount the worktree. {reason}."),
        ))),
    }
}

/// Why a mount didn't happen.
#[derive(Debug)]
pub(crate) enum Failure {
    /// A namespace or mount step failed with an errno.
    Step {
        stage: u8,
        err: io::Error,
    },
    Other(Error),
}

/// How a run enters its worktree.
pub(crate) enum Entry {
    /// Mount the worktree in a new private mount namespace.
    Mount(Spec),
    /// Bind the worktree's persistent mount in a new private mount namespace.
    Bind(CString),
    /// Join the mount namespace of a command already running in the worktree, and
    /// its user namespace when that differs from this process's.
    Join {
        user: Option<OwnedFd>,
        mount: OwnedFd,
    },
}

/// Everything a run does between fork and exec, prepared beforehand.
pub(crate) struct Enter {
    pub entry: Entry,
    pub privilege: Privilege,
    pub maps: IdMaps,
    /// The directory the worktree covers: the path its snapshot was imported from.
    pub target: CString,
    /// The command's working directory, resolved again after the mount.
    pub cwd: CString,
    /// A descriptor to keep open across exec: the lock that marks the run as live.
    pub keep: RawFd,
    /// The user the command runs as, with its groups, when it isn't this process's.
    pub user: Option<(Identity, Vec<Gid>)>,
}

impl Enter {
    /// Runs before exec, in the process that becomes the command: async-signal-safe
    /// calls only. A failure names the step and its errno.
    pub(crate) fn enter(&self) -> Result<(), (u8, i32)> {
        match &self.entry {
            Entry::Join { user, mount } => {
                // SAFETY: the descriptors are open namespace files.
                unsafe {
                    if let Some(user) = user
                        && libc::setns(user.as_raw_fd(), libc::CLONE_NEWUSER) != 0
                    {
                        return Err((STAGE_JOIN, errno()));
                    }
                    if libc::setns(mount.as_raw_fd(), libc::CLONE_NEWNS) != 0 {
                        return Err((STAGE_JOIN, errno()));
                    }
                }
            }
            Entry::Mount(spec) => {
                private_namespace(self.privilege, &self.maps)?;
                spec.mount(&self.target)?;
            }
            Entry::Bind(from) => {
                private_namespace(self.privilege, &self.maps)?;
                // SAFETY: both paths are NUL-terminated; the other pointers may be null.
                let rc = unsafe {
                    libc::mount(
                        from.as_ptr(),
                        self.target.as_ptr(),
                        std::ptr::null(),
                        libc::MS_BIND,
                        std::ptr::null(),
                    )
                };
                if rc != 0 {
                    return Err((STAGE_MOUNT, errno()));
                }
            }
        }
        // SAFETY: cwd is NUL-terminated; keep is an open descriptor.
        unsafe {
            if libc::chdir(self.cwd.as_ptr()) != 0 {
                return Err((STAGE_CHDIR, errno()));
            }
            if libc::fcntl(self.keep, libc::F_SETFD, 0) != 0 {
                return Err((STAGE_CHDIR, errno()));
            }
        }
        if let Some((user, groups)) = &self.user {
            owner::become_user(user, groups).map_err(|err| (STAGE_USER, err))?;
        }
        Ok(())
    }
}

/// Explain a failed namespace or mount step, with the commands that fix it.
pub(crate) fn unavailable(stage: u8, err: &io::Error, privilege: Privilege, store: &Path) -> Error {
    let errno = err.raw_os_error();
    let store_fs = crate::host::linux::filesystem_type(store);
    let container = ["/.dockerenv", "/run/.containerenv"]
        .iter()
        .any(|path| Path::new(path).exists());
    let docker = " In a container, start it with --cap-add SYS_ADMIN --security-opt apparmor=unconfined --security-opt seccomp=unconfined.";
    let mut message = match stage {
        STAGE_UNSHARE | STAGE_MAP if privilege == Privilege::Userns => format!(
            "Can't mount worktrees. beavy isn't root, and it can't create a user namespace: {err}."
        ),
        STAGE_MOUNT if errno == Some(libc::ENODEV) => {
            format!("Can't mount worktrees. The kernel has no overlayfs: {err}.")
        }
        STAGE_MOUNT if store_fs.as_deref() == Some("overlay") => format!(
            "Can't mount worktrees from store {}. It's on overlayfs, which can't hold another overlay's layers: {err}.",
            store.display()
        ),
        STAGE_MOUNT if matches!(errno, Some(libc::EPERM | libc::EACCES)) => {
            format!("Can't mount worktrees. The kernel refused the overlay mount: {err}.")
        }
        STAGE_MOUNT => format!(
            "Can't mount worktrees from store {} on {}: {err}.",
            store.display(),
            store_fs.as_deref().unwrap_or("this filesystem")
        ),
        STAGE_JOIN => format!("Can't join the worktree's running command in its namespace: {err}."),
        STAGE_USER => format!("Can't run the command as the user who ran sudo: {err}."),
        _ => format!("Can't mount worktrees. Preparing the mount failed at step {stage}: {err}."),
    };
    if container && matches!(errno, Some(libc::EPERM | libc::EACCES)) {
        message.push_str(docker);
    }
    let sysctl = |key: &str| {
        fs::read_to_string(format!("/proc/sys/{}", key.replace('.', "/")))
            .ok()
            .map(|value| value.trim().to_string())
    };
    let fixes = [
        (
            "kernel.apparmor_restrict_unprivileged_userns",
            "1",
            "0",
            "Allow unprivileged user namespaces (Ubuntu 23.10 and later)",
        ),
        (
            "kernel.unprivileged_userns_clone",
            "0",
            "1",
            "Allow unprivileged user namespaces",
        ),
        (
            "user.max_user_namespaces",
            "0",
            "15000",
            "Allow user namespaces",
        ),
    ];
    let sysctl_fix = (privilege == Privilege::Userns
        && matches!(stage, STAGE_UNSHARE | STAGE_MAP | STAGE_MOUNT))
    .then(|| {
        fixes
            .iter()
            .find(|(key, blocked, _, _)| sysctl(key).as_deref() == Some(*blocked))
    })
    .flatten()
    .map(|(key, _, allowed, instruction)| {
        (
            *instruction,
            Next::new(["sudo", "sysctl", "-w", &format!("{key}={allowed}")]),
        )
    });
    let (instruction, next) = sysctl_fix
        .or_else(|| {
            (store_fs.as_deref() == Some("overlay")).then(|| {
                (
                    "Snapshot the project into a store on a volume or tmpfs",
                    Next::new(["beavy", "snapshots", "create", "--store", "<DIR>"]),
                )
            })
        })
        .unwrap_or((
            "See which backends this machine supports",
            Next::new(["beavy", "host", "check"]),
        ));
    Error::new(Reason::BackendUnavailable, message).with_next(instruction, next)
}

/// A warning for a store on overlayfs, such as a container's root filesystem.
pub(crate) fn store_on_overlay(store: &Path) -> Option<crate::envelope::Warning> {
    (crate::host::linux::filesystem_type(store).as_deref() == Some("overlay")).then(|| {
        crate::envelope::Warning {
            reason: crate::envelope::WarningReason::StoreOnOverlayfs,
            message: format!(
                "Store {} is on overlayfs, such as a container's root filesystem. Worktrees stack another overlay on it, which some kernels refuse and every kernel slows down. Use a store on a volume or tmpfs with --store.",
                store.display()
            ),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_list_layers_top_first_with_relative_paths() {
        let layers = vec!["base".to_string(), "mid".into(), "top".into()];
        let options = options(
            &layers,
            Path::new("worktrees/wt_a/upper"),
            Path::new("worktrees/wt_a/work"),
        )
        .unwrap();
        assert_eq!(
            options.to_str().unwrap(),
            "lowerdir=layers/top:layers/mid:layers/base,upperdir=worktrees/wt_a/upper,workdir=worktrees/wt_a/work,userxattr,index=off"
        );
        let deep: Vec<String> = (0..400).map(|i| format!("{i:010}")).collect();
        assert_eq!(
            super::options(&deep, Path::new("u"), Path::new("w"))
                .unwrap_err()
                .reason(),
            Reason::InvalidStore
        );
    }

    #[test]
    fn mount_table_fields_are_unescaped() {
        assert_eq!(unescape(br"/a\040b\134c"), b"/a b\\c");
        assert_eq!(unescape(br"plain\0"), br"plain\0");
        let table = mounts(None).unwrap();
        assert!(table.iter().any(|mount| mount.point == Path::new("/")));
    }
}
