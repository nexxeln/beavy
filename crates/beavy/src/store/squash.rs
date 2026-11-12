//! Squashing: merging a chain's upper layers into one, so lookups stay fast however
//! many times a worktree is sealed.
//!
//! overlayfs looks a name up in every layer until it finds it, so each layer adds a
//! little to every miss and every directory read. Sealing a worktree adds a layer,
//! so a long-lived line of snapshots would slow down without bound. When a seal
//! would make a chain deeper than [`SQUASH_DEPTH`], beavy merges every layer above
//! the base into a single layer instead, so the chain is two layers deep again.
//!
//! Layers never change once written, so the merged layer hard-links regular files
//! from the layers it merges rather than copying them. A squash costs one link per
//! changed file, not one copy per byte.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{self, Permissions};
use std::io::{self, ErrorKind};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use super::{fs_error, tree};
use crate::error::Error;

/// The deepest chain a seal leaves unsquashed. See docs/design/worktrees.md for
/// the measurements behind it.
pub(crate) const SQUASH_DEPTH: usize = 8;

const OPAQUE: &std::ffi::CStr = c"user.overlay.opaque";

/// Whether an entry is an overlayfs whiteout: a character device numbered 0:0.
pub(crate) fn is_whiteout(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_char_device() && metadata.rdev() == 0
}

