//! Worktrees: a snapshot plus a private upper layer, mounted with overlayfs over
//! the directory the snapshot was imported from.
//!
//! ```text
//! worktrees/<id>/worktree.json   the record
//! worktrees/<id>/upper/          the worktree's own changes
//! worktrees/<id>/work/           overlayfs's work directory
//! worktrees/<id>/lock            held while a command enters the worktree
//! worktrees/<id>/runs/<entry>    one per running command, locked while it runs
//! worktrees/<id>/seal.json       a seal that hasn't finished
//! ```
//!
//! A command runs in a worktree inside its own mount namespace, with the worktree
//! mounted over the project's real path, so every tool sees the paths it always
//! sees. Commands that run in the same worktree at the same time share one mount:
//! the first one mounts it, and the rest join its namespace. A worktree is never
//! mounted twice, so two mounts never write to one upper layer.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use rustix::fs::FileType;
use serde::{Deserialize, Serialize};

use super::overlay::{self, Backend, Enter, Entry, Failure, Opened, Privilege, Spec, c_path};
use super::snapshot::{self, Method, Snapshot, SnapshotRecord};
use super::squash::{self, SQUASH_DEPTH};
use super::trash::Kind;
use super::{
    FORMAT, Guard, LAYERS, SNAPSHOTS, Store, WORKTREES, at, crash_point, damaged, fs_error, is_id,
    is_random_id, is_temporary_record, list_dir, lock_file, owner, random_id, read_record,
    try_lock_file, validate_name, write_record,
};
use crate::envelope::Warning;
use crate::error::{Error, Next, Reason};
use crate::sys::IdMaps;
use crate::time;

/// The environment variable that tells a command which worktree it runs in.
pub const WORKTREE_VARIABLE: &str = "BEAVY_WORKTREE";

const RECORD: &str = "worktree.json";
const SEAL: &str = "seal.json";
const UPPER: &str = "upper";
const WORK: &str = "work";
const RUNS: &str = "runs";
const LOCK: &str = "lock";
const PROBE: &str = "probe";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorktreeRecord {
    pub format: u32,
    pub id: String,
    pub name: Option<String>,
    /// The snapshot under the upper layer.
    pub snapshot: String,
    pub created_at: String,
}

/// A seal that hasn't finished: the layer it adds and the snapshot it records.
#[derive(Debug, Serialize, Deserialize)]
struct SealRecord {
    format: u32,
    layer: String,
    snapshot: SnapshotRecord,
}

/// A worktree and what's using it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Worktree {
    /// `wt_` and 10 characters. Stable for the worktree's life.
    pub id: String,
    pub name: Option<String>,
    /// The snapshot under the worktree's own changes: the one it was created from,
    /// or the one it was last sealed into.
    pub snapshot: String,
    /// The directory the worktree covers while a command runs in it: the one its
    /// snapshot was imported from.
    pub path: String,
    /// Directories where the worktree is mounted for every process.
    pub mounts: Vec<String>,
    /// Process IDs of commands running in the worktree.
    pub runs: Vec<u32>,
    pub created_at: String,
}

#[derive(Debug)]
pub struct CreatedWorktree {
    pub worktree: Worktree,
    pub backend: Backend,
    pub warnings: Vec<Warning>,
}

#[derive(Debug)]
pub struct DeletedWorktree {
    pub worktree: Worktree,
    pub warnings: Vec<Warning>,
}

/// The result of mounting or unmounting a worktree.
#[derive(Debug)]
pub struct Mounted {
    pub worktree: Worktree,
    /// False when the worktree was already in the requested state.
    pub changed: bool,
    pub warnings: Vec<Warning>,
}

/// The result of sealing a worktree into a snapshot.
#[derive(Debug)]
pub struct Sealed {
    pub snapshot: Snapshot,
    /// The worktree, now on the new snapshot with an empty upper layer.
    pub worktree: Worktree,
    /// The chain was squashed to its base and one layer.
    pub squashed: bool,
    pub warnings: Vec<Warning>,
}

/// A command ready to run in a worktree. Dropping it without running the command
/// leaves the worktree as it was.
pub struct Prepared {
    command: Command,
    program: OsString,
    worktree: Worktree,
    privilege: Privilege,
    store: PathBuf,
    run: File,
    _entering: File,
    _acting: owner::Acting,
    pub warnings: Vec<Warning>,
}

impl Prepared {
    pub fn worktree(&self) -> &Worktree {
        &self.worktree
    }

    pub fn privilege(&self) -> Privilege {
        self.privilege
    }

    /// Replace this process with the command, running in the worktree. The command
    /// keeps this process's ID, standard streams, and signals, so its exit status is
    /// the caller's. Returns only when that fails. The process must have one thread.
    pub fn exec(mut self) -> Error {
        // SAFETY: getpid has no preconditions.
        let pid = unsafe { libc::getpid() } as u32;
        if let Err(err) = self.record_pid(pid) {
            return err;
        }
        let err = self.command.exec();
        self.failure(err)
    }

