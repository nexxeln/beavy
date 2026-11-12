//! The store: an ordinary directory that holds one project's immutable layers,
//! snapshot records, and worktrees. beavy changes nothing outside it, so removing
//! beavy from a project means deleting its store.
//!
//! ```text
//! store.json            format marker
//! lock                  held briefly by every change to the store's records
//! layers/<layer>/       immutable directory trees
//! snapshots/<id>.json   a snapshot: a chain of layers, bottom first
//! worktrees/<id>/       a worktree: a snapshot and a private upper layer
//! trash/<id>/           deleted snapshots and worktrees, restorable until the
//!                       trash is emptied
//! tmp/<entry>/          work in progress, locked by the process doing it
//! ```
//!
//! The disk is the source of truth. A record appears with one atomic rename once
//! everything it names is in place, so a crash leaves either the old state or the
//! new one, plus leftovers in tmp/ and unreferenced layers. The next command that
//! changes the store removes those leftovers before it does anything else.
//!
//! Every file system call in a store runs as the store's owner (see [`owner`]),
//! and beavy refuses a store that anyone else can write. Records are untrusted
//! input: an ID or layer name is checked before it becomes part of a path.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::envelope::{Warning, WarningReason};
use crate::error::{Error, Next, Reason};

mod at;
mod git;
mod overlay;
mod owner;
mod snapshot;
mod squash;
mod trash;
mod tree;
mod worktree;

pub use overlay::{Backend, Privilege};
pub use snapshot::{Created, Deleted, Method, Snapshot};
pub use trash::{Emptied, Kind, Restored, TrashEntry};
pub use worktree::{
    CreatedWorktree, DeletedWorktree, Mounted, Prepared, Sealed, WORKTREE_VARIABLE, Worktree,
};

/// The version of the store layout this beavy reads and writes.
pub const FORMAT: u32 = 1;

/// The environment variable that selects a store when `--store` isn't given.
pub const STORE_VARIABLE: &str = "BEAVY_STORE";

const STORE_FILE: &str = "store.json";
const LOCK_FILE: &str = "lock";
const LAYERS: &str = "layers";
const SNAPSHOTS: &str = "snapshots";
const WORKTREES: &str = "worktrees";
const TRASH: &str = "trash";
const TMP: &str = "tmp";

/// How a store location was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Selection {
    /// The `--store` option.
    Option,
    /// The `BEAVY_STORE` environment variable.
    Environment,
    /// The default store for the project.
    Default,
}

/// Where a project's store lives, and which project the working directory belongs to.
#[derive(Debug, Clone)]
pub struct Location {
    /// The store directory. It may not exist yet.
    pub path: PathBuf,
    pub selection: Selection,
    /// The project directory: the nearest ancestor of the working directory (itself
    /// included) that already has a default store, else the root of the Git working
    /// tree that contains it, else the working directory.
    pub project: PathBuf,
}

/// Resolve the store for `cwd`. `option` is `--store`; `env` looks up an
/// environment variable. An empty `BEAVY_STORE` counts as unset.
pub fn locate(
    cwd: &Path,
    option: Option<&Path>,
    env: impl Fn(&str) -> Option<OsString>,
) -> Result<Location, Error> {
    let shown = cwd.display();
    let cwd = fs::canonicalize(cwd).map_err(|err| match err.kind() {
        ErrorKind::NotFound => Error::new(
            Reason::NotFound,
            format!("Couldn't find directory {shown}."),
        )
        .with_cause(err),
        _ => fs_error(err, format!("resolve directory {shown}")),
    })?;
    if !cwd.is_dir() {
        return Err(Error::new(
            Reason::InvalidArguments,
            format!("Can't use {shown}. It isn't a directory."),
        ));
    }
    let data_home = env("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            owner::store_home(env("HOME"))
                .filter(|home| !home.is_empty())
                .map(|home| PathBuf::from(home).join(".local/share"))
        });
    let stores = data_home.map(|home| home.join("beavy/stores"));

    let project = stores
        .as_ref()
        .and_then(|stores| {
            cwd.ancestors()
                .find(|dir| stores.join(default_name(dir)).is_dir())
        })
        .or_else(|| cwd.ancestors().find(|dir| dir.join(".git").exists()))
        .unwrap_or(&cwd)
        .to_path_buf();

    let explicit = |path: PathBuf, selection| -> Result<Location, Error> {
        let path = cwd.join(path);
        if path.exists() && !path.is_dir() {
            return Err(Error::new(
                Reason::InvalidArguments,
                format!("Can't use store {}. It isn't a directory.", path.display()),
            ));
        }
        let path = fs::canonicalize(&path).unwrap_or(path);
        Ok(Location {
            path,
            selection,
            project: project.clone(),
        })
    };
    if let Some(option) = option {
        return explicit(option.to_path_buf(), Selection::Option);
    }
    if let Some(value) = env(STORE_VARIABLE).filter(|value| !value.is_empty()) {
        return explicit(PathBuf::from(value), Selection::Environment);
    }
    let Some(stores) = stores else {
        return Err(Error::new(
            Reason::MissingArgument,
            format!(
                "Can't choose a store for {}. Neither HOME nor XDG_DATA_HOME is set.",
                project.display()
            ),
        )
        .with_next(
            "Pass a store directory",
            Next::new(["beavy", "snapshots", "list", "--store", "<DIR>"]),
        ));
    };
    Ok(Location {
        path: stores.join(default_name(&project)),
        selection: Selection::Default,
        project,
    })
}

