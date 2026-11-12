//! Host check: which worktree backends this machine can support.
//!
//! Every answer comes from evidence gathered on this machine: syscalls tried in a
//! temporary directory, device nodes opened, kernel tables read. When the evidence
//! doesn't settle a question the answer is `unknown`, never a guess. The check
//! never needs root, and it removes everything it creates before returning.

use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind};
use std::os::unix::fs::FileTypeExt;
use std::path::Path;

use serde::Serialize;

use crate::envelope::{Warning, WarningReason};
use crate::error::{Error, Reason};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("beavy supports Linux and macOS");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Support {
    Supported,
    Unsupported,
    Unknown,
}

impl Support {
    pub const fn as_str(self) -> &'static str {
        match self {
            Support::Supported => "supported",
            Support::Unsupported => "unsupported",
            Support::Unknown => "unknown",
        }
    }
}

/// One fact about the host, with the evidence behind it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    pub status: Support,
    pub reason: String,
}

impl Check {
    fn new(status: Support, reason: impl Into<String>) -> Self {
        Check {
            status,
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Os {
    /// `linux` or `macos`.
    pub name: &'static str,
    /// The kernel release on Linux and the product version on macOS; null when unknown.
    pub version: Option<String>,
    /// The kernel release (`uname -r`); null when unknown.
    pub kernel: Option<String>,
    pub arch: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct Directory {
    /// The canonical path that was checked.
    pub path: String,
    /// The filesystem type, such as `ext4` or `apfs`; null when unknown.
    pub filesystem: Option<String>,
    /// Whether beavy could create files here; null when unknown.
    pub writable: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Checks {
    /// Copy-on-write file clones in the directory: FICLONE on Linux, clonefile on macOS.
    pub file_clone: Check,
    /// The kernel provides overlayfs.
    pub overlayfs: Check,
    /// This process can mount overlayfs in the directory, as a privileged process or
    /// in an unprivileged user namespace.
    pub overlayfs_mount: Check,
    /// This process can create a user namespace and map its IDs into it.
    pub user_namespaces: Check,
    /// /dev/fuse exists and this process can open it.
    pub fuse: Check,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendName {
    Overlayfs,
    Clonefile,
    Reflink,
    Fuse,
    Copy,
}

impl BackendName {
    pub const fn as_str(self) -> &'static str {
        match self {
            BackendName::Overlayfs => "overlayfs",
            BackendName::Clonefile => "clonefile",
            BackendName::Reflink => "reflink",
            BackendName::Fuse => "fuse",
            BackendName::Copy => "copy",
        }
    }
}

/// Whether a worktree backend can work for this directory on this host.
#[derive(Debug, Clone, Serialize)]
pub struct Backend {
    pub name: BackendName,
    pub status: Support,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub os: Os,
    pub directory: Directory,
    /// The effective user is root.
    pub root: bool,
    pub checks: Checks,
    /// Every backend, in beavy's order of preference.
    pub backends: Vec<Backend>,
    /// Problems the check ran into, such as a probe it couldn't remove.
    #[serde(skip)]
    pub warnings: Vec<Warning>,
}

/// Probe results that depend on the platform.
pub(crate) struct PlatformChecks {
    pub file_clone: Check,
    pub overlayfs: Check,
    pub overlayfs_mount: Check,
    pub user_namespaces: Check,
}

/// Check which worktree backends `directory` supports on this host.
pub fn check(directory: &Path) -> Result<Report, Error> {
    let shown = directory.display();
    let metadata = fs::metadata(directory).map_err(|err| match err.kind() {
        ErrorKind::NotFound => Error::new(
            Reason::NotFound,
            format!("Couldn't find directory {shown}."),
        )
        .with_cause(err),
        ErrorKind::PermissionDenied => Error::new(
            Reason::PermissionDenied,
            format!("Can't read directory {shown}. Permission denied."),
        )
        .with_cause(err),
        _ => Error::new(
            Reason::IoError,
            format!("Failed to read directory {shown}."),
        )
        .with_cause(err),
    })?;
    if !metadata.is_dir() {
        return Err(Error::new(
            Reason::InvalidArguments,
            format!("Can't check {shown}. It isn't a directory."),
        ));
    }
    let directory = fs::canonicalize(directory).map_err(|err| {
        Error::new(
            Reason::IoError,
            format!("Failed to resolve directory {shown}."),
        )
        .with_cause(err)
    })?;

    let filesystem = platform::filesystem_type(&directory);
    let fs_name = filesystem.as_deref().unwrap_or("this filesystem");
    let mut warnings = Vec::new();

    let probe = tempfile::Builder::new()
        .prefix(".beavy-host-check-")
        .tempdir_in(&directory);
    let writable = match &probe {
        Ok(_) => Some(true),
        Err(err)
            if matches!(
                err.kind(),
                ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem
            ) =>
        {
            Some(false)
        }
        Err(_) => None,
    };

    let platform_checks = match probe {
        Ok(probe) => {
            let checks = platform::probe(probe.path(), fs_name);
            let path = probe.path().to_path_buf();
            if let Err(err) = probe.close() {
                warnings.push(cleanup_warning(&path, &err));
            } else if path.exists() {
                warnings.push(cleanup_warning(&path, &io::Error::other("it still exists")));
            }
            checks
        }
        Err(err) => {
            let text = err.to_string();
            let cause = text.split(" at path ").next().unwrap_or(&text);
            let reason = format!(
                "Couldn't create a probe directory in {}: {cause}.",
                directory.display()
            );
            platform::without_probe(&reason)
        }
    };

    let checks = Checks {
        file_clone: platform_checks.file_clone,
        overlayfs: platform_checks.overlayfs,
        overlayfs_mount: platform_checks.overlayfs_mount,
        user_namespaces: platform_checks.user_namespaces,
        fuse: fuse(),
    };
    let backends = backends(&checks, writable, &directory);
    let kernel = platform::kernel_release();

    Ok(Report {
        os: Os {
            name: platform::NAME,
            version: platform::os_version(kernel.as_deref()),
            kernel,
            arch: std::env::consts::ARCH,
        },
        directory: Directory {
            path: directory.to_string_lossy().into_owned(),
            filesystem,
            writable,
        },
        // SAFETY: geteuid has no preconditions and can't fail.
        root: unsafe { libc::geteuid() } == 0,
        checks,
        backends,
        warnings,
    })
}

fn cleanup_warning(path: &Path, err: &io::Error) -> Warning {
    Warning {
        reason: WarningReason::CleanupFailed,
        message: format!(
            "Couldn't remove the probe directory {}: {err}. Delete it by hand.",
            path.display()
        ),
    }
}

fn fuse() -> Check {
    let device = Path::new("/dev/fuse");
    match fs::metadata(device) {
        Err(err) if err.kind() == ErrorKind::NotFound => {
            return Check::new(Support::Unsupported, "/dev/fuse doesn't exist.");
        }
        Err(err) => {
            return Check::new(
                Support::Unknown,
                format!("Couldn't inspect /dev/fuse: {err}."),
            );
        }
        Ok(metadata) if !metadata.file_type().is_char_device() => {
            return Check::new(Support::Unsupported, "/dev/fuse isn't a character device.");
        }
        Ok(_) => {}
    }
    match OpenOptions::new().read(true).write(true).open(device) {
        Ok(_) => Check::new(
            Support::Supported,
            "Opened /dev/fuse for reading and writing.",
        ),
        Err(err) if err.kind() == ErrorKind::PermissionDenied => Check::new(
            Support::Unsupported,
            format!("Can't open /dev/fuse for reading and writing: {err}."),
        ),
        Err(err) => Check::new(Support::Unknown, format!("Couldn't open /dev/fuse: {err}.")),
    }
}

fn backends(checks: &Checks, writable: Option<bool>, directory: &Path) -> Vec<Backend> {
    let from = |name, check: &Check| Backend {
        name,
        status: check.status,
        reason: check.reason.clone(),
    };
    let only_on = |name, os: &str| Backend {
        name,
        status: Support::Unsupported,
        reason: format!("{} is only available on {os}.", BackendName::as_str(name)),
    };

    let overlayfs = if platform::NAME != "linux" {
        only_on(BackendName::Overlayfs, "Linux")
    } else if checks.overlayfs.status == Support::Unsupported {
        from(BackendName::Overlayfs, &checks.overlayfs)
    } else {
        from(BackendName::Overlayfs, &checks.overlayfs_mount)
    };
    let (clonefile, reflink) = if platform::NAME == "macos" {
        (
            from(BackendName::Clonefile, &checks.file_clone),
            only_on(BackendName::Reflink, "Linux"),
        )
    } else {
        (
            only_on(BackendName::Clonefile, "macOS"),
            from(BackendName::Reflink, &checks.file_clone),
        )
    };
    let copy = match writable {
        Some(true) => Backend {
            name: BackendName::Copy,
            status: Support::Supported,
            reason: "Copies every file, so it works on any writable filesystem.".into(),
        },
        Some(false) => Backend {
            name: BackendName::Copy,
            status: Support::Unsupported,
            reason: format!("Can't write to {}.", directory.display()),
        },
        None => Backend {
            name: BackendName::Copy,
            status: Support::Unknown,
            reason: format!("Couldn't tell whether {} is writable.", directory.display()),
        },
    };

    vec![
        overlayfs,
        clonefile,
        reflink,
        from(BackendName::Fuse, &checks.fuse),
        copy,
    ]
}

/// Read a NUL-terminated C string out of a fixed buffer, such as a `utsname` field.
pub(crate) fn c_chars_to_string(chars: &[libc::c_char]) -> Option<String> {
    let bytes: Vec<u8> = chars
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    let text = String::from_utf8(bytes).ok()?;
    (!text.is_empty()).then_some(text)
}

/// The kernel release from `uname`.
pub(crate) fn uname_release() -> Option<String> {
    // SAFETY: utsname is plain data, and uname fills it or fails without side effects.
    let mut name: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut name) } != 0 {
        return None;
    }
    c_chars_to_string(&name.release)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_reports_every_backend_and_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let report = check(dir.path()).unwrap();
        let names: Vec<_> = report.backends.iter().map(|b| b.name).collect();
        assert_eq!(
            names,
            [
                BackendName::Overlayfs,
                BackendName::Clonefile,
                BackendName::Reflink,
                BackendName::Fuse,
                BackendName::Copy
            ]
        );
        assert_eq!(report.directory.writable, Some(true));
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
        assert!(report.backends.iter().all(|b| !b.reason.is_empty()));
    }

    #[test]
    fn missing_directory_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = check(&dir.path().join("missing")).unwrap_err();
        assert_eq!(err.reason(), Reason::NotFound);
    }

    #[test]
    fn a_file_is_invalid_input() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let err = check(file.path()).unwrap_err();
        assert_eq!(err.reason(), Reason::InvalidArguments);
    }

    #[test]
    fn report_serializes_with_camel_case_check_keys() {
        let dir = tempfile::tempdir().unwrap();
        let value = serde_json::to_value(check(dir.path()).unwrap()).unwrap();
        for key in [
            "fileClone",
            "overlayfs",
            "overlayfsMount",
            "userNamespaces",
            "fuse",
        ] {
            let status = value["checks"][key]["status"].as_str().unwrap();
            assert!(["supported", "unsupported", "unknown"].contains(&status));
        }
        assert!(value.get("warnings").is_none());
    }

    #[test]
    fn c_strings_stop_at_nul() {
        let chars: Vec<libc::c_char> = b"6.1.0\0junk".iter().map(|&b| b as libc::c_char).collect();
        assert_eq!(c_chars_to_string(&chars).as_deref(), Some("6.1.0"));
        assert_eq!(c_chars_to_string(&[0]), None);
    }
}
