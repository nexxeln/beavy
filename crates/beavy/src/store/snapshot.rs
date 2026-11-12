//! Snapshots: immutable chains of layers. The first snapshot of a project imports a
//! directory into the store as its base layer. That copy is the only full copy
//! beavy makes, and the directory itself is never used as a layer.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::trash::Kind;
use super::{
    FORMAT, LAYERS, SNAPSHOTS, Store, crash_point, damaged, fs_error, git, identity, is_id,
    is_random_id, list_dir, random_id, read_record, sync_data, tree, validate_name, write_record,
};
use crate::envelope::{Warning, WarningReason};
use crate::error::{Error, Reason};
use crate::time;

/// How a snapshot's top layer was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Method {
    /// A copy of a directory.
    Copy,
    /// A copy of a directory whose files were all cloned with reflinks.
    Reflink,
    /// A worktree's changes, sealed.
    Seal,
}

impl Method {
    pub const fn as_str(self) -> &'static str {
        match self {
            Method::Copy => "copy",
            Method::Reflink => "reflink",
            Method::Seal => "seal",
        }
    }
}

/// A snapshot as stored in snapshots/<id>.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SnapshotRecord {
    pub format: u32,
    pub id: String,
    pub name: Option<String>,
    pub parent: Option<String>,
    /// Directories under layers/, bottom first.
    pub layers: Vec<String>,
    pub source: String,
    pub commit: Option<String>,
    pub method: Method,
    /// The worktree a sealed snapshot came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    pub created_at: String,
}

/// A snapshot and what depends on it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    /// `snap_` and 10 characters. Stable for the snapshot's life.
    pub id: String,
    pub name: Option<String>,
    /// The snapshot this one was made from, or null for an imported one.
    pub parent: Option<String>,
    /// The directory the base layer was imported from. Worktrees of this snapshot
    /// are entered at this path.
    pub source: String,
    /// The Git commit checked out when the snapshot was taken, if any.
    pub commit: Option<String>,
    pub method: Method,
    /// The worktree a sealed snapshot came from; null for an imported one.
    pub worktree: Option<String>,
    /// Layers in the chain, the base included.
    pub depth: usize,
    pub created_at: String,
    /// Snapshots whose parent is this one.
    pub children: Vec<String>,
}

/// The result of creating a snapshot from a directory.
#[derive(Debug)]
pub struct Created {
    pub snapshot: Snapshot,
    /// Files copied into the base layer, hard links included.
    pub files: u64,
    /// Bytes in those files, counting each hard-linked file once.
    pub bytes: u64,
    pub warnings: Vec<Warning>,
}

#[derive(Debug)]
pub struct Deleted {
    pub snapshot: Snapshot,
    pub warnings: Vec<Warning>,
}

impl Store {
    /// Every snapshot, newest first.
    pub fn snapshots(&self) -> Result<Vec<Snapshot>, Error> {
        let _acting = self.act()?;
        let records = self.read_snapshots()?;
        Ok(records
            .iter()
            .map(|record| view(record, &records))
            .collect())
    }

    /// The snapshot whose ID or name is `selector`.
    pub fn snapshot(&self, selector: &str) -> Result<Snapshot, Error> {
        let _acting = self.act()?;
        let records = self.read_snapshots()?;
        let record = find(&records, selector).ok_or_else(|| self.snapshot_not_found(selector))?;
        Ok(view(record, &records))
    }

