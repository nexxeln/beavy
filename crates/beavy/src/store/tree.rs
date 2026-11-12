//! Directory trees: importing a directory into a layer, and removing trees.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, ErrorKind};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{
    DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink,
};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use super::fs_error;
use crate::error::Error;

const FICLONE: libc::c_ulong = 0x4004_9409;

/// What [`copy`] did.
pub(crate) struct Copied {
    pub files: u64,
    pub bytes: u64,
    /// Every regular file was cloned with FICLONE, and there was at least one.
    pub cloned: bool,
    /// Entries that can't be copied, relative to the source: sockets, and device
    /// nodes when the process isn't root.
    pub skipped: Vec<PathBuf>,
}

/// The attributes a copy keeps.
#[derive(Clone, Copy)]
struct Attrs {
    mode: u32,
    uid: u32,
    gid: u32,
    atime: (i64, i64),
    mtime: (i64, i64),
}

impl Attrs {
    fn of(metadata: &fs::Metadata) -> Self {
        Attrs {
            mode: metadata.mode() & 0o7777,
            uid: metadata.uid(),
            gid: metadata.gid(),
            atime: (metadata.atime(), metadata.atime_nsec()),
            mtime: (metadata.mtime(), metadata.mtime_nsec()),
        }
    }
}

struct Node {
    relative: PathBuf,
    attrs: Attrs,
}

/// Copy the tree at `source` into the existing, empty directory `dest`. Keeps
/// modes, times, symlinks, FIFOs, and hard links within the tree, and ownership
/// when running as root. Regular files are cloned with FICLONE while the
/// filesystem allows it, and copied otherwise. Skips the directory whose device
/// and inode are `exclude`, such as a store inside the tree.
pub(crate) fn copy(
    source: &Path,
    dest: &Path,
    exclude: Option<(u64, u64)>,
) -> Result<Copied, Error> {
    let failed = |err: io::Error, action: &str, path: &Path| {
        fs_error(err, format!("{action} {}", path.display()))
    };
    // SAFETY: geteuid has no preconditions.
    let privileged = unsafe { libc::geteuid() } == 0;
    let root = fs::symlink_metadata(source).map_err(|err| failed(err, "read", source))?;
    let mut dirs = vec![Node {
        relative: PathBuf::new(),
        attrs: Attrs::of(&root),
    }];
    let mut files = Vec::new();
    let mut links = Vec::new();
    let mut seen: HashMap<(u64, u64), PathBuf> = HashMap::new();
    let mut skipped = Vec::new();
    let mut bytes = 0;

    let mut next_dir = 0;
    while next_dir < dirs.len() {
        let parent = dirs[next_dir].relative.clone();
        next_dir += 1;
        let from = source.join(&parent);
        let entries = fs::read_dir(&from).map_err(|err| failed(err, "read", &from))?;
        for entry in entries {
            let entry = entry.map_err(|err| failed(err, "read", &from))?;
            let relative = parent.join(entry.file_name());
            let metadata = entry
                .metadata()
                .map_err(|err| failed(err, "read", &source.join(&relative)))?;
            let kind = metadata.file_type();
            let attrs = Attrs::of(&metadata);
            let to = dest.join(&relative);
            if kind.is_dir() {
                if exclude == Some((metadata.dev(), metadata.ino())) {
                    continue;
                }
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(&to)
                    .map_err(|err| failed(err, "create", &to))?;
                dirs.push(Node { relative, attrs });
            } else if kind.is_file() {
                if metadata.nlink() > 1 {
                    match seen.entry((metadata.dev(), metadata.ino())) {
                        Entry::Occupied(first) => {
                            links.push((first.get().clone(), relative));
                            continue;
                        }
                        Entry::Vacant(slot) => {
                            slot.insert(relative.clone());
                        }
                    }
                }
                bytes += metadata.len();
                files.push(Node { relative, attrs });
            } else if kind.is_symlink() {
                let from = source.join(&relative);
                let target = fs::read_link(&from).map_err(|err| failed(err, "read", &from))?;
                symlink(&target, &to).map_err(|err| failed(err, "create", &to))?;
                finish(&to, attrs, privileged, false).map_err(|err| failed(err, "update", &to))?;
            } else if kind.is_fifo() {
                let path = c_path(&to)?;
                // SAFETY: path is NUL-terminated.
                if unsafe { libc::mkfifo(path.as_ptr(), 0o600) } != 0 {
                    return Err(failed(io::Error::last_os_error(), "create", &to));
                }
                finish(&to, attrs, privileged, true).map_err(|err| failed(err, "update", &to))?;
            } else {
                skipped.push(relative);
            }
        }
    }

    let next = AtomicUsize::new(0);
    let try_clone = AtomicBool::new(true);
    let cloned = AtomicU64::new(0);
    let failure: Mutex<Option<Error>> = Mutex::new(None);
    let stop = AtomicBool::new(false);
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    let Some(file) = files.get(next.fetch_add(1, Ordering::Relaxed)) else {
                        break;
                    };
                    match copy_file(source, dest, file, privileged, &try_clone) {
                        Ok(true) => {
                            cloned.fetch_add(1, Ordering::Relaxed);
                        }
                        Ok(false) => {}
                        Err(err) => {
                            stop.store(true, Ordering::Relaxed);
                            failure
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .get_or_insert(err);
                        }
                    }
                }
            });
        }
    });
    if let Some(err) = failure.into_inner().unwrap_or_else(|e| e.into_inner()) {
        return Err(err);
    }

    for (first, relative) in &links {
        let (from, to) = (dest.join(first), dest.join(relative));
        fs::hard_link(&from, &to).map_err(|err| failed(err, "link", &to))?;
    }
    for dir in dirs.iter().rev() {
        let path = dest.join(&dir.relative);
        finish(&path, dir.attrs, privileged, true).map_err(|err| failed(err, "update", &path))?;
    }

    let count = files.len() as u64;
    Ok(Copied {
        files: count + links.len() as u64,
        bytes,
        cloned: count > 0 && cloned.load(Ordering::Relaxed) == count,
        skipped,
    })
}