/// The default store's directory name for a project: its base name and a hash of
/// its path, such as `acme-3f9a1c2e`.
fn default_name(project: &Path) -> String {
    let base: String = project
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(32)
        .collect();
    let base = base.trim_matches('-');
    let base = if base.is_empty() { "root" } else { base };
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in project.as_os_str().as_bytes() {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
    }
    format!("{base}-{:08x}", hash >> 32)
}

/// A store directory. Reads never create it; the first change does.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
    context: Vec<String>,
}

/// Holds the store lock. Dropping it releases the lock.
pub(crate) struct Guard {
    _file: File,
}

/// A directory under tmp/ that this process owns. Dropping it removes the directory
/// unless it was moved into place with [`Scratch::persist`].
pub(crate) struct Scratch {
    path: PathBuf,
    dir: File,
    persisted: bool,
}

impl Scratch {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The directory, open and locked. Work inside it goes through this
    /// descriptor, so renaming tmp/<entry> can't redirect it.
    pub(crate) fn dir(&self) -> &File {
        &self.dir
    }

    /// Rename the directory to `target`, which must be on the same filesystem, and
    /// check that what arrived there is this directory.
    pub(crate) fn persist(mut self, target: &Path) -> io::Result<()> {
        fs::rename(&self.path, target)?;
        self.persisted = true;
        let moved = fs::symlink_metadata(target)?;
        let dir = self.dir.metadata()?;
        if (moved.dev(), moved.ino()) != (dir.dev(), dir.ino()) {
            return Err(io::Error::other(format!(
                "{} was replaced while beavy worked in it",
                self.path.display()
            )));
        }
        Ok(())
    }

