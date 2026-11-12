//! Directory trees: importing a directory into a layer, and removing trees.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, ErrorKind};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use rustix::fs::{AtFlags, FileType, Gid, Mode, OFlags, Timespec, Timestamps, Uid};

use super::at::{self, Stat};
use super::{fs_error, owner};
use crate::error::Error;

/// What [`copy`] did.
pub(crate) struct Copied {
    pub files: u64,
    pub bytes: u64,
    /// Every regular file was cloned with FICLONE, and there was at least one.
    pub cloned: bool,
    /// Entries that can't be copied, relative to the source: sockets and device
    /// nodes.
    pub skipped: Vec<PathBuf>,
}

struct Node {
    relative: PathBuf,
    stat: Stat,
}

/// Copy the tree at `source` into the empty directory open as `dest` (shown as
/// `shown`). Keeps modes, times, symlinks, FIFOs, and hard links within the tree,
/// and ownership when beavy works as root. Regular files are cloned with FICLONE
/// while the filesystem allows it, and copied otherwise. Skips the directory whose
/// device and inode are `exclude`, such as a store inside the tree.
///
/// Every path is resolved beneath an open directory without following symlinks,
/// and each copy takes its attributes from the file it actually opened, so
/// changing the tree while beavy copies it can't make beavy read or write anything
/// outside it.
pub(crate) fn copy(
    source: &Path,
    dest: &File,
    shown: &Path,
    exclude: Option<(u64, u64)>,
) -> Result<Copied, Error> {
    let failed = |err: io::Error, action: &str, path: &Path| {
        fs_error(err, format!("{action} {}", path.display()))
    };
    let keep_owner = owner::acting_as_root();
    let source_dir = at::open_dir(source).map_err(|err| failed(err, "read", source))?;
    let mut dirs = vec![Node {
        relative: PathBuf::new(),
        stat: at::fstat(&source_dir).map_err(|err| failed(err, "read", source))?,
    }];
    let mut files = Vec::new();
    let mut links = Vec::new();
    let mut seen: HashMap<(u64, u64), PathBuf> = HashMap::new();
    let mut skipped = Vec::new();

    let mut next_dir = 0;
    while next_dir < dirs.len() {
        let parent = dirs[next_dir].relative.clone();
        let from_path = source.join(&parent);
        let to_path = shown.join(&parent);
        let from = at::open_dir_beneath(&source_dir, &parent)
            .map_err(|err| failed(err, "read", &from_path))?;
        dirs[next_dir].stat = at::fstat(&from).map_err(|err| failed(err, "read", &from_path))?;
        next_dir += 1;
        let to =
            at::open_dir_beneath(dest, &parent).map_err(|err| failed(err, "read", &to_path))?;
        for name in at::names(&from).map_err(|err| failed(err, "read", &from_path))? {
            let relative = parent.join(&name);
            let stat = at::lstat(&from, &name)
                .map_err(|err| failed(err, "read", &source.join(&relative)))?;
            let target = shown.join(&relative);
            match stat.kind {
                FileType::Directory => {
                    if exclude == Some(stat.id()) {
                        continue;
                    }
                    rustix::fs::mkdirat(&to, &name, Mode::from_raw_mode(0o700))
                        .map_err(|err| failed(err.into(), "create", &target))?;
                    dirs.push(Node { relative, stat });
                }
                FileType::RegularFile => {
                    if stat.nlink > 1 {
                        match seen.entry(stat.id()) {
                            Entry::Occupied(first) => {
                                links.push((first.get().clone(), relative));
                                continue;
                            }
                            Entry::Vacant(slot) => {
                                slot.insert(relative.clone());
                            }
                        }
                    }
                    files.push(Node { relative, stat });
                }
                FileType::Symlink => {
                    let link = rustix::fs::readlinkat(&from, &name, Vec::new())
                        .map_err(|err| failed(err.into(), "read", &source.join(&relative)))?;
                    let link = OsStr::from_bytes(link.to_bytes());
                    rustix::fs::symlinkat(link, &to, &name)
                        .map_err(|err| failed(err.into(), "create", &target))?;
                    finish_symlink(&to, &name, &stat, keep_owner)
                        .map_err(|err| failed(err, "update", &target))?;
                }
                FileType::Fifo => {
                    rustix::fs::mknodat(&to, &name, FileType::Fifo, Mode::from_raw_mode(0o600), 0)
                        .map_err(|err| failed(err.into(), "create", &target))?;
                    let fifo = rustix::fs::openat(
                        &to,
                        &name,
                        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(|err| failed(err.into(), "open", &target))?;
                    finish(&fifo, &stat, keep_owner)
                        .map_err(|err| failed(err, "update", &target))?;
                }
                _ => skipped.push(relative),
            }
        }
    }

    let next = AtomicUsize::new(0);
    let try_clone = AtomicBool::new(true);
    let cloned = AtomicU64::new(0);
    let bytes = AtomicU64::new(0);
    let failure: Mutex<Option<Error>> = Mutex::new(None);
    let stop = AtomicBool::new(false);
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    let source_fd = source_dir.as_fd();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    let Some(file) = files.get(next.fetch_add(1, Ordering::Relaxed)) else {
                        break;
                    };
                    let copied = copy_file(source_fd, dest.as_fd(), file, keep_owner, &try_clone)
                        .map_err(|(err, action, to)| {
                            let path = if to { shown } else { source };
                            failed(err, action, &path.join(&file.relative))
                        });
                    match copied {
                        Ok((was_cloned, size)) => {
                            bytes.fetch_add(size, Ordering::Relaxed);
                            if was_cloned {
                                cloned.fetch_add(1, Ordering::Relaxed);
                            }
                        }
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
        let target = shown.join(relative);
        let link = || -> io::Result<()> {
            let from = at::open_dir_beneath(dest, parent_of(first))?;
            let to = at::open_dir_beneath(dest, parent_of(relative))?;
            rustix::fs::linkat(
                &from,
                name_of(first),
                &to,
                name_of(relative),
                AtFlags::empty(),
            )?;
            Ok(())
        };
        link().map_err(|err| failed(err, "link", &target))?;
    }
    for dir in dirs.iter().rev() {
        let target = shown.join(&dir.relative);
        at::open_dir_beneath(dest, &dir.relative)
            .and_then(|fd| finish(&fd, &dir.stat, keep_owner))
            .map_err(|err| failed(err, "update", &target))?;
    }

    let count = files.len() as u64;
    Ok(Copied {
        files: count + links.len() as u64,
        bytes: bytes.into_inner(),
        cloned: count > 0 && cloned.load(Ordering::Relaxed) == count,
        skipped,
    })
}

type Failure = (io::Error, &'static str, bool);

/// Copy one regular file. Returns whether it was cloned, and its size. A failure
/// names the action and whether it was on the destination.
fn copy_file(
    source: BorrowedFd<'_>,
    dest: BorrowedFd<'_>,
    file: &Node,
    keep_owner: bool,
    try_clone: &AtomicBool,
) -> Result<(bool, u64), Failure> {
    let read = |err: io::Error| (err, "read", false);
    let input = at::beneath(
        source,
        &file.relative,
        OFlags::RDONLY | OFlags::NOCTTY | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(read)?;
    let stat = at::fstat(&input).map_err(read)?;
    if stat.kind != FileType::RegularFile {
        return Err(read(io::Error::other("it changed while beavy copied it")));
    }
    let output = at::beneath(
        dest,
        &file.relative,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
        Mode::from_raw_mode(0o600),
    )
    .map_err(|err| (err, "create", true))?;
    let (mut input, mut output) = (File::from(input), File::from(output));
    let mut cloned = false;
    if try_clone.load(Ordering::Relaxed) {
        if rustix::fs::ioctl_ficlone(&output, &input).is_ok() {
            cloned = true;
        } else {
            try_clone.store(false, Ordering::Relaxed);
        }
    }
    if !cloned {
        io::copy(&mut input, &mut output).map_err(|err| (err, "copy", false))?;
    }
    finish(&output, &stat, keep_owner).map_err(|err| (err, "update", true))?;
    Ok((cloned, stat.size))
}

/// Apply ownership (as root), mode, and times to an open file or directory.
fn finish(fd: impl AsFd, stat: &Stat, keep_owner: bool) -> io::Result<()> {
    if keep_owner {
        rustix::fs::fchown(
            &fd,
            Some(Uid::from_raw(stat.uid)),
            Some(Gid::from_raw(stat.gid)),
        )?;
    }
    rustix::fs::fchmod(&fd, Mode::from_raw_mode(stat.mode))?;
    rustix::fs::futimens(&fd, &times(stat))?;
    Ok(())
}

fn finish_symlink(dir: &OwnedFd, name: &OsStr, stat: &Stat, keep_owner: bool) -> io::Result<()> {
    if keep_owner {
        rustix::fs::chownat(
            dir,
            name,
            Some(Uid::from_raw(stat.uid)),
            Some(Gid::from_raw(stat.gid)),
            AtFlags::SYMLINK_NOFOLLOW,
        )?;
    }
    rustix::fs::utimensat(dir, name, &times(stat), AtFlags::SYMLINK_NOFOLLOW)?;
    Ok(())
}

fn times(stat: &Stat) -> Timestamps {
    let timespec = |(secs, nanos): (i64, i64)| Timespec {
        tv_sec: secs,
        tv_nsec: nanos as _,
    };
    Timestamps {
        last_access: timespec(stat.atime),
        last_modification: timespec(stat.mtime),
    }
}

fn parent_of(path: &Path) -> &Path {
    path.parent().unwrap_or(Path::new(""))
}

fn name_of(path: &Path) -> &OsStr {
    path.file_name().unwrap_or_default()
}

/// Remove a directory tree, including directories whose mode denies the owner
/// access (overlayfs leaves its work directory at mode 000). A missing path is
/// already removed. Never follows a symlink.
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
            let parent = at::open_dir(parent_of(path))?;
            open_up(&parent, name_of(path))?;
            fs::remove_dir_all(path)
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// Give the owner full access to the directory `name` in `parent` and everything
/// under it.
fn open_up(parent: &OwnedFd, name: &OsStr) -> io::Result<()> {
    let stat = at::lstat(parent, name)?;
    if stat.kind != FileType::Directory {
        return Ok(());
    }
    if stat.mode & 0o700 != 0o700 {
        rustix::fs::chmodat(
            parent,
            name,
            Mode::from_raw_mode(stat.mode | 0o700),
            AtFlags::empty(),
        )?;
    }
    let dir = rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let entries: Vec<OsString> = at::names(&dir)?;
    for entry in entries {
        open_up(&dir, &entry)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;
    use std::fs::Permissions;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink};

    use super::*;

    fn layer(dir: &Path) -> (PathBuf, File) {
        let d = dir.join("layer");
        fs::create_dir(&d).unwrap();
        let fd = File::from(at::open_dir(&d).unwrap());
        (d, fd)
    }

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
        let old = [libc::timespec {
            tv_sec: 1_000_000,
            tv_nsec: 5,
        }; 2];
        let file = File::open(s.join("a/b/file")).unwrap();
        assert_eq!(unsafe { libc::futimens(file.as_raw_fd(), old.as_ptr()) }, 0);
        fs::create_dir_all(s.join("store/layers")).unwrap();
        let exclude = super::super::identity(&s.join("store"));

        let dst = tempfile::tempdir().unwrap();
        let (d, fd) = layer(dst.path());
        let copied = copy(s, &fd, &d, exclude).unwrap();

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
    fn copy_writes_only_through_the_open_destination() {
        let src = tempfile::tempdir().unwrap();
        fs::create_dir(src.path().join("sub")).unwrap();
        fs::write(src.path().join("sub/file"), b"data").unwrap();
        let dst = tempfile::tempdir().unwrap();
        let (d, fd) = layer(dst.path());
        let moved = dst.path().join("moved");
        let elsewhere = dst.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        fs::rename(&d, &moved).unwrap();
        symlink(&elsewhere, &d).unwrap();
        copy(src.path(), &fd, &d, None).unwrap();
        assert_eq!(fs::read(moved.join("sub/file")).unwrap(), b"data");
        assert!(fs::read_dir(&elsewhere).unwrap().next().is_none());
    }

    #[test]
    fn copy_never_reads_through_a_file_swapped_for_a_symlink() {
        let src = tempfile::tempdir().unwrap();
        let secret = tempfile::tempdir().unwrap();
        fs::write(secret.path().join("shadow"), b"root::").unwrap();
        fs::write(src.path().join("secret"), b"mine").unwrap();
        let source = File::from(at::open_dir(src.path()).unwrap());
        let walked = Node {
            relative: PathBuf::from("secret"),
            stat: at::lstat(&source, OsStr::new("secret")).unwrap(),
        };
        fs::remove_file(src.path().join("secret")).unwrap();
        symlink(secret.path().join("shadow"), src.path().join("secret")).unwrap();
        let dst = tempfile::tempdir().unwrap();
        let dest = File::from(at::open_dir(dst.path()).unwrap());
        let copied = copy_file(
            source.as_fd(),
            dest.as_fd(),
            &walked,
            false,
            &AtomicBool::new(true),
        );
        assert!(copied.is_err());
        assert!(!dst.path().join("secret").exists());
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
