use std::ffi::CString;
use std::fs::File;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::{Check, PlatformChecks, Support, c_chars_to_string, uname_release};

pub(super) const NAME: &str = "macos";

unsafe extern "C" {
    fn clonefile(src: *const libc::c_char, dst: *const libc::c_char, flags: u32) -> libc::c_int;
}

pub(super) fn kernel_release() -> Option<String> {
    uname_release()
}

/// The macOS product version, such as `15.1`, from `kern.osproductversion`.
pub(super) fn os_version(_kernel: Option<&str>) -> Option<String> {
    let mut buf = [0 as libc::c_char; 64];
    let mut len = buf.len();
    // SAFETY: buf and len describe a writable buffer; the name is NUL-terminated.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.osproductversion".as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    c_chars_to_string(&buf[..len.min(buf.len())])
}

pub(super) fn filesystem_type(path: &Path) -> Option<String> {
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: statfs is plain data; the call fills it or fails.
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    c_chars_to_string(&stat.f_fstypename)
}

fn linux_only(what: &str) -> Check {
    Check::new(
        Support::Unsupported,
        format!("{what} is only available on Linux."),
    )
}

pub(super) fn without_probe(reason: &str) -> PlatformChecks {
    PlatformChecks {
        file_clone: Check::new(Support::Unknown, reason),
        overlayfs: linux_only("overlayfs"),
        overlayfs_mount: linux_only("overlayfs"),
        user_namespaces: linux_only("A user namespace"),
    }
}

pub(super) fn probe(dir: &Path, fs_name: &str) -> PlatformChecks {
    PlatformChecks {
        file_clone: file_clone(dir, fs_name),
        ..without_probe("")
    }
}

fn file_clone(dir: &Path, fs_name: &str) -> Check {
    let attempt = || -> io::Result<()> {
        let source_path = dir.join("clone-source");
        let mut source = File::create_new(&source_path)?;
        source.write_all(b"beavy host check\n")?;
        source.sync_all()?;
        let source = CString::new(source_path.as_os_str().as_bytes()).map_err(io::Error::other)?;
        let target = CString::new(dir.join("clone-target").as_os_str().as_bytes())
            .map_err(io::Error::other)?;
        // SAFETY: both paths are NUL-terminated and outlive the call.
        if unsafe { clonefile(source.as_ptr(), target.as_ptr(), 0) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    };
    match attempt() {
        Ok(()) => Check::new(
            Support::Supported,
            format!("Cloned a file on {fs_name} with clonefile."),
        ),
        Err(err) if matches!(err.raw_os_error(), Some(libc::ENOTSUP | libc::EXDEV)) => Check::new(
            Support::Unsupported,
            format!("{fs_name} doesn't support clonefile: {err}."),
        ),
        Err(err) => Check::new(
            Support::Unknown,
            format!("Couldn't test clonefile on {fs_name}: {err}."),
        ),
    }
}
