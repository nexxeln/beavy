//! File system calls relative to an open directory. beavy opens a directory once
//! and resolves every path beneath it without following symlinks, so nothing that
//! renames or swaps entries while beavy works can send it anywhere else.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

use rustix::fs::{AtFlags, FileType, Mode, OFlags, ResolveFlags};
use rustix::io::Errno;

/// The parts of a stat that beavy uses.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Stat {
    pub kind: FileType,
    pub dev: u64,
    pub ino: u64,
    pub nlink: u64,
    /// Permission bits, setuid, setgid, and sticky included.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub atime: (i64, i64),
    pub mtime: (i64, i64),
}

impl Stat {
    #[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
    fn from_raw(stat: rustix::fs::Stat) -> Stat {
        Stat {
            kind: FileType::from_raw_mode(stat.st_mode as _),
            dev: stat.st_dev as u64,
            ino: stat.st_ino as u64,
            nlink: stat.st_nlink as u64,
            mode: (stat.st_mode as u32) & 0o7777,
            uid: stat.st_uid,
            gid: stat.st_gid,
            size: stat.st_size as u64,
            atime: (stat.st_atime as i64, stat.st_atime_nsec as i64),
            mtime: (stat.st_mtime as i64, stat.st_mtime_nsec as i64),
        }
    }

    pub(crate) fn id(&self) -> (u64, u64) {
        (self.dev, self.ino)
    }
}

pub(crate) fn fstat(fd: impl AsFd) -> io::Result<Stat> {
    Ok(Stat::from_raw(rustix::fs::fstat(fd)?))
}

/// Stat `name` in `dir` without following it.
pub(crate) fn lstat(dir: impl AsFd, name: &OsStr) -> io::Result<Stat> {
    Ok(Stat::from_raw(rustix::fs::statat(
        dir,
        name,
        AtFlags::SYMLINK_NOFOLLOW,
    )?))
}

/// Open the directory at `path`, which must not be a symlink itself.
pub(crate) fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    Ok(rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

/// Open the directory at the absolute `path` without following a symlink anywhere
/// on the way.
pub(crate) fn open_dir_no_symlinks(path: &Path) -> io::Result<OwnedFd> {
    let relative = path
        .strip_prefix("/")
        .map_err(|_| io::Error::from(Errno::INVAL))?;
    open_dir_beneath(open_dir(Path::new("/"))?, relative)
}

/// Open the directory `relative` beneath `dir`.
pub(crate) fn open_dir_beneath(dir: impl AsFd, relative: &Path) -> io::Result<OwnedFd> {
    beneath(
        dir,
        relative,
        OFlags::RDONLY | OFlags::DIRECTORY,
        Mode::empty(),
    )
}

/// Open `relative` beneath `dir` with `flags`. No component may be a symlink,
/// `..`, or absolute, so the result is always inside `dir`. An empty path opens
/// `dir` itself.
pub(crate) fn beneath(
    dir: impl AsFd,
    relative: &Path,
    flags: OFlags,
    mode: Mode,
) -> io::Result<OwnedFd> {
    let dir = dir.as_fd();
    let flags = flags | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let path: &OsStr = if relative.as_os_str().is_empty() {
        OsStr::new(".")
    } else {
        relative.as_os_str()
    };
    let resolve = ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS;
    for _ in 0..8 {
        match rustix::fs::openat2(dir, path, flags, mode, resolve) {
            Err(Errno::AGAIN) | Err(Errno::INTR) => continue,
            Err(Errno::NOSYS) => break,
            result => return Ok(result?),
        }
    }
    let components: Vec<Component> = relative.components().collect();
    let mut current = None;
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err(Errno::XDEV.into());
        };
        let parent = current.as_ref().map_or(dir, |fd: &OwnedFd| fd.as_fd());
        let opened = if index + 1 == components.len() {
            rustix::fs::openat(parent, *name, flags, mode)?
        } else {
            rustix::fs::openat(
                parent,
                *name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?
        };
        current = Some(opened);
    }
    match current {
        Some(fd) => Ok(fd),
        None => Ok(rustix::fs::openat(dir, ".", flags, mode)?),
    }
}

/// The names in a directory, sorted, without `.` and `..`.
pub(crate) fn names(dir: impl AsFd) -> io::Result<Vec<OsString>> {
    let mut names = Vec::new();
    for entry in rustix::fs::Dir::read_from(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(OsStr::from_bytes(name).to_owned());
        }
    }
    names.sort();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn beneath_never_leaves_the_directory_or_follows_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/file"), b"x").unwrap();
        fs::write(root.join("outside"), b"x").unwrap();
        symlink(root.join("a"), root.join("a/link")).unwrap();
        symlink("../outside", root.join("a/up")).unwrap();
        let a = open_dir(&root.join("a")).unwrap();
        let file = beneath(&a, Path::new("b/file"), OFlags::RDONLY, Mode::empty()).unwrap();
        assert!(fstat(&file).unwrap().kind == FileType::RegularFile);
        for bad in ["link/b/file", "up", "../outside", "/etc/passwd", "link"] {
            assert!(
                beneath(&a, Path::new(bad), OFlags::RDONLY, Mode::empty()).is_err(),
                "{bad}"
            );
        }
        assert_eq!(names(&a).unwrap(), ["b", "link", "up"]);
        assert!(lstat(&a, OsStr::new("link")).unwrap().kind == FileType::Symlink);
        assert!(open_dir(&root.join("a/link")).is_err());
        let itself = open_dir_beneath(&a, Path::new("")).unwrap();
        assert_eq!(fstat(&itself).unwrap().id(), fstat(&a).unwrap().id());
    }
}
