use std::io::Write;

use beavy::Document;
use beavy::output::{Format, Output};
use beavy::store::{Snapshot, Worktree};
use serde::Serialize;

use crate::store::{self, Opened, StoreView};
use crate::{Failure, Global, SnapshotsCommand, StoreArgs, format};

pub fn run<O: Write, E: Write>(
    output: &mut Output<O, E>,
    global: &Global,
    args: &StoreArgs,
    command: &SnapshotsCommand,
) -> Result<(), Failure> {
    let opened = store::open(global, args)?;
    match command {
        SnapshotsCommand::Create {
            name,
            worktree: Some(worktree),
        } => seal(output, &opened, worktree, name.as_deref()),
        SnapshotsCommand::Create {
            name,
            worktree: None,
        } => create(output, &opened, name.as_deref()),
        SnapshotsCommand::List => list(output, &opened),
        SnapshotsCommand::Inspect { snapshot } => inspect(output, &opened, snapshot),
        SnapshotsCommand::Delete { snapshot } => delete(output, &opened, snapshot),
    }
}

#[derive(Serialize)]
struct Import {
    files: u64,
    bytes: u64,
}

#[derive(Serialize)]
struct Created<'a> {
    snapshot: &'a Snapshot,
    outcome: &'static str,
    import: Import,
    store: StoreView,
}

fn create<O: Write, E: Write>(
    output: &mut Output<O, E>,
    opened: &Opened,
    name: Option<&str>,
) -> Result<(), Failure> {
    let source = &opened.location.project;
    output
        .stderr()
        .line(&format!("Importing {}…", source.display()))?;
    let created = opened.store.create_snapshot(source, name)?;
    let snapshot = &created.snapshot;
    if output.mode().format == Format::Json {
        let document = Document::new(Created {
            snapshot,
            outcome: "created",
            import: Import {
                files: created.files,
                bytes: created.bytes,
            },
            store: opened.view(),
        })
        .with_warnings(created.warnings.clone())
        .with_next(
            opened
                .store
                .next(&["beavy", "snapshots", "inspect", &snapshot.id]),
        );
        output.json(&document)?;
        return Ok(());
    }
    let mut stderr = output.stderr();
    stderr.receipt("Created", &snapshot.id)?;
    if let Some(name) = &snapshot.name {
        stderr.row("Name", name)?;
    }
    stderr.row("Source", &snapshot.source)?;
    if let Some(commit) = &snapshot.commit {
        stderr.row("Commit", commit)?;
    }
    stderr.row("Method", snapshot.method.as_str())?;
    stderr.row(
        "Files",
        &format!(
            "{} ({})",
            format::count(created.files),
            format::bytes(created.bytes)
        ),
    )?;
    stderr.row("Store", &opened.path().to_string_lossy())?;
    output.warnings(&created.warnings)?;
    output.identifier(&snapshot.id)?;
    Ok(())
}

#[derive(Serialize)]
struct Sealed<'a> {
    snapshot: &'a Snapshot,
    outcome: &'static str,
    worktree: &'a Worktree,
    squashed: bool,
    store: StoreView,
}

fn seal<O: Write, E: Write>(
    output: &mut Output<O, E>,
    opened: &Opened,
    worktree: &str,
    name: Option<&str>,
) -> Result<(), Failure> {
    let sealed = opened.store.create_snapshot_from_worktree(worktree, name)?;
    let snapshot = &sealed.snapshot;
    let create = opened
        .store
        .next(&["beavy", "worktrees", "create", &snapshot.id]);
    if output.mode().format == Format::Json {
        let document = Document::new(Sealed {
            snapshot,
            outcome: "created",
            worktree: &sealed.worktree,
            squashed: sealed.squashed,
            store: opened.view(),
        })
        .with_warnings(sealed.warnings.clone())
        .with_next(create);
        output.json(&document)?;
        return Ok(());
    }
    let mut stderr = output.stderr();
    stderr.receipt("Created", &snapshot.id)?;
    if let Some(name) = &snapshot.name {
        stderr.row("Name", name)?;
    }
    stderr.row("Worktree", &sealed.worktree.id)?;
    stderr.row("Parent", snapshot.parent.as_deref().unwrap_or("-"))?;
    if let Some(commit) = &snapshot.commit {
        stderr.row("Commit", commit)?;
    }
    let depth = if sealed.squashed {
        format!("{} (squashed)", snapshot.depth)
    } else {
        snapshot.depth.to_string()
    };
    stderr.row("Depth", &depth)?;
    stderr.next("Start another worktree from it", &create.command)?;
    output.warnings(&sealed.warnings)?;
    output.identifier(&snapshot.id)?;
    Ok(())
}