    /// Start the command in the worktree as a child process.
    pub fn spawn(mut self) -> Result<Child, Error> {
        let child = self.command.spawn().map_err(|err| self.failure(err))?;
        self.record_pid(child.id())?;
        Ok(child)
    }

    fn record_pid(&mut self, pid: u32) -> Result<(), Error> {
        self.run
            .write_all(pid.to_string().as_bytes())
            .map_err(|err| fs_error(err, "record the run"))
    }

    fn failure(&self, err: std::io::Error) -> Error {
        let code = err.raw_os_error().unwrap_or(0);
        let (stage, errno) = ((code >> 16) as u8, code & 0xffff);
        let program = self.program.to_string_lossy();
        if stage == crate::sys::STAGE_USER {
            return Error::new(
                Reason::PermissionDenied,
                format!("Can't run {program} as the user who ran sudo."),
            )
            .with_cause(std::io::Error::from_raw_os_error(errno));
        }
        if stage != 0 {
            let err = std::io::Error::from_raw_os_error(errno);
            return overlay::unavailable(stage, &err, self.privilege, &self.store);
        }
        match err.kind() {
            ErrorKind::NotFound => Error::new(
                Reason::NotFound,
                format!(
                    "Couldn't find command {program} in worktree {}.",
                    self.worktree.id
                ),
            ),
            ErrorKind::PermissionDenied => Error::new(
                Reason::PermissionDenied,
                format!("Can't run {program}. Permission denied."),
            ),
            _ => Error::new(Reason::IoError, format!("Failed to run {program}.")),
        }
        .with_cause(err)
    }
}

impl Store {
    /// Every worktree, newest first.
    pub fn worktrees(&self) -> Result<Vec<Worktree>, Error> {
        let _acting = self.act()?;
        let records = self.read_worktrees()?;
        let snapshots = self.read_snapshots()?;
        let table = overlay::mounts(None).unwrap_or_default();
        Ok(records
            .iter()
            .map(|record| self.view_worktree(record, &snapshots, &table))
            .collect())
    }

    /// The worktree whose ID or name is `selector`.
    pub fn worktree(&self, selector: &str) -> Result<Worktree, Error> {
        let _acting = self.act()?;
        let records = self.read_worktrees()?;
        let record = find(&records, selector).ok_or_else(|| self.worktree_not_found(selector))?;
        self.fresh_view(record)
    }

    /// Create a worktree of a snapshot. Mounts it once in a throwaway namespace
    /// first, so a worktree beavy records is one this machine can run.
    pub fn create_worktree(
        &self,
        snapshot: &str,
        name: Option<&str>,
    ) -> Result<CreatedWorktree, Error> {
        if let Some(name) = name {
            validate_name(name)?;
        }
        let _acting = self.act()?;
        if !self.exists()? {
            return Err(self.snapshot_not_found(snapshot));
        }
        let backend = Backend::detect();
        let (guard, mut warnings) = self.open_for_change()?;
        let snapshots = self.snapshot_records()?;
        let parent = snapshot::find(&snapshots, snapshot)
            .ok_or_else(|| self.snapshot_not_found(snapshot))?
            .clone();
        if let Some(name) = name {
            self.ensure_worktree_name_free(name)?;
        }

        let scratch = self.scratch(&guard)?;
        let relative = scratch
            .path()
            .strip_prefix(self.path())
            .map_err(|err| Error::new(Reason::InternalError, err.to_string()))?
            .to_path_buf();
        let record = WorktreeRecord {
            format: FORMAT,
            id: format!("wt_{}", random_id()),
            name: name.map(str::to_owned),
            snapshot: parent.id.clone(),
            created_at: time::now(),
        };
        self.new_upper(&scratch.path().join(UPPER), &parent.layers)?;
        for dir in [WORK, RUNS, PROBE] {
            let path = scratch.path().join(dir);
            fs::create_dir(&path)
                .map_err(|err| fs_error(err, format!("create {}", path.display())))?;
        }
        write_record(scratch.path(), RECORD, &record)?;

        let spec = self.spec(&parent.layers, &relative)?;
        let point = self.open_beneath(&relative.join(PROBE))?;
        overlay::probe(&spec, &point, backend.privilege)
            .map_err(|failure| self.mount_failure(failure, backend.privilege))?;
        crash_point("worktree-create");
        let probe = scratch.path().join(PROBE);
        fs::remove_dir(&probe)
            .map_err(|err| fs_error(err, format!("remove {}", probe.display())))?;

        let worktrees = self.join(WORKTREES);
        fs::create_dir_all(&worktrees)
            .map_err(|err| fs_error(err, format!("create {}", worktrees.display())))?;
        let target = worktrees.join(&record.id);
        scratch
            .persist(&target)
            .map_err(|err| fs_error(err, format!("create {}", target.display())))?;
        drop(guard);
        warnings.extend(overlay::store_on_overlay(self.path()));
        Ok(CreatedWorktree {
            worktree: self.fresh_view(&record)?,
            backend,
            warnings,
        })
    }