    /// Import `directory` as a new base snapshot: clone its files into a new layer
    /// when the filesystem supports reflinks, copy them otherwise. `directory`
    /// itself is only read.
    pub fn create_snapshot(&self, directory: &Path, name: Option<&str>) -> Result<Created, Error> {
        if let Some(name) = name {
            validate_name(name)?;
        }
        let _acting = self.act()?;
        let shown = directory.display();
        let directory = fs::canonicalize(directory).map_err(|err| match err.kind() {
            ErrorKind::NotFound => Error::new(
                Reason::NotFound,
                format!("Couldn't find directory {shown}."),
            )
            .with_cause(err),
            _ => fs_error(err, format!("resolve directory {shown}")),
        })?;
        if !directory.is_dir() {
            return Err(Error::new(
                Reason::InvalidArguments,
                format!("Can't import {shown}. It isn't a directory."),
            ));
        }
        let Some(source) = directory.to_str().map(str::to_owned) else {
            return Err(Error::new(
                Reason::InvalidArguments,
                format!("Can't import {shown}. Its path isn't valid UTF-8."),
            ));
        };
        if let Ok(root) = fs::canonicalize(self.path())
            && directory.starts_with(&root)
        {
            return Err(Error::new(
                Reason::InvalidArguments,
                format!(
                    "Can't import {shown}. It's inside the store {}.",
                    root.display()
                ),
            ));
        }

        git::check(&directory)?;

        let (guard, mut warnings) = self.open_for_change()?;
        if let Some(name) = name {
            self.ensure_snapshot_name_free(name)?;
        }
        let scratch = self.scratch(&guard)?;
        drop(guard);

        let copied = tree::copy(
            &directory,
            scratch.dir(),
            scratch.path(),
            identity(self.path()),
        )?;
        crash_point("snapshot-import");
        let commit = git::head(git::reader(scratch.dir()));
        if let Some(commit) = &commit {
            git::detach(scratch.dir(), commit).map_err(|err| {
                fs_error(err, format!("detach HEAD in {}", scratch.path().display()))
            })?;
        }
        sync_data(scratch.dir(), scratch.path())?;
        let record = SnapshotRecord {
            format: FORMAT,
            id: format!("snap_{}", random_id()),
            name: name.map(str::to_owned),
            parent: None,
            layers: vec![random_id()],
            source,
            commit,
            method: if copied.cloned {
                Method::Reflink
            } else {
                Method::Copy
            },
            worktree: None,
            created_at: time::now(),
        };

        let guard = self.lock()?;
        if let Some(name) = name {
            self.ensure_snapshot_name_free(name)?;
        }
        let layer = self.join(LAYERS).join(&record.layers[0]);
        scratch.persist(&layer).map_err(|err| {
            fs_error(
                err,
                format!("move the imported tree to {}", layer.display()),
            )
        })?;
        write_record(
            &self.join(SNAPSHOTS),
            &format!("{}.json", record.id),
            &record,
        )?;
        drop(guard);

        if !copied.skipped.is_empty() {
            let shown: Vec<String> = copied
                .skipped
                .iter()
                .take(5)
                .map(|path| path.display().to_string())
                .collect();
            let more = copied.skipped.len().saturating_sub(5);
            let mut message = format!(
                "Skipped {} that can't be copied (sockets and device nodes): {}",
                plural(copied.skipped.len(), "entry", "entries"),
                shown.join(", ")
            );
            if more > 0 {
                message.push_str(&format!(", and {more} more"));
            }
            message.push('.');
            warnings.push(Warning {
                reason: WarningReason::EntriesSkipped,
                message,
            });
        }
        Ok(Created {
            snapshot: view(&record, std::slice::from_ref(&record)),
            files: copied.files,
            bytes: copied.bytes,
            warnings,
        })
    }

    /// Move a snapshot to the trash. Its layers stay until the trash is emptied.
    /// Refused while another snapshot was made from it or a worktree uses it.
    pub fn delete_snapshot(&self, selector: &str) -> Result<Deleted, Error> {
        let _acting = self.act()?;
        if !self.exists()? {
            return Err(self.snapshot_not_found(selector));
        }
        let (guard, warnings) = self.open_for_change()?;
        let records = self.snapshot_records()?;
        let record = find(&records, selector)
            .ok_or_else(|| self.snapshot_not_found(selector))?
            .clone();
        let snapshot = view(&record, &records);
        if !snapshot.children.is_empty() {
            return Err(Error::new(
                Reason::SnapshotInUse,
                format!(
                    "Can't delete snapshot {}. {} depend on it: {}.",
                    record.id,
                    plural(snapshot.children.len(), "snapshot", "snapshots"),
                    listing(&snapshot.children)
                ),
            )
            .with_next(
                "See what depends on it",
                self.next(&["beavy", "snapshots", "inspect", &record.id]),
            ));
        }

        let worktrees: Vec<String> = self
            .worktree_records()?
            .into_iter()
            .filter(|worktree| worktree.snapshot == record.id)
            .map(|worktree| worktree.id)
            .collect();
        if !worktrees.is_empty() {
            return Err(Error::new(
                Reason::SnapshotInUse,
                format!(
                    "Can't delete snapshot {}. {} use it: {}.",
                    record.id,
                    plural(worktrees.len(), "worktree", "worktrees"),
                    listing(&worktrees)
                ),
            )
            .with_next(
                "Delete a worktree",
                self.next(&["beavy", "worktrees", "delete", &worktrees[0]]),
            ));
        }

        let path = self.join(SNAPSHOTS).join(format!("{}.json", record.id));
        self.move_to_trash(
            &guard,
            Kind::Snapshot,
            &record.id,
            record.name.as_deref(),
            &path,
        )?;
        Ok(Deleted { snapshot, warnings })
    }

