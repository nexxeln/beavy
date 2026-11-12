//! Who beavy acts as. beavy reads and writes a store as the store's owner, so a
//! store never gives anyone more access than its owner has. As root in another
//! user's store, beavy takes on that user's file system identity on the thread
//! doing the work, and keeps root only for what needs it. Through sudo, beavy
//! works for the user who ran sudo, and only in stores that user owns.

use std::ffi::{CStr, CString, OsString};
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use rustix::process::Gid;

/// A user, and the groups their access to files is checked against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Identity {
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
}

impl Identity {
    /// A user with primary group `gid` and the groups the group database gives
    /// them.
    pub(crate) fn of(uid: u32, gid: u32) -> Identity {
        let groups = account(uid)
            .and_then(|account| group_list(&account.name, gid))
            .unwrap_or_else(|| vec![gid]);
        Identity { uid, gid, groups }
    }
}

/// Whether this process runs as root.
pub(crate) fn privileged() -> bool {
    rustix::process::geteuid().is_root()
}

/// Whether file access on this thread is root's: beavy is root and isn't acting as
/// another user.
pub(crate) fn acting_as_root() -> bool {
    // SAFETY: setfsuid with an invalid ID changes nothing and returns the current one.
    privileged() && unsafe { libc::setfsuid(u32::MAX) } == 0
}

/// The user and group that ran sudo, when beavy is root through sudo.
pub(crate) fn sudo_caller() -> Option<(u32, u32)> {
    if !privileged() {
        return None;
    }
    let number = |name: &str| std::env::var(name).ok()?.parse::<u32>().ok();
    let uid = number("SUDO_UID").filter(|uid| *uid != 0)?;
    let gid = number("SUDO_GID").or_else(|| primary_gid(uid))?;
    Some((uid, gid))
}

/// The user beavy works for: the one who ran sudo when beavy is root through sudo,
/// and the process's own user otherwise.
pub(crate) fn caller() -> Identity {
    if let Some((uid, gid)) = sudo_caller() {
        return Identity::of(uid, gid);
    }
    let uid = rustix::process::geteuid().as_raw();
    let gid = rustix::process::getegid().as_raw();
    let groups = rustix::process::getgroups()
        .map(|groups| groups.iter().map(|gid| gid.as_raw()).collect())
        .unwrap_or_else(|_| vec![gid]);
    Identity { uid, gid, groups }
}

/// The user a command beavy runs should run as, when that isn't the process's own:
/// the one who ran sudo.
pub(crate) fn command_user() -> Option<Identity> {
    sudo_caller().map(|(uid, gid)| Identity::of(uid, gid))
}

/// Become `identity` for good on this thread: groups, then group, then user.
/// Async-signal-safe, for the step between fork and exec. Fails with the errno.
pub(crate) fn become_user(identity: &Identity, groups: &[Gid]) -> Result<(), i32> {
    let gid = Gid::from_raw(identity.gid);
    let uid = rustix::process::Uid::from_raw(identity.uid);
    rustix::thread::set_thread_groups(groups).map_err(|err| err.raw_os_error())?;
    rustix::thread::set_thread_res_gid(gid, gid, gid).map_err(|err| err.raw_os_error())?;
    rustix::thread::set_thread_res_uid(uid, uid, uid).map_err(|err| err.raw_os_error())
}

/// `identity`'s groups, ready for [`become_user`].
pub(crate) fn groups_of(identity: &Identity) -> Vec<Gid> {
    identity
        .groups
        .iter()
        .map(|gid| Gid::from_raw(*gid))
        .collect()
}

/// A user's primary group, from the password database.
pub(crate) fn primary_gid(uid: u32) -> Option<u32> {
    account(uid).map(|account| account.gid)
}

/// The home directory that holds default stores. sudo usually sets `HOME` to
/// root's, so through sudo beavy uses the home of the user who ran it instead.
pub(crate) fn store_home(home: Option<OsString>) -> Option<OsString> {
    let Some((uid, _)) = sudo_caller() else {
        return home;
    };
    let root_home = account(0).map(|account| account.home.into_os_string());
    match home {
        Some(home) if Some(&home) == root_home.as_ref() => {
            account(uid).map(|account| account.home.into_os_string())
        }
        home => home,
    }
}

/// A user's name, or their number when the password database doesn't know them.
pub(crate) fn name_of(uid: u32) -> String {
    account(uid)
        .map(|account| account.name.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("user {uid}"))
}