    /// Move a worktree to the trash, unmounting it first. Refused while a command
    /// runs in it.
    pub fn delete_worktree(&self, selector: &str) -> Result<DeletedWorktree, Error> {
        let _acting = self.act()?;
        if !self.exists()? {
            return Err(self.worktree_not_found(selector));
        }
        let (guard, warnings) = self.open_for_change()?;
        let record = self.find_worktree(selector)?;
        let _entering = self.lock_worktree(&record.id)?;
        let worktree = self.fresh_view(&record)?;
        self.ensure_idle(&worktree)?;
        self.unmount_all(&worktree)?;
        let dir = self.join(WORKTREES).join(&record.id);
        self.move_to_trash(
            &guard,
            Kind::Worktree,
            &record.id,
            record.name.as_deref(),
            &dir,
        )?;
        Ok(DeletedWorktree {
            worktree: Worktree {
                mounts: Vec::new(),
                ..worktree
            },
            warnings,
        })
    }

    /// Mount a worktree at `dir` for every process. Needs root.
    pub fn mount_worktree(&self, selector: &str, dir: &Path) -> Result<Mounted, Error> {
        let _acting = self.act()?;
        let privilege = Privilege::detect();
        if privilege != Privilege::Root {
            return Err(Error::new(
                Reason::PermissionDenied,
                format!(
                    "Can't mount worktree {selector} at {}. Only root can mount for every process.",
                    dir.display()
                ),
            )
            .with_next(
                "Run a command in the worktree instead",
                self.next(&["beavy", "worktrees", "run", selector, "--", "<COMMAND>"]),
            ));
        }
        let shown = dir.display();
        let dir = fs::canonicalize(dir).map_err(|err| match err.kind() {
            ErrorKind::NotFound => Error::new(
                Reason::NotFound,
                format!("Couldn't find directory {shown}."),
            )
            .with_cause(err),
            _ => fs_error(err, format!("resolve directory {shown}")),
        })?;
        if !dir.is_dir() {
            return Err(Error::new(
                Reason::InvalidArguments,
                format!("Can't mount a worktree at {shown}. It isn't a directory."),
            ));
        }
        if dir.starts_with(self.absolute()) {
            return Err(Error::new(
                Reason::InvalidArguments,
                format!("Can't mount a worktree at {shown}. It's inside the store."),
            ));
        }
        if self.absolute().starts_with(&dir) {
            return Err(Error::new(
                Reason::InvalidArguments,
                format!(
                    "Can't mount a worktree at {shown}. The store is inside it, and the mount would hide the store."
                ),
            ));
        }
        let point =
            at::open_dir_no_symlinks(&dir).map_err(|err| fs_error(err, format!("open {shown}")))?;
        if let Some(uid) = owner::acting_uid() {
            let owner_uid = at::fstat(&point)
                .map_err(|err| fs_error(err, format!("read {shown}")))?
                .uid;
            if owner_uid != uid {
                return Err(Error::new(
                    Reason::PermissionDenied,
                    format!(
                        "Can't mount a worktree at {shown}. It belongs to {}, and beavy mounts a worktree of {}'s store only on a directory {} owns.",
                        owner::name_of(owner_uid),
                        owner::name_of(uid),
                        owner::name_of(uid)
                    ),
                ));
            }
        }
        let (_guard, warnings) = self.open_for_change()?;
        let record = self.find_worktree(selector)?;
        let _entering = self.lock_worktree(&record.id)?;
        let worktree = self.fresh_view(&record)?;
        let shown_dir = dir.to_string_lossy();
        if worktree.mounts.iter().any(|point| *point == shown_dir) {
            return Ok(Mounted {
                worktree,
                changed: false,
                warnings,
            });
        }
        if let Some(point) = worktree.mounts.first() {
            return Err(Error::new(
                Reason::WorktreeInUse,
                format!(
                    "Can't mount worktree {} at {shown}. It's mounted at {point}, and a worktree has one mount at a time.",
                    record.id
                ),
            )
            .with_next(
                "Unmount it first",
                self.next(&["beavy", "worktrees", "unmount", &record.id]),
            ));
        }
        self.ensure_idle(&worktree)?;
        let snapshot = self.snapshot_of(&record)?;
        let spec = self.spec(&snapshot.layers, &Path::new(WORKTREES).join(&record.id))?;
        overlay::mount_persistent(&spec, &point)
            .map_err(|failure| self.mount_failure(failure, privilege))?;
        Ok(Mounted {
            worktree: self.fresh_view(&record)?,
            changed: true,
            warnings,
        })
    }