    /// Snapshot records, newest first; none when the store doesn't exist.
    pub(crate) fn read_snapshots(&self) -> Result<Vec<SnapshotRecord>, Error> {
        if !self.exists()? {
            return Ok(Vec::new());
        }
        self.snapshot_records()
    }

    pub(crate) fn snapshot_records(&self) -> Result<Vec<SnapshotRecord>, Error> {
        let dir = self.join(SNAPSHOTS);
        let mut records = Vec::new();
        for name in list_dir(&dir)? {
            let is_record = name
                .to_str()
                .is_some_and(|name| name.ends_with(".json") && !name.starts_with('.'));
            if !is_record {
                continue;
            }
            let path = dir.join(&name);
            if let Some(record) = read_record::<SnapshotRecord>(&path)? {
                let id = name.to_str().and_then(|name| name.strip_suffix(".json"));
                check(&record, &path, id.unwrap_or_default())?;
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

    pub(crate) fn ensure_snapshot_name_free(&self, name: &str) -> Result<(), Error> {
        let records = self.snapshot_records()?;
        match records.iter().find(|r| r.name.as_deref() == Some(name)) {
            None => Ok(()),
            Some(taken) => Err(Error::new(
                Reason::NameTaken,
                format!(
                    "Can't name the snapshot {name}. Snapshot {} already has that name.",
                    taken.id
                ),
            )),
        }
    }

    pub(crate) fn snapshot_not_found(&self, selector: &str) -> Error {
        Error::new(
            Reason::NotFound,
            format!(
                "Couldn't find snapshot {selector} in store {}.",
                self.path().display()
            ),
        )
        .with_next(
            "See the snapshots",
            self.next(&["beavy", "snapshots", "list"]),
        )
    }
}

/// Check a snapshot record read from `path` before any of it becomes part of a
/// path: its ID must be `id`, the name it was stored under.
pub(crate) fn check(record: &SnapshotRecord, path: &Path, id: &str) -> Result<(), Error> {
    if !is_id("snap", &record.id) || record.id != id {
        return Err(damaged(path, format!("its ID {:?} isn't {id}", record.id)));
    }
    if record.layers.is_empty() || !record.layers.iter().all(is_random_id) {
        return Err(damaged(path, "it names a layer that isn't one"));
    }
    if record
        .parent
        .as_deref()
        .is_some_and(|parent| !is_id("snap", parent))
    {
        return Err(damaged(path, "its parent isn't a snapshot ID"));
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

pub(crate) fn find<'a>(
    records: &'a [SnapshotRecord],
    selector: &str,
) -> Option<&'a SnapshotRecord> {
    records
        .iter()
        .find(|record| record.id == selector)
        .or_else(|| {
            records
                .iter()
                .find(|record| record.name.as_deref() == Some(selector))
        })
}

pub(crate) fn view(record: &SnapshotRecord, records: &[SnapshotRecord]) -> Snapshot {
    Snapshot {
        id: record.id.clone(),
        name: record.name.clone(),
        parent: record.parent.clone(),
        source: record.source.clone(),
        commit: record.commit.clone(),
        method: record.method,
        worktree: record.worktree.clone(),
        depth: record.layers.len(),
        created_at: record.created_at.clone(),
        children: records
            .iter()
            .filter(|other| other.parent.as_deref() == Some(record.id.as_str()))
            .map(|other| other.id.clone())
            .collect(),
    }
}

pub(crate) fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// At most 5 items, then "and N more".
pub(crate) fn listing(items: &[String]) -> String {
    let mut text = items.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
    if items.len() > 5 {
        text.push_str(&format!(", and {} more", items.len() - 5));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listings_and_plurals_read_naturally() {
        assert_eq!(plural(1, "entry", "entries"), "1 entry");
        assert_eq!(plural(3, "entry", "entries"), "3 entries");
        let items: Vec<String> = (1..=7).map(|i| format!("s{i}")).collect();
        assert_eq!(listing(&items), "s1, s2, s3, s4, s5, and 2 more");
    }
}