/// This thread's file system identity while beavy acts as another user. Dropping it
/// returns the thread to root.
pub(crate) struct Acting {
    restore: Option<Vec<Gid>>,
}

impl Drop for Acting {
    fn drop(&mut self) {
        if let Some(groups) = &self.restore {
            // SAFETY: setfsuid and setfsgid take IDs only, and root may return to 0.
            unsafe {
                libc::setfsuid(0);
                libc::setfsgid(0);
            }
            let _ = rustix::thread::set_thread_groups(groups);
        }
    }
}

/// Act as `identity` for file access on this thread, and on every thread it
/// starts, until the guard drops. Only root acting as itself changes anything: any
/// other user can only be itself, and a thread already acting stays as it is.
pub(crate) fn act_as(identity: &Identity) -> io::Result<Acting> {
    if identity.uid == 0 || !acting_as_root() {
        return Ok(Acting { restore: None });
    }
    let groups = rustix::process::getgroups()?;
    let wanted: Vec<Gid> = identity
        .groups
        .iter()
        .map(|gid| Gid::from_raw(*gid))
        .collect();
    rustix::thread::set_thread_groups(&wanted)?;
    let acting = Acting {
        restore: Some(groups),
    };
    // SAFETY: setfsgid and setfsuid take IDs only. Each returns the previous ID and
    // never an error, so an invalid ID reads the current one back to check.
    let (gid, uid) = unsafe {
        libc::setfsgid(identity.gid);
        libc::setfsuid(identity.uid);
        (libc::setfsgid(u32::MAX), libc::setfsuid(u32::MAX))
    };
    if (uid as u32, gid as u32) != (identity.uid, identity.gid) {
        return Err(io::Error::other(format!(
            "couldn't act as {}",
            name_of(identity.uid)
        )));
    }
    Ok(acting)
}

struct Account {
    name: CString,
    home: PathBuf,
    gid: u32,
}

fn account(uid: u32) -> Option<Account> {
    let mut buffer = vec![0u8; 4096];
    loop {
        // SAFETY: passwd is plain data that getpwuid_r fills in.
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found = std::ptr::null_mut();
        // SAFETY: every pointer is valid for the call, and the buffer's length is given.
        let rc = unsafe {
            libc::getpwuid_r(
                uid,
                &mut entry,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut found,
            )
        };
        if rc == libc::ERANGE && buffer.len() < 1 << 20 {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        if rc != 0 || found.is_null() {
            return None;
        }
        // SAFETY: getpwuid_r succeeded, so both strings point into the buffer.
        let (name, home) = unsafe { (CStr::from_ptr(entry.pw_name), CStr::from_ptr(entry.pw_dir)) };
        return Some(Account {
            name: name.to_owned(),
            home: PathBuf::from(OsString::from_vec(home.to_bytes().to_vec())),
            gid: entry.pw_gid,
        });
    }
}

fn group_list(name: &CStr, gid: u32) -> Option<Vec<u32>> {
    let mut capacity: libc::c_int = 64;
    loop {
        let mut groups = vec![0 as libc::gid_t; capacity as usize];
        let mut count = capacity;
        // SAFETY: name is NUL-terminated and groups has room for count entries.
        let rc = unsafe { libc::getgrouplist(name.as_ptr(), gid, groups.as_mut_ptr(), &mut count) };
        if rc >= 0 {
            groups.truncate(count as usize);
            return Some(groups);
        }
        if count <= capacity {
            return None;
        }
        capacity = count;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_user_acts_only_as_itself() {
        if privileged() {
            return;
        }
        let me = caller();
        assert!(
            act_as(&Identity::of(me.uid + 1, me.gid))
                .unwrap()
                .restore
                .is_none()
        );
        assert!(!acting_as_root());
        assert_eq!(sudo_caller(), None);
        assert_eq!(store_home(Some("/h".into())), Some("/h".into()));
        assert!(!name_of(me.uid).is_empty());
    }

    #[test]
    fn root_acts_as_a_user_and_returns() {
        if !privileged() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let nobody = Identity::of(65534, 65534);
        {
            let _acting = act_as(&nobody).unwrap();
            assert!(!acting_as_root());
            let _again = act_as(&nobody).unwrap();
            let err = std::fs::write(dir.path().join("x"), b"x").unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        }
        assert!(acting_as_root());
        std::fs::write(dir.path().join("x"), b"x").unwrap();
    }
}