/// Copy one regular file. Returns whether it was cloned.
fn copy_file(
    source: &Path,
    dest: &Path,
    file: &Node,
    privileged: bool,
    try_clone: &AtomicBool,
) -> Result<bool, Error> {
    let (from, to) = (source.join(&file.relative), dest.join(&file.relative));
    let mut input =
        File::open(&from).map_err(|err| fs_error(err, format!("read {}", from.display())))?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&to)
        .map_err(|err| fs_error(err, format!("create {}", to.display())))?;
    let mut cloned = false;
    if try_clone.load(Ordering::Relaxed) {
        // SAFETY: both descriptors are open for the duration of the call.
        let rc = unsafe { libc::ioctl(output.as_raw_fd(), FICLONE as _, input.as_raw_fd()) };
        if rc == 0 {
            cloned = true;
        } else {
            try_clone.store(false, Ordering::Relaxed);
        }
    }
    if !cloned {
        io::copy(&mut input, &mut output)
            .map_err(|err| fs_error(err, format!("copy {}", from.display())))?;
    }
    let attrs = file.attrs;
    let set = || -> io::Result<()> {
        if privileged {
            std::os::unix::fs::fchown(&output, Some(attrs.uid), Some(attrs.gid))?;
        }
        output.set_permissions(Permissions::from_mode(attrs.mode))?;
        let times = [timespec(attrs.atime), timespec(attrs.mtime)];
        // SAFETY: the descriptor is open and times has two entries.
        if unsafe { libc::futimens(output.as_raw_fd(), times.as_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    };
    set().map_err(|err| fs_error(err, format!("update {}", to.display())))?;
    Ok(cloned)
}

/// Apply ownership (as root), mode (unless it's a symlink), and times to a path.
fn finish(path: &Path, attrs: Attrs, privileged: bool, set_mode: bool) -> io::Result<()> {
    if privileged {
        std::os::unix::fs::lchown(path, Some(attrs.uid), Some(attrs.gid))?;
    }
    if set_mode {
        fs::set_permissions(path, Permissions::from_mode(attrs.mode))?;
    }
    let path = CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let times = [timespec(attrs.atime), timespec(attrs.mtime)];
    // SAFETY: path is NUL-terminated and times has two entries.
    let rc = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            path.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn timespec((secs, nanos): (i64, i64)) -> libc::timespec {
    libc::timespec {
        tv_sec: secs as libc::time_t,
        tv_nsec: nanos as _,
    }
}

fn c_path(path: &Path) -> Result<CString, Error> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|err| fs_error(io::Error::other(err), format!("use {}", path.display())))
}