    /// Unmount a worktree's persistent mount.
    pub fn unmount_worktree(&self, selector: &str) -> Result<Mounted, Error> {
        let _acting = self.act()?;
        if !self.exists()? {
            return Err(self.worktree_not_found(selector));
        }
        let (_guard, warnings) = self.open_for_change()?;
        let record = self.find_worktree(selector)?;
        let _entering = self.lock_worktree(&record.id)?;
        let worktree = self.fresh_view(&record)?;
        if worktree.mounts.is_empty() {
            return Ok(Mounted {
                worktree,
                changed: false,
                warnings,
            });
        }
        self.ensure_idle(&worktree)?;
        self.unmount_all(&worktree)?;
        Ok(Mounted {
            worktree: self.fresh_view(&record)?,
            changed: true,
            warnings,
        })
    }

    /// Prepare `program` to run in a worktree, with `cwd` as its working directory
    /// when that's inside the worktree's path, and the path itself otherwise.
    pub fn prepare_run(
        &self,
        selector: &str,
        cwd: &Path,
        program: &OsStr,
        args: &[OsString],
    ) -> Result<Prepared, Error> {
        let acting = self.act()?;
        if !self.exists()? {
            return Err(self.worktree_not_found(selector));
        }
        let privilege = Privilege::detect();
        let (guard, warnings) = self.open_for_change()?;
        let record = self.find_worktree(selector)?;
        drop(guard);
        let entering = self.lock_worktree(&record.id)?;
        let record = self.find_worktree(&record.id)?;
        let snapshot = self.snapshot_of(&record)?;
        let target = PathBuf::from(&snapshot.source);
        let point = File::open(&target)
            .map(OwnedFd::from)
            .ok()
            .filter(|point| at::fstat(point).is_ok_and(|stat| stat.kind == FileType::Directory))
            .ok_or_else(|| {
                Error::new(
                    Reason::NotFound,
                    format!(
                        "Can't run in worktree {}. Commands run in a worktree at the directory its snapshot was imported from, and {} doesn't exist.",
                        record.id,
                        target.display()
                    ),
                )
            })?;
        if let Some(uid) = owner::acting_uid() {
            let owner_uid = at::fstat(&point)
                .map_err(|err| fs_error(err, format!("read {}", target.display())))?
                .uid;
            if owner_uid != uid {
                return Err(Error::new(
                    Reason::PermissionDenied,
                    format!(
                        "Can't run in worktree {} at {}. It belongs to {}, and beavy mounts a worktree of {}'s store only on a directory {} owns.",
                        record.id,
                        target.display(),
                        owner::name_of(owner_uid),
                        owner::name_of(uid),
                        owner::name_of(uid)
                    ),
                ));
            }
        }
        let point = Opened::new(&target, point)?;
        let cwd = fs::canonicalize(cwd)
            .ok()
            .filter(|cwd| cwd.starts_with(&target))
            .unwrap_or_else(|| target.clone());
        let source = self.worktree_source(&record.id);
        let live = self.live_runs(&record.id, true);
        let table = overlay::mounts(None).unwrap_or_default();
        let entry = if let Some(point) = overlay::mounts_of(&table, &source).first() {
            Entry::Bind(c_path(point)?)
        } else if live.is_empty() {
            Entry::Mount(self.spec(&snapshot.layers, &Path::new(WORKTREES).join(&record.id))?)
        } else {
            self.join_run(&record.id, &live, &target, &source)?
        };

        let runs = self.join(WORKTREES).join(&record.id).join(RUNS);
        fs::create_dir_all(&runs)
            .map_err(|err| fs_error(err, format!("create {}", runs.display())))?;
        let path = runs.join(random_id());
        let run = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&path)
            .and_then(|file| lock_file(&file, libc::LOCK_EX).map(|()| file))
            .map_err(|err| fs_error(err, format!("create {}", path.display())))?;
        let enter = Enter {
            entry,
            privilege,
            maps: IdMaps::identity(),
            point,
            cwd: c_path(&cwd)?,
            keep: run.as_raw_fd(),
            user: owner::command_user().map(|user| {
                let groups = owner::groups_of(&user);
                (user, groups)
            }),
        };
        let mut command = Command::new(program);
        command.args(args).env(WORKTREE_VARIABLE, &record.id);
        // SAFETY: enter makes only async-signal-safe calls.
        unsafe {
            command.pre_exec(move || {
                enter.enter().map_err(|(stage, errno)| {
                    std::io::Error::from_raw_os_error(i32::from(stage) << 16 | errno)
                })
            });
        }
        Ok(Prepared {
            command,
            program: program.to_owned(),
            worktree: self.fresh_view(&record)?,
            privilege,
            store: self.path().to_path_buf(),
            run,
            _entering: entering,
            _acting: acting,
            warnings,
        })
    }

    /// Seal a worktree into a new snapshot: its upper layer becomes the snapshot's
    /// top layer, and the worktree carries on from the new snapshot with an empty
    /// upper layer. Refused while a command runs in it.
    pub fn create_snapshot_from_worktree(
        &self,
        selector: &str,
        name: Option<&str>,
    ) -> Result<Sealed, Error> {
        if let Some(name) = name {
            validate_name(name)?;
        }
        let _acting = self.act()?;
        if !self.exists()? {
            return Err(self.worktree_not_found(selector));
        }
        let (guard, mut warnings) = self.open_for_change()?;
        if let Some(name) = name {
            self.ensure_snapshot_name_free(name)?;
        }
        let record = self.find_worktree(selector)?;
        let _entering = self.lock_worktree(&record.id)?;
        let before = self.fresh_view(&record)?;
        self.ensure_idle(&before)?;
        self.unmount_all(&before)?;
        let parent = self.snapshot_of(&record)?;
        let dir = self.join(WORKTREES).join(&record.id);
        let upper = dir.join(UPPER);

        let layer = random_id();
        let squashed = parent.layers.len() + 1 > SQUASH_DEPTH;
        let merged = if squashed {
            let scratch = self.scratch(&guard)?;
            let mut sources: Vec<PathBuf> = parent.layers[1..]
                .iter()
                .map(|layer| self.join(LAYERS).join(layer))
                .collect();
            sources.push(upper.clone());
            let base = self.join(LAYERS).join(&parent.layers[0]);
            squash::merge(&base, &sources, scratch.path())?;
            Some(scratch)
        } else {
            None
        };
        let layers = if squashed {
            vec![parent.layers[0].clone(), layer.clone()]
        } else {
            let mut layers = parent.layers.clone();
            layers.push(layer.clone());
            layers
        };
        let mut stack: Vec<PathBuf> = vec![upper.clone()];
        stack.extend(
            parent
                .layers
                .iter()
                .rev()
                .map(|layer| self.join(LAYERS).join(layer)),
        );
        let commit = super::git::head(|relative| {
            squash::resolve(&stack, relative).and_then(|path| fs::read_to_string(path).ok())
        });
        let journal = SealRecord {
            format: FORMAT,
            layer: layer.clone(),
            snapshot: SnapshotRecord {
                format: FORMAT,
                id: format!("snap_{}", random_id()),
                name: name.map(str::to_owned),
                parent: Some(parent.id.clone()),
                layers,
                source: parent.source.clone(),
                commit,
                method: Method::Seal,
                worktree: Some(record.id.clone()),
                created_at: time::now(),
            },
        };
        write_record(&dir, SEAL, &journal)?;
        crash_point("seal-journal");
        let target = self.join(LAYERS).join(&layer);
        match merged {
            Some(scratch) => scratch.persist(&target),
            None => fs::rename(&upper, &target),
        }
        .map_err(|err| {
            fs_error(
                err,
                format!("move the sealed layer to {}", target.display()),
            )
        })?;
        crash_point("seal-layer");
        warnings.extend(self.finish_seal(&guard, &dir, &journal)?);
        let relative = Path::new(WORKTREES).join(&record.id);
        for point in &before.mounts {
            let mounted = self
                .spec(&journal.snapshot.layers, &relative)
                .and_then(|spec| {
                    let dir = at::open_dir_no_symlinks(Path::new(point))
                        .map_err(|err| fs_error(err, format!("open {point}")))?;
                    overlay::mount_persistent(&spec, &dir)
                        .map_err(|failure| self.mount_failure(failure, Privilege::Root))
                });
            if let Err(err) = mounted {
                warnings.push(Warning {
                    reason: crate::envelope::WarningReason::RemountFailed,
                    message: format!(
                        "Sealed worktree {}, but couldn't mount it at {point} again: {}",
                        record.id,
                        err.message()
                    ),
                });
            }
        }
        drop(guard);

        let snapshots = self.snapshot_records()?;
        Ok(Sealed {
            snapshot: snapshot::view(&journal.snapshot, &snapshots),
            worktree: self.fresh_view(&self.find_worktree(&record.id)?)?,
            squashed,
            warnings,
        })
    }

    /// Record the sealed snapshot, give the worktree a fresh upper layer on it, and
    /// drop the journal. Safe to repeat: repair calls it for a seal that a crash
    /// interrupted after its layer was in place.
    fn finish_seal(
        &self,
        guard: &Guard,
        dir: &Path,
        journal: &SealRecord,
    ) -> Result<Vec<Warning>, Error> {
        let mut warnings = Vec::new();
        let record_name = format!("{}.json", journal.snapshot.id);
        if !self.join(SNAPSHOTS).join(&record_name).exists() {
            write_record(&self.join(SNAPSHOTS), &record_name, &journal.snapshot)?;
        }
        let path = dir.join(RECORD);
        let mut record: WorktreeRecord = read_record(&path)?.ok_or_else(|| {
            Error::new(
                Reason::InvalidStore,
                format!(
                    "Can't finish sealing {}. Its record is missing.",
                    dir.display()
                ),
            )
        })?;
        check(&record, &path, &record.id)?;
        if record.snapshot != journal.snapshot.id {
            let upper = dir.join(UPPER);
            if fs::symlink_metadata(&upper).is_ok() {
                let scratch = self.scratch(guard)?;
                fs::rename(&upper, scratch.path().join(UPPER))
                    .map_err(|err| fs_error(err, format!("move {}", upper.display())))?;
                warnings.extend(scratch.discard());
            }
            self.new_upper(&upper, &journal.snapshot.layers)?;
            record.snapshot = journal.snapshot.id.clone();
            write_record(dir, RECORD, &record)?;
        }
        let seal = dir.join(SEAL);
        fs::remove_file(&seal)
            .map_err(|err| fs_error(err, format!("remove {}", seal.display())))?;
        Ok(warnings)
    }

    /// Finish or roll back seals a crash interrupted, and detach mounts of
    /// worktrees that no longer exist. Part of every change's repair.
    pub(crate) fn repair_worktrees(&self, guard: &Guard) -> Result<Vec<Warning>, Error> {
        let mut warnings = Vec::new();
        for id in list_dir(&self.join(WORKTREES))? {
            if !id.to_str().is_some_and(|id| is_id("wt", id)) {
                continue;
            }
            let dir = self.join(WORKTREES).join(&id);
            for name in list_dir(&dir)? {
                if is_temporary_record(&name) {
                    let _ = fs::remove_file(dir.join(name));
                }
            }
            let path = dir.join(SEAL);
            let Some(journal) = read_record::<SealRecord>(&path)? else {
                continue;
            };
            check_seal(&journal, &path)?;
            if self.join(LAYERS).join(&journal.layer).exists() {
                warnings.extend(self.finish_seal(guard, &dir, &journal)?);
            } else {
                let _ = fs::remove_file(dir.join(SEAL));
            }
        }
        if Privilege::detect() == Privilege::Root {
            let worktrees = self.absolute().join(WORKTREES);
            for mount in overlay::mounts(None).unwrap_or_default() {
                let source = Path::new(&mount.source);
                if mount.fstype == "overlay"
                    && source.starts_with(&worktrees)
                    && !source.join(RECORD).exists()
                {
                    overlay::detach(&mount.point);
                }
            }
        }
        Ok(warnings)
    }

    /// Worktree records, newest first; none when the store doesn't exist.
    pub(crate) fn read_worktrees(&self) -> Result<Vec<WorktreeRecord>, Error> {
        if !self.exists()? {
            return Ok(Vec::new());
        }
        self.worktree_records()
    }

    pub(crate) fn worktree_records(&self) -> Result<Vec<WorktreeRecord>, Error> {
        let mut records = Vec::new();
        for id in list_dir(&self.join(WORKTREES))? {
            let Some(id) = id.to_str().filter(|id| is_id("wt", id)) else {
                continue;
            };
            let path = self.join(WORKTREES).join(id).join(RECORD);
            if let Some(record) = read_record::<WorktreeRecord>(&path)? {
                check(&record, &path, id)?;
                records.push(record);
            }
        }
        records.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(records)
    }

    fn find_worktree(&self, selector: &str) -> Result<WorktreeRecord, Error> {
        let records = self.worktree_records()?;
        find(&records, selector)
            .cloned()
            .ok_or_else(|| self.worktree_not_found(selector))
    }

    fn fresh_view(&self, record: &WorktreeRecord) -> Result<Worktree, Error> {
        let snapshots = self.read_snapshots()?;
        let table = overlay::mounts(None).unwrap_or_default();
        Ok(self.view_worktree(record, &snapshots, &table))
    }

    fn view_worktree(
        &self,
        record: &WorktreeRecord,
        snapshots: &[SnapshotRecord],
        table: &[overlay::Mount],
    ) -> Worktree {
        let path = snapshots
            .iter()
            .find(|snapshot| snapshot.id == record.snapshot)
            .map(|snapshot| snapshot.source.clone())
            .unwrap_or_default();
        Worktree {
            id: record.id.clone(),
            name: record.name.clone(),
            snapshot: record.snapshot.clone(),
            path,
            mounts: overlay::mounts_of(table, &self.worktree_source(&record.id))
                .iter()
                .map(|point| point.to_string_lossy().into_owned())
                .collect(),
            runs: self.live_runs(&record.id, false),
            created_at: record.created_at.clone(),
        }
    }

    fn snapshot_of(&self, record: &WorktreeRecord) -> Result<SnapshotRecord, Error> {
        self.snapshot_records()?
            .into_iter()
            .find(|snapshot| snapshot.id == record.snapshot)
            .ok_or_else(|| {
                Error::new(
                    Reason::InvalidStore,
                    format!(
                        "Can't use worktree {}. Its snapshot {} is missing from store {}.",
                        record.id,
                        record.snapshot,
                        self.path().display()
                    ),
                )
            })
    }

    /// The worktree's directory as an absolute path: the source of its mounts.
    fn worktree_source(&self, id: &str) -> PathBuf {
        self.absolute().join(WORKTREES).join(id)
    }

    /// What mounting `layers` under the worktree directory `worktree`, relative to
    /// the store, needs: every directory open, so the mount can't be redirected.
    fn spec(&self, layers: &[String], worktree: &Path) -> Result<Spec, Error> {
        let mut dirs = Vec::new();
        for layer in layers {
            dirs.push(self.open_beneath(&Path::new(LAYERS).join(layer))?);
        }
        let upper = self.open_beneath(&worktree.join(UPPER))?;
        let work = self.open_beneath(&worktree.join(WORK))?;
        let options = overlay::options(&dirs, &upper, &work)?;
        dirs.extend([upper, work]);
        Ok(Spec {
            source: c_path(&self.absolute().join(worktree))?,
            options,
            dirs,
        })
    }

    /// Open the directory `relative` in the store without following a symlink.
    fn open_beneath(&self, relative: &Path) -> Result<Opened, Error> {
        let path = self.absolute().join(relative);
        at::open_dir(&self.absolute())
            .and_then(|store| at::open_dir_beneath(&store, relative))
            .map_err(|err| fs_error(err, format!("open {}", path.display())))
            .and_then(|fd| Opened::new(&path, fd))
    }

    fn mount_failure(&self, failure: Failure, privilege: Privilege) -> Error {
        match failure {
            Failure::Step { stage, err } => {
                overlay::unavailable(stage, &err, privilege, self.path())
            }
            Failure::Other(err) => err,
        }
    }

    /// An empty upper layer whose root looks like the top layer's root, since the
    /// root of the upper layer is the root a command sees.
    fn new_upper(&self, path: &Path, layers: &[String]) -> Result<(), Error> {
        let top = layers.last().map(|layer| self.join(LAYERS).join(layer));
        let metadata = top
            .as_deref()
            .and_then(|top| fs::symlink_metadata(top).ok());
        fs::DirBuilder::new()
            .mode(0o755)
            .create(path)
            .map_err(|err| fs_error(err, format!("create {}", path.display())))?;
        if let Some(metadata) = metadata {
            let set = || -> std::io::Result<()> {
                if owner::acting_as_root() {
                    std::os::unix::fs::lchown(path, Some(metadata.uid()), Some(metadata.gid()))?;
                }
                fs::set_permissions(path, Permissions::from_mode(metadata.mode() & 0o7777))
            };
            set().map_err(|err| fs_error(err, format!("update {}", path.display())))?;
        }
        Ok(())
    }

    /// Take the lock that serializes entering, sealing, mounting, and deleting a
    /// worktree. Fails when the worktree no longer exists once the lock is held.
    fn lock_worktree(&self, id: &str) -> Result<File, Error> {
        let dir = self.join(WORKTREES).join(id);
        let path = dir.join(LOCK);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o644)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|err| match err.kind() {
                ErrorKind::NotFound => self.worktree_not_found(id),
                _ => fs_error(err, format!("open {}", path.display())),
            })?;
        lock_file(&file, libc::LOCK_EX)
            .map_err(|err| fs_error(err, format!("lock {}", path.display())))?;
        if !dir.join(RECORD).exists() {
            return Err(self.worktree_not_found(id));
        }
        Ok(file)
    }

    /// Process IDs of commands running in a worktree. With `clean`, removes the
    /// entries of commands that have exited; only do that holding the worktree's
    /// lock.
    fn live_runs(&self, id: &str, clean: bool) -> Vec<u32> {
        let runs = self.join(WORKTREES).join(id).join(RUNS);
        let mut pids = Vec::new();
        for entry in list_dir(&runs).unwrap_or_default() {
            let path = runs.join(&entry);
            let Ok(mut file) = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)
            else {
                continue;
            };
            match try_lock_file(&file, libc::LOCK_EX) {
                Ok(true) => {
                    if clean {
                        let _ = fs::remove_file(&path);
                    }
                }
                Ok(false) => {
                    let mut text = String::new();
                    if file.read_to_string(&mut text).is_ok()
                        && let Ok(pid) = text.trim().parse()
                    {
                        pids.push(pid);
                    }
                }
                Err(_) => {}
            }
        }
        pids.sort_unstable();
        pids
    }

    /// Join the mount namespace of a command already running in the worktree.
    fn join_run(
        &self,
        id: &str,
        live: &[u32],
        target: &Path,
        source: &Path,
    ) -> Result<Entry, Error> {
        for pid in live {
            let mounted = overlay::mounts(Some(*pid)).is_ok_and(|table| {
                overlay::mounts_of(&table, source)
                    .iter()
                    .any(|p| p == target)
            });
            if !mounted {
                continue;
            }
            let open = |kind: &str| File::open(format!("/proc/{pid}/ns/{kind}")).ok();
            let (Some(mount), Some(user)) = (open("mnt"), open("user")) else {
                continue;
            };
            let same_user = fs::read_link(format!("/proc/{pid}/ns/user")).ok()
                == fs::read_link("/proc/self/ns/user").ok();
            return Ok(Entry::Join {
                user: (!same_user).then(|| user.into()),
                mount: mount.into(),
            });
        }
        Err(Error::new(
            Reason::WorktreeInUse,
            format!(
                "Can't run in worktree {id}. Commands are still running in it (process {}), and beavy can't join their mount namespace.",
                live.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
            ),
        )
        .with_next(
            "See what's running in it",
            self.next(&["beavy", "worktrees", "inspect", id]),
        ))
    }

    fn ensure_idle(&self, worktree: &Worktree) -> Result<(), Error> {
        let live = self.live_runs(&worktree.id, true);
        if live.is_empty() {
            return Ok(());
        }
        Err(Error::new(
            Reason::WorktreeInUse,
            format!(
                "Can't change worktree {} while commands run in it (process {}).",
                worktree.id,
                live.iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
        .with_next(
            "See what's running in it",
            self.next(&["beavy", "worktrees", "inspect", &worktree.id]),
        ))
    }

    fn unmount_all(&self, worktree: &Worktree) -> Result<(), Error> {
        for point in &worktree.mounts {
            match overlay::unmount(Path::new(point)) {
                Ok(_) => {}
                Err(err) if err.raw_os_error() == Some(libc::EBUSY) => {
                    return Err(Error::new(
                        Reason::WorktreeInUse,
                        format!(
                            "Can't unmount worktree {} from {point}. Processes have files open there.",
                            worktree.id
                        ),
                    )
                    .with_cause(err)
                    .with_next("See which processes", Next::new(["fuser", "-vm", point])));
                }
                Err(err) if err.raw_os_error() == Some(libc::EPERM) => {
                    return Err(Error::new(
                        Reason::PermissionDenied,
                        format!(
                            "Can't unmount worktree {} from {point}. Only root can.",
                            worktree.id
                        ),
                    )
                    .with_cause(err));
                }
                Err(err) => return Err(fs_error(err, format!("unmount {point}"))),
            }
        }
        Ok(())
    }

    fn ensure_worktree_name_free(&self, name: &str) -> Result<(), Error> {
        match self
            .worktree_records()?
            .iter()
            .find(|record| record.name.as_deref() == Some(name))
        {
            None => Ok(()),
            Some(taken) => Err(Error::new(
                Reason::NameTaken,
                format!(
                    "Can't name the worktree {name}. Worktree {} already has that name.",
                    taken.id
                ),
            )),
        }
    }

    pub(crate) fn worktree_not_found(&self, selector: &str) -> Error {
        Error::new(
            Reason::NotFound,
            format!(
                "Couldn't find worktree {selector} in store {}.",
                self.path().display()
            ),
        )
        .with_next(
            "See the worktrees",
            self.next(&["beavy", "worktrees", "list"]),
        )
    }
}

/// Check a worktree record read from `path` before any of it becomes part of a
/// path: its ID must be `id`, the name of its directory.
pub(crate) fn check(record: &WorktreeRecord, path: &Path, id: &str) -> Result<(), Error> {
    if !is_id("wt", &record.id) || record.id != id {
        return Err(damaged(path, format!("its ID {:?} isn't {id}", record.id)));
    }
    if !is_id("snap", &record.snapshot) {
        return Err(damaged(path, "its snapshot isn't a snapshot ID"));
    }
    if record
        .name
        .as_deref()
        .is_some_and(|name| validate_name(name).is_err())
    {
        return Err(damaged(path, "its name isn't a valid name"));
    }
    Ok(())
}

/// Check a seal journal: the layer it adds is a layer name, and the top of the
/// snapshot it records.
fn check_seal(journal: &SealRecord, path: &Path) -> Result<(), Error> {
    snapshot::check(&journal.snapshot, path, &journal.snapshot.id)?;
    if !is_random_id(&journal.layer) || journal.snapshot.layers.last() != Some(&journal.layer) {
        return Err(damaged(path, "its layer isn't the snapshot's top layer"));
    }
    Ok(())
}

pub(crate) fn find<'a>(
    records: &'a [WorktreeRecord],
    selector: &str,
) -> Option<&'a WorktreeRecord> {
    records
        .iter()
        .find(|record| record.id == selector)
        .or_else(|| {
            records
                .iter()
                .find(|record| record.name.as_deref() == Some(selector))
        })
}