#[derive(Serialize)]
struct Listed<'a> {
    snapshots: &'a [Snapshot],
    store: StoreView,
}

fn list<O: Write, E: Write>(output: &mut Output<O, E>, opened: &Opened) -> Result<(), Failure> {
    let snapshots = opened.store.snapshots()?;
    if output.mode().format == Format::Json {
        output.json(&Document::new(Listed {
            snapshots: &snapshots,
            store: opened.view(),
        }))?;
        return Ok(());
    }
    opened.context(output)?;
    let terminal = output.mode().stdout.terminal;
    let header = ["ID", "Name", "Parent", "Depth", "Created"];
    if snapshots.is_empty() {
        let mut stderr = output.stderr();
        stderr.line("No snapshots in this store.")?;
        let next = opened.store.next(&["beavy", "snapshots", "create"]);
        stderr.next("Create one from the project", &next.command)?;
        if !terminal {
            output.stdout().table(&header, &[])?;
        }
        return Ok(());
    }
    let rows: Vec<Vec<String>> = snapshots
        .iter()
        .map(|snapshot| {
            vec![
                snapshot.id.clone(),
                snapshot.name.clone().unwrap_or_else(|| "-".into()),
                snapshot.parent.clone().unwrap_or_else(|| "-".into()),
                snapshot.depth.to_string(),
                format::when(&snapshot.created_at, terminal),
            ]
        })
        .collect();
    output.stdout().table(&header, &rows)?;
    Ok(())
}

#[derive(Serialize)]
struct Inspected<'a> {
    snapshot: &'a Snapshot,
    store: StoreView,
}

fn inspect<O: Write, E: Write>(
    output: &mut Output<O, E>,
    opened: &Opened,
    selector: &str,
) -> Result<(), Failure> {
    let snapshot = opened.store.snapshot(selector)?;
    if output.mode().format == Format::Json {
        output.json(&Document::new(Inspected {
            snapshot: &snapshot,
            store: opened.view(),
        }))?;
        return Ok(());
    }
    let none = || "-".to_string();
    let mut stdout = output.stdout();
    stdout.row("ID", &snapshot.id)?;
    stdout.row("Name", &snapshot.name.clone().unwrap_or_else(none))?;
    stdout.row("Parent", &snapshot.parent.clone().unwrap_or_else(none))?;
    stdout.row("Source", &snapshot.source)?;
    stdout.row("Commit", &snapshot.commit.clone().unwrap_or_else(none))?;
    stdout.row("Method", snapshot.method.as_str())?;
    stdout.row("Worktree", snapshot.worktree.as_deref().unwrap_or("-"))?;
    stdout.row("Depth", &snapshot.depth.to_string())?;
    stdout.row("Created", &format::detail_time(&snapshot.created_at))?;
    let children = if snapshot.children.is_empty() {
        none()
    } else {
        snapshot.children.join(", ")
    };
    stdout.row("Children", &children)?;
    Ok(())
}

#[derive(Serialize)]
struct Deleted<'a> {
    snapshot: &'a Snapshot,
    outcome: &'static str,
    store: StoreView,
}

fn delete<O: Write, E: Write>(
    output: &mut Output<O, E>,
    opened: &Opened,
    selector: &str,
) -> Result<(), Failure> {
    let deleted = opened.store.delete_snapshot(selector)?;
    let snapshot = &deleted.snapshot;
    let restore = opened
        .store
        .next(&["beavy", "trash", "restore", &snapshot.id]);
    if output.mode().format == Format::Json {
        let document = Document::new(Deleted {
            snapshot,
            outcome: "deleted",
            store: opened.view(),
        })
        .with_warnings(deleted.warnings.clone())
        .with_next(opened.store.next(&["beavy", "snapshots", "list"]))
        .with_next(restore);
        output.json(&document)?;
        return Ok(());
    }
    let mut stderr = output.stderr();
    stderr.receipt("Deleted", &snapshot.id)?;
    stderr.line("It's in the trash until you empty it.")?;
    stderr.next("Restore it", &restore.command)?;
    output.warnings(&deleted.warnings)?;
    output.identifier(&snapshot.id)?;
    Ok(())
}
