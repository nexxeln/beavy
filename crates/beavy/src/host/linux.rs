use std::ffi::CString;
use std::fs::{self, File, Permissions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use super::{Check, PlatformChecks, Support, uname_release};

pub(super) const NAME: &str = "linux";

const FICLONE: libc::c_ulong = 0x4004_9409;
const CAP_SYS_ADMIN: u32 = 21;

pub(super) fn kernel_release() -> Option<String> {
    uname_release()
}

pub(super) fn os_version(kernel: Option<&str>) -> Option<String> {
    kernel.map(str::to_owned)
}

/// The filesystem type of `path`, from the mount table entry whose device matches
/// the path's device, falling back to statfs magic numbers that name exactly one
/// filesystem.
pub(super) fn filesystem_type(path: &Path) -> Option<String> {
    let dev = fs::metadata(path).ok()?.dev();
    let (major, minor) = (
        ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff),
        (dev & 0xff) | ((dev >> 12) & !0xff),
    );
    let device = format!("{major}:{minor}");
    if let Ok(mountinfo) = fs::read_to_string("/proc/self/mountinfo") {
        let found = mountinfo
            .lines()
            .filter(|line| line.split(' ').nth(2) == Some(device.as_str()))
            .filter_map(|line| line.split_once(" - ")?.1.split(' ').next())
            .next_back();
        if let Some(fstype) = found {
            return Some(fstype.to_owned());
        }
    }
    statfs_magic_name(path)
}

fn statfs_magic_name(path: &Path) -> Option<String> {
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: statfs is plain data; the call fills it or fails.
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let name = match stat.f_type as u64 {
        0x9123_683e => "btrfs",
        0x5846_5342 => "xfs",
        0x0102_1994 => "tmpfs",
        0x794c_7630 => "overlay",
        0x2fc1_2fc1 => "zfs",
        0x6969 => "nfs",
        0xf2f5_2010 => "f2fs",
        0x6573_5546 => "fuse",
        _ => return None,
    };
    Some(name.to_owned())
}

pub(super) fn without_probe(reason: &str) -> PlatformChecks {
    PlatformChecks {
        file_clone: Check::new(Support::Unknown, reason),
        overlayfs: overlayfs_available(),
        overlayfs_mount: Check::new(Support::Unknown, reason),
        user_namespaces: user_namespaces(),
    }
}

pub(super) fn probe(dir: &Path, fs_name: &str) -> PlatformChecks {
    let (mount, kernel_lacks_overlay) = overlayfs_mount(dir, fs_name);
    let overlayfs = if mount.status == Support::Supported {
        Check::new(
            Support::Supported,
            "Mounted overlayfs in a probe directory.",
        )
    } else if kernel_lacks_overlay {
        Check::new(
            Support::Unsupported,
            "The kernel has no overlayfs: mounting it failed with ENODEV.",
        )
    } else {
        overlayfs_available()
    };
    PlatformChecks {
        file_clone: file_clone(dir, fs_name),
        overlayfs,
        overlayfs_mount: mount,
        user_namespaces: user_namespaces(),
    }
}

fn file_clone(dir: &Path, fs_name: &str) -> Check {
    let attempt = || -> io::Result<()> {
        let mut source = File::create_new(dir.join("clone-source"))?;
        source.write_all(b"beavy host check\n")?;
        source.sync_all()?;
        let target = File::create_new(dir.join("clone-target"))?;
        // SAFETY: both descriptors are open for the duration of the call.
        let rc = unsafe { libc::ioctl(target.as_raw_fd(), FICLONE as _, source.as_raw_fd()) };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    };
    match attempt() {
        Ok(()) => Check::new(
            Support::Supported,
            format!("Cloned a file on {fs_name} with FICLONE."),
        ),
        Err(err)
            if matches!(
                err.raw_os_error(),
                Some(libc::EOPNOTSUPP | libc::EINVAL | libc::ENOTTY | libc::EXDEV | libc::ENOSYS)
            ) =>
        {
            Check::new(
                Support::Unsupported,
                format!("{fs_name} doesn't support reflinks: FICLONE failed with {err}."),
            )
        }
        Err(err) => Check::new(
            Support::Unknown,
            format!("Couldn't test FICLONE on {fs_name}: {err}."),
        ),
    }
}