/// Remove a directory tree, including directories whose mode denies the owner
/// access (overlayfs leaves its work directory at mode 000). A missing path is
/// already removed.
pub(crate) fn remove(path: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if !metadata.is_dir() {
        return fs::remove_file(path);
    }
    match fs::remove_dir_all(path) {
        Err(err) if err.kind() == ErrorKind::PermissionDenied => {
            open_up(path)?;
            fs::remove_dir_all(path)
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn open_up(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Ok(());
    }
    if metadata.mode() & 0o700 != 0o700 {
        fs::set_permissions(
            path,
            Permissions::from_mode((metadata.mode() & 0o7777) | 0o700),
        )?;
    }
    for entry in fs::read_dir(path)? {
        open_up(&entry?.path())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    #[test]
    fn copy_keeps_contents_modes_times_links_and_fifos() {
        let src = tempfile::tempdir().unwrap();
        let s = src.path();
        fs::create_dir_all(s.join("a/b")).unwrap();
        fs::write(s.join("a/b/file"), b"hello").unwrap();
        fs::set_permissions(s.join("a/b/file"), Permissions::from_mode(0o751)).unwrap();
        fs::hard_link(s.join("a/b/file"), s.join("a/twin")).unwrap();
        symlink("b/file", s.join("a/link")).unwrap();
        symlink("missing", s.join("dangling")).unwrap();
        let fifo = CString::new(s.join("pipe").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o640) }, 0);
        fs::create_dir(s.join("ro")).unwrap();
        fs::write(s.join("ro/x"), b"x").unwrap();
        fs::set_permissions(s.join("ro"), Permissions::from_mode(0o555)).unwrap();
        let old = [timespec((1_000_000, 5)), timespec((1_000_000, 5))];
        let file = File::open(s.join("a/b/file")).unwrap();
        assert_eq!(unsafe { libc::futimens(file.as_raw_fd(), old.as_ptr()) }, 0);
        fs::create_dir_all(s.join("store/layers")).unwrap();
        let exclude = super::super::identity(&s.join("store"));

        let dst = tempfile::tempdir().unwrap();
        let d = dst.path().join("layer");
        fs::create_dir(&d).unwrap();
        let copied = copy(s, &d, exclude).unwrap();

        assert_eq!(copied.files, 3);
        assert_eq!(copied.bytes, 6);
        assert!(copied.skipped.is_empty());
        assert_eq!(fs::read(d.join("a/b/file")).unwrap(), b"hello");
        let meta = fs::metadata(d.join("a/b/file")).unwrap();
        assert_eq!(meta.mode() & 0o7777, 0o751);
        assert_eq!((meta.mtime(), meta.mtime_nsec()), (1_000_000, 5));
        assert_eq!(meta.ino(), fs::metadata(d.join("a/twin")).unwrap().ino());
        assert_eq!(
            fs::read_link(d.join("a/link")).unwrap(),
            Path::new("b/file")
        );
        assert_eq!(
            fs::read_link(d.join("dangling")).unwrap(),
            Path::new("missing")
        );
        let pipe = fs::symlink_metadata(d.join("pipe")).unwrap();
        assert!(pipe.file_type().is_fifo());
        assert_eq!(pipe.mode() & 0o777, 0o640);
        assert_eq!(fs::metadata(d.join("ro")).unwrap().mode() & 0o777, 0o555);
        assert!(!d.join("store").exists());

        fs::set_permissions(s.join("ro"), Permissions::from_mode(0o755)).unwrap();
        remove(&d).unwrap();
        assert!(!d.exists());
    }

    #[test]
    fn remove_handles_unreadable_directories_and_missing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("t");
        fs::create_dir_all(tree.join("work/work/inner")).unwrap();
        fs::set_permissions(tree.join("work/work"), Permissions::from_mode(0o0)).unwrap();
        remove(&tree).unwrap();
        assert!(!tree.exists());
        remove(&tree).unwrap();
        fs::write(&tree, b"file").unwrap();
        remove(&tree).unwrap();
        assert!(!tree.exists());
    }
}