    /// Remove the directory now, and say so in a warning if that fails.
    pub(crate) fn discard(mut self) -> Option<Warning> {
        self.persisted = true;
        tree::remove(&self.path)
            .err()
            .map(|err| cleanup_warning(&self.path, &err))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !self.persisted {
            let _ = tree::remove(&self.path);
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct StoreRecord {
    format: u32,
}

impl Store {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Store {
            root: path.into(),
            context: Vec::new(),
        }
    }

    /// Arguments to repeat in every command the store suggests, such as the
    /// `--store <DIR>` the caller passed.
    pub fn with_context(mut self, args: Vec<String>) -> Self {
        self.context = args;
        self
    }

    /// A suggested command, with the caller's context before any `--`.
    pub fn next(&self, argv: &[&str]) -> Next {
        let split = argv
            .iter()
            .position(|arg| *arg == "--")
            .unwrap_or(argv.len());
        let argv = argv[..split]
            .iter()
            .map(|arg| arg.to_string())
            .chain(self.context.iter().cloned())
            .chain(argv[split..].iter().map(|arg| arg.to_string()));
        Next::new(argv)
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    pub(crate) fn join(&self, relative: impl AsRef<Path>) -> PathBuf {
        self.root.join(relative)
    }

    /// The store's path with symlinks resolved, as the kernel reports it in mounts.
    pub(crate) fn absolute(&self) -> PathBuf {
        fs::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone())
    }

    /// Act as the store's owner on this thread until the guard drops. Every public
    /// operation starts here. A store that doesn't exist yet will belong to the
    /// caller. Refuses a store that users other than its owner can write, a store
    /// that belongs to someone else unless beavy is root without sudo, and, when
    /// beavy works as root, a store that another user could move or replace.
    pub(crate) fn act(&self) -> Result<owner::Acting, Error> {
        let shown = self.root.display();
        let caller = owner::caller();
        let identity = match fs::symlink_metadata(&self.root) {
            Err(err) if err.kind() == ErrorKind::NotFound => caller,
            Err(err) => return Err(fs_error(err, format!("read store {shown}"))),
            Ok(metadata) => {
                if !metadata.is_dir() {
                    return Err(Error::new(
                        Reason::InvalidArguments,
                        format!("Can't use store {shown}. It isn't a directory."),
                    ));
                }
                if metadata.mode() & 0o022 != 0 {
                    return Err(Error::new(
                        Reason::PermissionDenied,
                        format!(
                            "Can't use store {shown}. Users other than its owner can write to it."
                        ),
                    )
                    .with_next(
                        "Make it writable only by its owner",
                        Next::new(["chmod", "go-w", &shown.to_string()]),
                    ));
                }
                let uid = metadata.uid();
                if uid == caller.uid {
                    caller
                } else if owner::privileged() && owner::sudo_caller().is_none() {
                    owner::Identity::of(uid, owner::primary_gid(uid).unwrap_or(metadata.gid()))
                } else {
                    return Err(Error::new(
                        Reason::PermissionDenied,
                        format!(
                            "Can't use store {shown}. It belongs to {}, and beavy only uses a store as its owner.",
                            owner::name_of(uid)
                        ),
                    ));
                }
            }
        };
        if identity.uid == 0 && owner::privileged() {
            self.check_root_path()?;
        }
        owner::act_as(&identity).map_err(|err| fs_error(err, format!("use store {shown}")))
    }

    /// Working as root, refuse a store when a directory on the way to it belongs to
    /// another user or is writable by others without the sticky bit, because that
    /// user could replace the store while beavy works in it.
    fn check_root_path(&self) -> Result<(), Error> {
        for dir in self.root.ancestors() {
            let metadata = match fs::symlink_metadata(dir) {
                Ok(metadata) => metadata,
                Err(err) if err.kind() == ErrorKind::NotFound => continue,
                Err(err) => return Err(fs_error(err, format!("read {}", dir.display()))),
            };
            let shared = metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0;
            if metadata.uid() != 0 || shared {
                let who = if metadata.uid() != 0 {
                    format!("belongs to {}", owner::name_of(metadata.uid()))
                } else {
                    "is writable by other users".to_owned()
                };
                return Err(Error::new(
                    Reason::PermissionDenied,
                    format!(
                        "Can't use store {} as root. {} {who}, who could replace the store while beavy works in it.",
                        self.root.display(),
                        dir.display()
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Whether the store exists, after checking that this beavy can read it.
    pub(crate) fn exists(&self) -> Result<bool, Error> {
        let marker = self.join(STORE_FILE);
        let Some(record) = read_record::<StoreRecord>(&marker)? else {
            return Ok(false);
        };
        if record.format != FORMAT {
            return Err(Error::new(
                Reason::InvalidStore,
                format!(
                    "Can't use store {}. It has layout version {}, and this beavy reads version {FORMAT}.",
                    self.root.display(),
                    record.format
                ),
            ));
        }
        Ok(true)
    }

    /// Create the store if it doesn't exist, take its lock, and repair whatever an
    /// interrupted command left behind. Every change starts here.
    pub(crate) fn open_for_change(&self) -> Result<(Guard, Vec<Warning>), Error> {
        if !self.exists()? {
            self.create()?;
        }
        let guard = self.lock()?;
        let warnings = self.repair(&guard)?;
        Ok((guard, warnings))
    }

    /// Make the store in a new or empty directory. A directory with anything in it
    /// is refused: beavy removes what it doesn't recognize from tmp/ and layers/, so
    /// it never adopts a directory it didn't create.
    fn create(&self) -> Result<(), Error> {
        let shown = self.root.display();
        match fs::read_dir(&self.root) {
            Ok(mut entries) => {
                if entries.next().is_some() {
                    return Err(Error::new(
                        Reason::InvalidArguments,
                        format!(
                            "Can't create a store in {shown}. The directory isn't empty, and it isn't a beavy store."
                        ),
                    ));
                }
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {
                if let Some(parent) = self.root.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|err| fs_error(err, format!("create store {shown}")))?;
                }
                match fs::DirBuilder::new().mode(0o700).create(&self.root) {
                    Err(err) if err.kind() != ErrorKind::AlreadyExists => {
                        return Err(fs_error(err, format!("create store {shown}")));
                    }
                    _ => {}
                }
            }
            Err(err) => return Err(fs_error(err, format!("read {shown}"))),
        }
        for dir in [LAYERS, SNAPSHOTS, WORKTREES, TRASH, TMP] {
            let path = self.join(dir);
            match fs::create_dir(&path) {
                Err(err) if err.kind() != ErrorKind::AlreadyExists => {
                    return Err(fs_error(err, format!("create {}", path.display())));
                }
                _ => {}
            }
        }
        let record = StoreRecord { format: FORMAT };
        write_record(&self.root, STORE_FILE, &record)
    }

    pub(crate) fn lock(&self) -> Result<Guard, Error> {
        let path = self.join(LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o644)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|err| fs_error(err, format!("open {}", path.display())))?;
        lock_file(&file, libc::LOCK_EX)
            .map_err(|err| fs_error(err, format!("lock {}", path.display())))?;
        Ok(Guard { _file: file })
    }

    /// Make a locked directory under tmp/. Requires the store lock, so repair never
    /// sees it before it's locked.
    pub(crate) fn scratch(&self, _guard: &Guard) -> Result<Scratch, Error> {
        let path = self.join(TMP).join(random_id());
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|err| fs_error(err, format!("create {}", path.display())))?;
        let dir = at::open_dir(&path)
            .map(File::from)
            .and_then(|dir| lock_file(&dir, libc::LOCK_EX).map(|()| dir))
            .map_err(|err| fs_error(err, format!("lock {}", path.display())))?;
        Ok(Scratch {
            path,
            dir,
            persisted: false,
        })
    }

    /// Remove leftovers of interrupted commands: tmp/ entries no process holds,
    /// half-written records and trash entries, and layers no snapshot uses. Finish
    /// interrupted seals, and detach mounts of worktrees that no longer exist. Only
    /// names beavy makes are touched.
    fn repair(&self, guard: &Guard) -> Result<Vec<Warning>, Error> {
        let mut warnings = Vec::new();
        for entry in list_dir(&self.join(TMP))? {
            if !is_random_id(&entry) {
                continue;
            }
            let path = self.join(TMP).join(&entry);
            let Ok(file) = at::open_dir(&path).map(File::from) else {
                continue;
            };
            if try_lock_file(&file, libc::LOCK_EX).unwrap_or(false)
                && let Err(err) = tree::remove(&path)
            {
                warnings.push(cleanup_warning(&path, &err));
            }
        }
        for entry in list_dir(&self.join(SNAPSHOTS))? {
            if is_temporary_record(&entry) {
                let _ = fs::remove_file(self.join(SNAPSHOTS).join(entry));
            }
        }
        warnings.extend(self.repair_worktrees(guard)?);
        warnings.extend(self.repair_trash()?);
        if let Some(trash) = self.trash_unused_layers(guard)? {
            warnings.extend(trash.discard());
        }
        Ok(warnings)
    }

    /// Move every layer that no snapshot uses, live or trashed, into a scratch
    /// directory for the caller to discard once it releases the lock.
    pub(crate) fn trash_unused_layers(&self, guard: &Guard) -> Result<Option<Scratch>, Error> {
        let mut used = HashSet::new();
        for record in self
            .snapshot_records()?
            .into_iter()
            .chain(self.trashed_snapshots()?)
        {
            used.extend(record.layers);
        }
        let unused: Vec<OsString> = list_dir(&self.join(LAYERS))?
            .into_iter()
            .filter(|layer| is_random_id(layer))
            .filter(|layer| !layer.to_str().is_some_and(|layer| used.contains(layer)))
            .collect();
        if unused.is_empty() {
            return Ok(None);
        }
        let trash = self.scratch(guard)?;
        for layer in unused {
            let from = self.join(LAYERS).join(&layer);
            fs::rename(&from, trash.path().join(&layer))
                .map_err(|err| fs_error(err, format!("move {}", from.display())))?;
        }
        Ok(Some(trash))
    }
}

/// The names in a directory, or none when it doesn't exist.
pub(crate) fn list_dir(path: &Path) -> Result<Vec<OsString>, Error> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(fs_error(err, format!("read {}", path.display()))),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| fs_error(err, format!("read {}", path.display())))?;
        names.push(entry.file_name());
    }
    names.sort();
    Ok(names)
}

/// Read a JSON record, or `None` when the file doesn't exist. A symlink is never
/// followed.
pub(crate) fn read_record<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, Error> {
    let read = || -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    let bytes = match read() {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(fs_error(err, format!("read {}", path.display()))),
    };
    serde_json::from_slice(&bytes).map(Some).map_err(|err| {
        Error::new(
            Reason::InvalidStore,
            format!(
                "Can't read {}. It isn't a valid beavy record.",
                path.display()
            ),
        )
        .with_cause(err)
    })
}

/// Write a JSON record atomically: a temporary file in the same directory, synced,
/// then renamed over the target, then the directory synced.
pub(crate) fn write_record<T: Serialize>(dir: &Path, name: &str, record: &T) -> Result<(), Error> {
    let target = dir.join(name);
    let write = || -> io::Result<()> {
        let mut bytes = serde_json::to_vec_pretty(record).map_err(io::Error::other)?;
        bytes.push(b'\n');
        let temporary = dir.join(format!(".{name}.{}", random_id()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o644)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, &target)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
        File::open(dir)?.sync_all()
    };
    write().map_err(|err| fs_error(err, format!("write {}", target.display())))
}

const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// Whether `name` has the shape of [`random_id`]: the names of layers and tmp/
/// entries.
pub(crate) fn is_random_id(name: impl AsRef<std::ffi::OsStr>) -> bool {
    let name = name.as_ref().as_bytes();
    name.len() == 10 && name.iter().all(|b| ALPHABET.contains(b))
}

/// Whether `id` is `prefix`, an underscore, and a [`random_id`], such as
/// `snap_0123456789`.
pub(crate) fn is_id(prefix: &str, id: &str) -> bool {
    id.strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('_'))
        .is_some_and(is_random_id)
}

/// A temporary file [`write_record`] left: `.<name>.<random id>`.
fn is_temporary_record(name: &std::ffi::OsStr) -> bool {
    let name = name.as_bytes();
    name.len() > 12
        && name.starts_with(b".")
        && name[name.len() - 11] == b'.'
        && is_random_id(std::ffi::OsStr::from_bytes(&name[name.len() - 10..]))
}

/// A record whose contents can't be right, such as an ID that isn't one.
pub(crate) fn damaged(path: &Path, problem: impl AsRef<str>) -> Error {
    Error::new(
        Reason::InvalidStore,
        format!(
            "Can't use {}. It isn't a valid beavy record: {}.",
            path.display(),
            problem.as_ref()
        ),
    )
}

/// Write everything on the filesystem that holds `dir` to disk, so a record never
/// names data a power loss could take away.
pub(crate) fn sync_data(dir: &File, shown: &Path) -> Result<(), Error> {
    rustix::fs::syncfs(dir)
        .map_err(|err| fs_error(err.into(), format!("write {} to disk", shown.display())))
}

/// Ten random characters from a lowercase alphabet without look-alikes.
pub(crate) fn random_id() -> String {
    let mut bytes = [0u8; 10];
    let mut filled = 0;
    while filled < bytes.len() {
        // SAFETY: the pointer and length describe the unfilled part of `bytes`.
        let n = unsafe {
            libc::getrandom(bytes[filled..].as_mut_ptr().cast(), bytes.len() - filled, 0)
        };
        if n > 0 {
            filled += n as usize;
        } else if io::Error::last_os_error().kind() != ErrorKind::Interrupted {
            panic!("getrandom failed: {}", io::Error::last_os_error());
        }
    }
    bytes
        .iter()
        .map(|b| char::from(ALPHABET[usize::from(*b) % 32]))
        .collect()
}

/// Check a snapshot or worktree name: 1 to 63 lowercase letters, numbers, and
/// hyphens, starting and ending with a letter or number. IDs contain `_`, so a
/// name never looks like an ID.
pub fn validate_name(name: &str) -> Result<(), Error> {
    let ok = (1..=63).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if ok {
        Ok(())
    } else {
        Err(Error::new(
            Reason::InvalidArguments,
            format!(
                "Can't use the name {name:?}. Names are 1 to 63 lowercase letters, numbers, and hyphens, and start and end with a letter or number."
            ),
        ))
    }
}

pub(crate) fn lock_file(file: &File, operation: libc::c_int) -> io::Result<()> {
    loop {
        // SAFETY: the descriptor is open for the duration of the call.
        if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.kind() != ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Try to take a lock without waiting. `Ok(false)` means another process holds it.
pub(crate) fn try_lock_file(file: &File, operation: libc::c_int) -> io::Result<bool> {
    match lock_file(file, operation | libc::LOCK_NB) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == ErrorKind::WouldBlock => Ok(false),
        Err(err) => Err(err),
    }
}

/// Kill this process at a named point when `__BEAVY_CRASH_AT` names it. Tests use
/// it to prove that the next command repairs an interrupted one.
pub(crate) fn crash_point(name: &str) {
    if std::env::var_os("__BEAVY_CRASH_AT").is_some_and(|value| value == name) {
        // SAFETY: kill has no preconditions; SIGKILL ends the process here.
        unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
    }
}

/// An operating-system failure on `action`, such as `write /x/y`.
pub(crate) fn fs_error(err: io::Error, action: impl AsRef<str>) -> Error {
    let action = action.as_ref();
    match err.kind() {
        ErrorKind::PermissionDenied => Error::new(
            Reason::PermissionDenied,
            format!("Can't {action}. Permission denied."),
        ),
        _ => Error::new(Reason::IoError, format!("Failed to {action}.")),
    }
    .with_cause(err)
}

pub(crate) fn cleanup_warning(path: &Path, err: &io::Error) -> Warning {
    Warning {
        reason: WarningReason::CleanupFailed,
        message: format!(
            "Couldn't remove {}: {err}. The next change to the store retries.",
            path.display()
        ),
    }
}

/// The device and inode of a path, without following a final symlink.
pub(crate) fn identity(path: &Path) -> Option<(u64, u64)> {
    fs::symlink_metadata(path)
        .ok()
        .map(|metadata| (metadata.dev(), metadata.ino()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        }
    }

    #[test]
    fn the_default_store_lives_outside_the_project() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let home_path = home.path().to_str().unwrap();
        let location = locate(project.path(), None, env(&[("HOME", home_path)])).unwrap();
        let project_path = project.path().canonicalize().unwrap();
        assert_eq!(location.selection, Selection::Default);
        assert_eq!(location.project, project_path);
        assert_eq!(
            location.path,
            home.path()
                .join(".local/share/beavy/stores")
                .join(default_name(&project_path))
        );
        assert!(!location.path.starts_with(&project_path));
        assert_eq!(
            default_name(Path::new("/w/Acme Web")).len(),
            "acme-web-".len() + 8
        );
        assert!(default_name(Path::new("/")).starts_with("root-"));
    }

    #[test]
    fn the_option_beats_the_variable_and_an_empty_variable_is_unset() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let vars = [("HOME", "/home/a"), (STORE_VARIABLE, "/var/beavy")];
        let chosen = locate(&cwd, Some(Path::new("s")), env(&vars)).unwrap();
        assert_eq!(
            (chosen.path, chosen.selection),
            (cwd.join("s"), Selection::Option)
        );
        let chosen = locate(&cwd, None, env(&vars)).unwrap();
        assert_eq!(
            (chosen.path, chosen.selection),
            (PathBuf::from("/var/beavy"), Selection::Environment)
        );
        let empty = [("HOME", "/home/a"), (STORE_VARIABLE, "")];
        assert_eq!(
            locate(&cwd, None, env(&empty)).unwrap().selection,
            Selection::Default
        );
        let err = locate(&cwd, None, env(&[])).unwrap_err();
        assert_eq!(err.reason(), Reason::MissingArgument);
    }

