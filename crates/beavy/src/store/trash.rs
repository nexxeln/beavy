//! The trash. Deleting a snapshot moves it here with one rename, so deletion is
//! instant whatever the size of the data, and restorable until the trash is
//! emptied. Layers stay in layers/ while any snapshot, live or trashed, uses them.

use std::fs;
use std::io::ErrorKind;

use serde::{Deserialize, Serialize};

use super::snapshot::SnapshotRecord;
use super::{FORMAT, SNAPSHOTS, Store, TRASH, fs_error, list_dir, read_record, write_record};
use crate::envelope::Warning;
use crate::error::{Error, Reason};
use crate::time;

const ENTRY_FILE: &str = "entry.json";
pub(crate) const SNAPSHOT_FILE: &str = "snapshot.json";

/// What a trash entry holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Snapshot,
}

impl Kind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Kind::Snapshot => "snapshot",
        }
    }

    const fn payload(self) -> &'static str {
        match self {
            Kind::Snapshot => SNAPSHOT_FILE,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EntryRecord {
    format: u32,
    kind: Kind,
    id: String,
    name: Option<String>,
    deleted_at: String,
}

/// A deleted snapshot that can still be restored.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrashEntry {
    /// The ID it had, and has again once restored.
    pub id: String,
    pub kind: Kind,
    pub name: Option<String>,
    pub deleted_at: String,
}

impl From<EntryRecord> for TrashEntry {
    fn from(record: EntryRecord) -> Self {
        TrashEntry {
            id: record.id,
            kind: record.kind,
            name: record.name,
            deleted_at: record.deleted_at,
        }
    }
}

#[derive(Debug)]
pub struct Emptied {
    pub entries: Vec<TrashEntry>,
    pub warnings: Vec<Warning>,
}

#[derive(Debug)]
pub struct Restored {
    pub entry: TrashEntry,
    pub warnings: Vec<Warning>,
}

impl Store {
    /// Everything in the trash, most recently deleted first.
    pub fn trash(&self) -> Result<Vec<TrashEntry>, Error> {
        if !self.exists()? {
            return Ok(Vec::new());
        }
        Ok(self
            .trash_records()?
            .into_iter()
            .map(TrashEntry::from)
            .collect())
    }

    /// Move a snapshot into the trash. `payload` is the record to move. Needs the
    /// store lock.
    pub(crate) fn move_to_trash(
        &self,
        _guard: &super::Guard,
        kind: Kind,
        id: &str,
        name: Option<&str>,
        payload: &std::path::Path,
    ) -> Result<(), Error> {
        let entry = self.join(TRASH).join(id);
        match fs::create_dir(&entry) {
            Err(err) if err.kind() != ErrorKind::AlreadyExists => {
                return Err(fs_error(err, format!("create {}", entry.display())));
            }
            _ => {}
        }
        let record = EntryRecord {
            format: FORMAT,
            kind,
            id: id.to_owned(),
            name: name.map(str::to_owned),
            deleted_at: time::now(),
        };
        write_record(&entry, ENTRY_FILE, &record)?;
        super::crash_point("trash-move");
        let target = entry.join(kind.payload());
        fs::rename(payload, &target)
            .map_err(|err| fs_error(err, format!("move {} to the trash", payload.display())))
    }

    /// Put a trashed snapshot back.
    pub fn restore(&self, id: &str) -> Result<Restored, Error> {
        if !self.exists()? {
            return Err(self.trash_not_found(id));
        }
        let (_guard, warnings) = self.open_for_change()?;
        let record = self
            .trash_records()?
            .into_iter()
            .find(|record| record.id == id)
            .ok_or_else(|| self.trash_not_found(id))?;
        let entry = self.join(TRASH).join(&record.id);
        let payload = entry.join(record.kind.payload());
        match record.kind {
            Kind::Snapshot => {
                if let Some(name) = &record.name {
                    let live = self.snapshot_records()?;
                    if let Some(taken) = live.iter().find(|r| r.name.as_ref() == Some(name)) {
                        return Err(Error::new(
                            Reason::NameTaken,
                            format!(
                                "Can't restore snapshot {}. Snapshot {} has its name {name} now.",
                                record.id, taken.id
                            ),
                        ));
                    }
                }
                let target = self.join(SNAPSHOTS).join(format!("{}.json", record.id));
                fs::rename(&payload, &target)
                    .map_err(|err| fs_error(err, format!("restore {}", target.display())))?;
            }
        }
        let _ = super::tree::remove(&entry);
        Ok(Restored {
            entry: record.into(),
            warnings,
        })
    }

    /// Delete everything in the trash for good, and every layer that no live
    /// snapshot uses.
    pub fn empty_trash(&self) -> Result<Emptied, Error> {
        if !self.exists()? {
            return Ok(Emptied {
                entries: Vec::new(),
                warnings: Vec::new(),
            });
        }
        let (guard, mut warnings) = self.open_for_change()?;
        let records = self.trash_records()?;
        let scratch = self.scratch(&guard)?;
        for record in &records {
            let from = self.join(TRASH).join(&record.id);
            fs::rename(&from, scratch.path().join(&record.id))
                .map_err(|err| fs_error(err, format!("move {}", from.display())))?;
        }
        let trash = self.trash_unused_layers(&guard)?;
        drop(guard);
        warnings.extend(trash.and_then(|trash| trash.discard()));
        warnings.extend(scratch.discard());
        Ok(Emptied {
            entries: records.into_iter().map(TrashEntry::from).collect(),
            warnings,
        })
    }

    /// Snapshot records in the trash. They keep their layers alive.
    pub(crate) fn trashed_snapshots(&self) -> Result<Vec<SnapshotRecord>, Error> {
        let mut records = Vec::new();
        for entry in list_dir(&self.join(TRASH))? {
            let path = self.join(TRASH).join(&entry).join(SNAPSHOT_FILE);
            if let Some(record) = read_record::<SnapshotRecord>(&path)? {
                records.push(record);
            }
        }
        Ok(records)
    }

    /// Trash entries with their payload, newest first. Entries without one were
    /// left by an interrupted delete or restore.
    fn trash_records(&self) -> Result<Vec<EntryRecord>, Error> {
        let mut records = Vec::new();
        for entry in list_dir(&self.join(TRASH))? {
            let dir = self.join(TRASH).join(&entry);
            let Some(record) = read_record::<EntryRecord>(&dir.join(ENTRY_FILE))? else {
                continue;
            };
            if dir.join(record.kind.payload()).exists() {
                records.push(record);
            }
        }
        records.sort_by(|a, b| {
            b.deleted_at
                .cmp(&a.deleted_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(records)
    }

    /// Remove trash entries an interrupted delete or restore left without a payload.
    pub(crate) fn repair_trash(&self) -> Result<Vec<Warning>, Error> {
        let mut warnings = Vec::new();
        for entry in list_dir(&self.join(TRASH))? {
            let dir = self.join(TRASH).join(&entry);
            let complete = read_record::<EntryRecord>(&dir.join(ENTRY_FILE))
                .ok()
                .flatten()
                .is_some_and(|record| dir.join(record.kind.payload()).exists());
            if !complete && let Err(err) = super::tree::remove(&dir) {
                warnings.push(super::cleanup_warning(&dir, &err));
            }
        }
        Ok(warnings)
    }

    fn trash_not_found(&self, id: &str) -> Error {
        Error::new(
            Reason::NotFound,
            format!(
                "Couldn't find {id} in the trash of store {}.",
                self.path().display()
            ),
        )
        .with_next("See the trash", self.next(&["beavy", "trash", "list"]))
    }
}