fn overlayfs_available() -> Check {
    match fs::read_to_string("/proc/filesystems") {
        Ok(table)
            if table
                .lines()
                .any(|line| line.split_whitespace().last() == Some("overlay")) =>
        {
            Check::new(
                Support::Supported,
                "overlay is listed in /proc/filesystems.",
            )
        }
        _ if Path::new("/sys/module/overlay").exists() => {
            Check::new(Support::Supported, "The overlay module is loaded.")
        }
        Ok(_) => Check::new(
            Support::Unknown,
            "overlay isn't in /proc/filesystems; the kernel may still load it on first mount.",
        ),
        Err(err) => Check::new(
            Support::Unknown,
            format!("Couldn't read /proc/filesystems: {err}."),
        ),
    }
}

fn has_cap_sys_admin() -> bool {
    let Ok(status) = fs::read_to_string("/proc/self/status") else {
        return false;
    };
    status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
        .is_some_and(|caps| caps & (1 << CAP_SYS_ADMIN) != 0)
}

/// Files and contents for entering a user namespace with this process's IDs mapped
/// to root, prepared before fork so the child only makes syscalls.
struct IdMaps {
    setgroups: CString,
    uid_map: CString,
    gid_map: CString,
    deny: Vec<u8>,
    uid_line: Vec<u8>,
    gid_line: Vec<u8>,
}

impl IdMaps {
    fn new() -> Self {
        // SAFETY: geteuid and getegid have no preconditions.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        IdMaps {
            setgroups: c"/proc/self/setgroups".to_owned(),
            uid_map: c"/proc/self/uid_map".to_owned(),
            gid_map: c"/proc/self/gid_map".to_owned(),
            deny: b"deny".to_vec(),
            uid_line: format!("0 {uid} 1").into_bytes(),
            gid_line: format!("0 {gid} 1").into_bytes(),
        }
    }

    /// Runs in the forked child: async-signal-safe calls only.
    fn enter(&self) -> Result<(), (u8, i32)> {
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

const STAGE_UNSHARE: u8 = 1;
const STAGE_MAP: u8 = 2;
const STAGE_CHDIR: u8 = 3;
const STAGE_PRIVATE: u8 = 4;
const STAGE_MOUNT: u8 = 5;

fn errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
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

enum ChildResult {
    Succeeded,
    Failed { stage: u8, err: io::Error },
    Abnormal(String),
}

/// Run `work` in a forked child so namespace changes never touch this process.
/// `work` must make only async-signal-safe calls: the parent may have other threads.
fn in_child(work: impl FnOnce() -> Result<(), (u8, i32)>) -> ChildResult {
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
                "Couldn't wait for the probe process: {}",
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
            "The probe process was stopped by signal {}",
            libc::WTERMSIG(status)
        ));
    }
    ChildResult::Abnormal("The probe process exited without a result".into())
}

fn user_namespaces() -> Check {
    let maps = IdMaps::new();
    match in_child(|| maps.enter()) {
        ChildResult::Succeeded => Check::new(
            Support::Supported,
            "Created a user namespace and mapped this user to root in it.",
        ),
        ChildResult::Failed {
            stage: STAGE_UNSHARE,
            err,
        } => Check::new(
            Support::Unsupported,
            format!("Can't create a user namespace: {err}."),
        ),
        ChildResult::Failed { err, .. } => Check::new(
            Support::Unsupported,
            format!("Created a user namespace but can't map IDs in it: {err}."),
        ),
        ChildResult::Abnormal(reason) => Check::new(Support::Unknown, format!("{reason}.")),
    }
}