/// Whether a directory is opaque: it hides the same directory in lower layers.
pub(crate) fn is_opaque(path: &Path) -> bool {
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mut value = [0u8; 4];
    // SAFETY: both strings are NUL-terminated and value has room for 4 bytes.
    let n = unsafe {
        libc::lgetxattr(
            path.as_ptr(),
            OPAQUE.as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    n == 1 && value[0] == b'y'
}

/// Merge `layers`, bottom first, into the empty directory `dest`, so that `dest`
/// over `base` shows exactly what `base` and the whole stack show: whiteouts and
/// opaque directories are kept wherever they hide something in `base`.
pub(crate) fn merge(base: &Path, layers: &[PathBuf], dest: &Path) -> Result<(), Error> {
    let failed = |err: io::Error, action: &str, path: &Path| {
        fs_error(err, format!("{action} {}", path.display()))
    };
    // SAFETY: geteuid has no preconditions.
    let privileged = unsafe { libc::geteuid() } == 0;
    let mut dirs: HashMap<PathBuf, fs::Metadata> = HashMap::new();
    for layer in layers {
        let root = fs::symlink_metadata(layer).map_err(|err| failed(err, "read", layer))?;
        dirs.insert(PathBuf::new(), root);
        let mut pending = vec![PathBuf::new()];
        while let Some(parent) = pending.pop() {
            let from = layer.join(&parent);
            for entry in fs::read_dir(&from).map_err(|err| failed(err, "read", &from))? {
                let entry = entry.map_err(|err| failed(err, "read", &from))?;
                let relative = parent.join(entry.file_name());
                let source = layer.join(&relative);
                let target = dest.join(&relative);
                let metadata =
                    fs::symlink_metadata(&source).map_err(|err| failed(err, "read", &source))?;
                let existing = fs::symlink_metadata(&target).ok();
                if metadata.is_dir() {
                    let opaque = is_opaque(&source);
                    let merges = existing.as_ref().is_some_and(|m| m.is_dir()) && !opaque;
                    if !merges {
                        let hides_below = opaque || existing.is_some();
                        if existing.is_some() {
                            tree::remove(&target).map_err(|err| failed(err, "remove", &target))?;
                        }
                        fs::DirBuilder::new()
                            .mode(0o700)
                            .create(&target)
                            .map_err(|err| failed(err, "create", &target))?;
                        if hides_below {
                            set_opaque(&target).map_err(|err| failed(err, "mark", &target))?;
                        }
                    }
                    dirs.insert(relative.clone(), metadata);
                    pending.push(relative);
                    continue;
                }
                if existing.is_some() {
                    tree::remove(&target).map_err(|err| failed(err, "remove", &target))?;
                    dirs.remove(&relative);
                }
                place(&source, &target, &metadata, privileged)
                    .map_err(|err| failed(err, "create", &target))?;
            }
        }
    }
    let mut dirs: Vec<(PathBuf, fs::Metadata)> = dirs.into_iter().collect();
    dirs.sort_by_key(|(relative, _)| std::cmp::Reverse(relative.components().count()));
    for (relative, metadata) in dirs {
        let path = dest.join(&relative);
        if fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
            attributes(&path, &metadata, privileged, true)
                .map_err(|err| failed(err, "update", &path))?;
        }
    }
    prune(base, dest).map_err(|err| failed(err, "clean up", dest))
}

/// Remove whiteouts that hide nothing in `base`. overlayfs reads a directory that
/// exists in only one layer straight from that layer, so a whiteout left there
/// would show up as an entry.
fn prune(base: &Path, dest: &Path) -> io::Result<()> {
    let mut pending = vec![(PathBuf::new(), true)];
    while let Some((parent, below)) = pending.pop() {
        for entry in fs::read_dir(dest.join(&parent))? {
            let relative = parent.join(entry?.file_name());
            let path = dest.join(&relative);
            let metadata = fs::symlink_metadata(&path)?;
            let under = fs::symlink_metadata(base.join(&relative)).ok();
            if is_whiteout(&metadata) {
                if !below || under.is_none() {
                    fs::remove_file(&path)?;
                }
            } else if metadata.is_dir() {
                let visible = below && !is_opaque(&path) && under.is_some_and(|m| m.is_dir());
                pending.push((relative, visible));
            }
        }
    }
    Ok(())
}

/// Put one non-directory entry in the merged layer: a hard link for a regular
/// file, a copy of anything else.
fn place(
    source: &Path,
    target: &Path,
    metadata: &fs::Metadata,
    privileged: bool,
) -> io::Result<()> {
    let kind = metadata.file_type();
    if kind.is_file() {
        return match fs::hard_link(source, target) {
            Err(err) if err.raw_os_error() == Some(libc::EMLINK) => {
                fs::copy(source, target)?;
                attributes(target, metadata, privileged, true)
            }
            result => result,
        };
    }
    if kind.is_symlink() {
        symlink(fs::read_link(source)?, target)?;
        return attributes(target, metadata, privileged, false);
    }
    let path = CString::new(target.as_os_str().as_bytes()).map_err(io::Error::other)?;
    // SAFETY: path is NUL-terminated. Whiteouts (0:0) need no privilege.
    if unsafe { libc::mknod(path.as_ptr(), metadata.mode(), metadata.rdev()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    attributes(target, metadata, privileged, !kind.is_char_device())
}

fn set_opaque(path: &Path) -> io::Result<()> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    // SAFETY: both strings are NUL-terminated and the value is 1 byte.
    let rc = unsafe { libc::lsetxattr(path.as_ptr(), OPAQUE.as_ptr(), c"y".as_ptr().cast(), 1, 0) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Copy ownership (as root), mode, and times from `metadata` to `path`.
fn attributes(
    path: &Path,
    metadata: &fs::Metadata,
    privileged: bool,
    set_mode: bool,
) -> io::Result<()> {
    if privileged {
        std::os::unix::fs::lchown(path, Some(metadata.uid()), Some(metadata.gid()))?;
    }
    if set_mode {
        fs::set_permissions(path, Permissions::from_mode(metadata.mode() & 0o7777))?;
    }
    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let times = [
        libc::timespec {
            tv_sec: metadata.atime() as libc::time_t,
            tv_nsec: metadata.atime_nsec() as _,
        },
        libc::timespec {
            tv_sec: metadata.mtime() as libc::time_t,
            tv_nsec: metadata.mtime_nsec() as _,
        },
    ];
    // SAFETY: the path is NUL-terminated and times has two entries.
    let rc = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            c_path.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        let err = io::Error::last_os_error();
        if err.kind() != ErrorKind::NotFound {
            return Err(err);
        }
    }
    Ok(())
}

/// The file at `relative` in a stack of layers, top first, as overlayfs would
/// resolve it: the first layer that has it, unless a whiteout, a file in place of
/// a parent directory, or an opaque parent hides the layers below.
pub(crate) fn resolve(layers_top_first: &[PathBuf], relative: &Path) -> Option<PathBuf> {
    let ancestors: Vec<&Path> = relative
        .ancestors()
        .skip(1)
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .collect();
    for layer in layers_top_first {
        let mut blocked = false;
        for ancestor in ancestors.iter().rev() {
            let path = layer.join(ancestor);
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_dir() => blocked |= is_opaque(&path),
                Ok(_) => return None,
                Err(_) => {}
            }
        }
        let path = layer.join(relative);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if is_whiteout(&metadata) => return None,
            Ok(_) => return Some(path),
            Err(_) if blocked => return None,
            Err(_) => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn whiteout(path: &Path) {
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mknod(path.as_ptr(), libc::S_IFCHR, 0) }, 0);
    }

    #[test]
    fn merge_keeps_whiteouts_opaque_directories_and_the_latest_files() {
        let dir = tempfile::tempdir().unwrap();
        let (one, two, out) = (
            dir.path().join("1"),
            dir.path().join("2"),
            dir.path().join("out"),
        );
        fs::create_dir_all(one.join("kept/sub")).unwrap();
        fs::write(one.join("kept/sub/a"), b"one").unwrap();
        fs::write(one.join("replaced"), b"one").unwrap();
        fs::create_dir_all(one.join("wiped")).unwrap();
        fs::write(one.join("wiped/old"), b"one").unwrap();
        fs::write(one.join("file-then-dir"), b"one").unwrap();
        fs::create_dir_all(two.join("kept/sub")).unwrap();
        fs::write(two.join("kept/sub/b"), b"two").unwrap();
        fs::write(two.join("replaced"), b"two").unwrap();
        fs::create_dir_all(two.join("wiped")).unwrap();
        set_opaque(&two.join("wiped")).unwrap();
        fs::write(two.join("wiped/new"), b"two").unwrap();
        whiteout(&two.join("gone"));
        fs::create_dir_all(two.join("file-then-dir")).unwrap();
        let base = dir.path().join("base");
        fs::create_dir_all(base.join("kept")).unwrap();
        fs::write(base.join("gone"), b"base").unwrap();
        fs::create_dir_all(one.join("fresh/x")).unwrap();
        fs::create_dir_all(two.join("fresh")).unwrap();
        whiteout(&two.join("fresh/x"));
        fs::create_dir(&out).unwrap();

        merge(&base, &[one.clone(), two.clone()], &out).unwrap();

        assert_eq!(fs::read(out.join("kept/sub/a")).unwrap(), b"one");
        assert_eq!(fs::read(out.join("kept/sub/b")).unwrap(), b"two");
        assert!(!is_opaque(&out.join("kept")));
        assert_eq!(fs::read(out.join("replaced")).unwrap(), b"two");
        assert_eq!(
            fs::metadata(out.join("replaced")).unwrap().ino(),
            fs::metadata(two.join("replaced")).unwrap().ino()
        );
        assert!(is_opaque(&out.join("wiped")));
        assert!(!out.join("wiped/old").exists());
        assert!(out.join("wiped/new").exists());
        assert!(is_whiteout(
            &fs::symlink_metadata(out.join("gone")).unwrap()
        ));
        assert!(out.join("fresh").is_dir());
        assert!(fs::symlink_metadata(out.join("fresh/x")).is_err());
        assert!(out.join("file-then-dir").is_dir());
        assert!(is_opaque(&out.join("file-then-dir")));

        assert_eq!(
            resolve(&[two.clone(), one.clone()], Path::new("replaced")),
            Some(two.join("replaced"))
        );
        assert_eq!(
            resolve(&[two.clone(), one.clone()], Path::new("kept/sub/a")),
            Some(one.join("kept/sub/a"))
        );
        assert_eq!(
            resolve(&[two.clone(), one.clone()], Path::new("wiped/old")),
            None
        );
        whiteout(&two.join("kept-gone"));
        assert_eq!(resolve(&[two, one], Path::new("kept-gone")), None);
    }
}