    #[test]
    fn the_project_is_the_git_root_or_an_ancestor_with_a_store() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let nested = root_path.join("packages/web");
        fs::create_dir_all(&nested).unwrap();
        let vars = [("HOME", home.path().to_str().unwrap())];
        assert_eq!(locate(&nested, None, env(&vars)).unwrap().project, nested);
        fs::create_dir(root_path.join(".git")).unwrap();
        assert_eq!(
            locate(&nested, None, env(&vars)).unwrap().project,
            root_path
        );
        let store = locate(&root_path.join("packages"), None, env(&vars)).unwrap();
        fs::remove_dir(root_path.join(".git")).unwrap();
        let packages = root_path.join("packages");
        let store_of_packages = home
            .path()
            .join(".local/share/beavy/stores")
            .join(default_name(&packages));
        fs::create_dir_all(&store_of_packages).unwrap();
        assert_eq!(locate(&nested, None, env(&vars)).unwrap().project, packages);
        assert_eq!(store.project, root_path);
    }

    #[test]
    fn names_never_look_like_ids() {
        for good in ["base", "fix-login-2", "a", "0"] {
            assert!(validate_name(good).is_ok(), "{good}");
        }
        for bad in ["", "Base", "snap_abc", "-x", "x-", "a b", &"a".repeat(64)] {
            assert_eq!(
                validate_name(bad).unwrap_err().reason(),
                Reason::InvalidArguments
            );
        }
        let id = random_id();
        assert_eq!(id.len(), 10);
        assert_ne!(id, random_id());
    }

    #[test]
    fn records_are_written_atomically_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        write_record(dir.path(), "r.json", &StoreRecord { format: 7 }).unwrap();
        let record: StoreRecord = read_record(&dir.path().join("r.json")).unwrap().unwrap();
        assert_eq!(record.format, 7);
        assert_eq!(list_dir(dir.path()).unwrap(), ["r.json"]);
        assert!(
            read_record::<StoreRecord>(&dir.path().join("none.json"))
                .unwrap()
                .is_none()
        );
        fs::write(dir.path().join("bad.json"), b"{").unwrap();
        let err = read_record::<StoreRecord>(&dir.path().join("bad.json")).unwrap_err();
        assert_eq!(err.reason(), Reason::InvalidStore);
    }

    #[test]
    fn repair_removes_unlocked_scratch_and_unused_layers() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("store"));
        let (guard, warnings) = store.open_for_change().unwrap();
        assert!(warnings.is_empty());
        let kept = store.scratch(&guard).unwrap();
        fs::create_dir(store.join(TMP).join(random_id())).unwrap();
        fs::create_dir_all(store.join(LAYERS).join(random_id()).join("sub")).unwrap();
        let temporary = format!(".snap_0123456789.json.{}", random_id());
        fs::write(store.join(SNAPSHOTS).join(temporary), b"").unwrap();
        drop(guard);
        let (_guard, warnings) = store.open_for_change().unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            list_dir(&store.join(TMP)).unwrap(),
            [kept.path().file_name().unwrap()]
        );
        assert!(list_dir(&store.join(LAYERS)).unwrap().is_empty());
        assert!(list_dir(&store.join(SNAPSHOTS)).unwrap().is_empty());
    }

    #[test]
    fn a_store_is_made_only_in_an_empty_directory_and_repair_keeps_foreign_names() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        fs::create_dir_all(data.join("tmp/build-cache")).unwrap();
        fs::create_dir_all(data.join("layers/photos")).unwrap();
        let err = Store::at(&data).open_for_change().err().unwrap();
        assert_eq!(err.reason(), Reason::InvalidArguments);
        assert!(data.join("tmp/build-cache").exists());
        assert!(data.join("layers/photos").exists());

        let store = Store::at(dir.path().join("store"));
        drop(store.open_for_change().unwrap());
        for (dir, name) in [(TMP, "notes"), (LAYERS, "photos"), (TRASH, "keep")] {
            fs::create_dir(store.join(dir).join(name)).unwrap();
        }
        fs::write(store.join(SNAPSHOTS).join(".hidden"), b"").unwrap();
        drop(store.open_for_change().unwrap());
        for (dir, name) in [
            (TMP, "notes"),
            (LAYERS, "photos"),
            (TRASH, "keep"),
            (SNAPSHOTS, ".hidden"),
        ] {
            assert!(store.join(dir).join(name).exists(), "{dir}/{name}");
        }
        assert!(is_id("snap", "snap_0123456789"));
        for bad in [
            "snap_012345678",
            "snap_../../xx",
            "wt_0123456789",
            "snap_ABCDEFGHJK",
        ] {
            assert!(!is_id("snap", bad), "{bad}");
        }
    }
}