/// Try an overlay mount in `dir`. The flag is true when the kernel reported that it
/// has no overlayfs.
fn overlayfs_mount(dir: &Path, fs_name: &str) -> (Check, bool) {
    let overlay = dir.join("overlay");
    let layout = ["lower", "upper", "work", "merged"]
        .iter()
        .try_for_each(|name| fs::create_dir_all(overlay.join(name)));
    if let Err(err) = layout {
        let reason = format!("Couldn't create overlay probe directories: {err}.");
        return (Check::new(Support::Unknown, reason), false);
    }
    let Ok(overlay_c) = CString::new(overlay.as_os_str().as_bytes()) else {
        let reason = "The directory path contains a NUL byte.";
        return (Check::new(Support::Unknown, reason), false);
    };

    let privileged = has_cap_sys_admin();
    let maps = IdMaps::new();
    let source = c"overlay";
    let target = c"merged";
    let options = c"lowerdir=lower,upperdir=upper,workdir=work";
    let root = c"/";

    let result = in_child(|| {
        // SAFETY: every pointer is a NUL-terminated string prepared before fork.
        unsafe {
            if privileged {
                if libc::unshare(libc::CLONE_NEWNS) != 0 {
                    return Err((STAGE_UNSHARE, errno()));
                }
            } else {
                maps.enter()?;
            }
            if libc::mount(
                std::ptr::null(),
                root.as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            ) != 0
            {
                return Err((STAGE_PRIVATE, errno()));
            }
            if libc::chdir(overlay_c.as_ptr()) != 0 {
                return Err((STAGE_CHDIR, errno()));
            }
            if libc::mount(
                source.as_ptr(),
                target.as_ptr(),
                source.as_ptr(),
                0,
                options.as_ptr().cast(),
            ) != 0
            {
                return Err((STAGE_MOUNT, errno()));
            }
            libc::umount2(target.as_ptr(), libc::MNT_DETACH);
        }
        Ok(())
    });
    let _ = fs::set_permissions(overlay.join("work/work"), Permissions::from_mode(0o700));

    let how = if privileged {
        "as a privileged process"
    } else {
        "in an unprivileged user namespace"
    };
    let kernel_lacks_overlay = matches!(
        &result,
        ChildResult::Failed { stage: STAGE_MOUNT, err } if err.raw_os_error() == Some(libc::ENODEV)
    );
    let check = match result {
        ChildResult::Succeeded => Check::new(
            Support::Supported,
            format!("Mounted overlayfs on {fs_name} {how}."),
        ),
        ChildResult::Failed {
            stage: STAGE_UNSHARE | STAGE_MAP,
            err,
        } if !privileged => Check::new(
            Support::Unsupported,
            format!("The process lacks CAP_SYS_ADMIN and can't use a user namespace: {err}."),
        ),
        ChildResult::Failed {
            stage: STAGE_MOUNT,
            err,
        } => match err.raw_os_error() {
            Some(libc::ENODEV) => Check::new(
                Support::Unsupported,
                format!("The kernel has no overlayfs: mounting it failed with {err}."),
            ),
            Some(libc::EPERM | libc::EACCES) => Check::new(
                Support::Unsupported,
                format!("The kernel refused an overlay mount {how}: {err}."),
            ),
            Some(libc::EINVAL) => Check::new(
                Support::Unsupported,
                format!("The kernel rejected an overlay upper layer on {fs_name}: {err}."),
            ),
            _ => Check::new(
                Support::Unknown,
                format!("Mounting overlayfs {how} failed: {err}."),
            ),
        },
        ChildResult::Failed { stage, err } => Check::new(
            Support::Unknown,
            format!("Preparing an overlay mount {how} failed at step {stage}: {err}."),
        ),
        ChildResult::Abnormal(reason) => Check::new(Support::Unknown, format!("{reason}.")),
    };
    (check, kernel_lacks_overlay)
}
