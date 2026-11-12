use std::ffi::OsString;
use std::io::Write;
use std::path::Path;

use beavy::Document;
use beavy::output::{Format, Output};
use beavy::store::{Backend, Worktree};
use serde::Serialize;

use crate::store::{self, Opened, StoreView};
use crate::{Failure, Global, StoreArgs, WorktreesCommand, format, working_directory};

pub fn run<O: Write, E: Write>(
    output: &mut Output<O, E>,
    global: &Global,
    args: &StoreArgs,
    command: &WorktreesCommand,
) -> Result<(), Failure> {
    let opened = store::open(global, args)?;
    match command {
        WorktreesCommand::Create { snapshot, name } => {
            create(output, &opened, snapshot, name.as_deref())
        }
        WorktreesCommand::List => list(output, &opened),
        WorktreesCommand::Inspect { worktree } => inspect(output, &opened, worktree),
        WorktreesCommand::Run { worktree, command } => {
            exec(output, &opened, global, worktree, command)
        }
        WorktreesCommand::Mount { worktree, dir } => mount(output, &opened, worktree, dir),
        WorktreesCommand::Unmount { worktree } => unmount(output, &opened, worktree),
        WorktreesCommand::Delete { worktree } => delete(output, &opened, worktree),
    }
}

/// `overlayfs (userns)`.
fn backend_text(backend: Backend) -> String {
    format!("{} ({})", backend.name, backend.privilege.as_str())
}

fn none_or(items: &[String]) -> String {
    if items.is_empty() {
        "-".into()
    } else {
        items.join(", ")
    }
}

#[derive(Serialize)]
struct Changed<'a> {
    worktree: &'a Worktree,
    outcome: &'static str,
    backend: Backend,
    store: StoreView,
}

fn create<O: Write, E: Write>(
    output: &mut Output<O, E>,
    opened: &Opened,
    snapshot: &str,
    name: Option<&str>,
) -> Result<(), Failure> {
    let created = opened.store.create_worktree(snapshot, name)?;
    let worktree = &created.worktree;
    let run = opened
        .store
        .next(&["beavy", "worktrees", "run", &worktree.id, "--", "<COMMAND>"]);
    if output.mode().format == Format::Json {
        let document = Document::new(Changed {
            worktree,
            outcome: "created",
            backend: created.backend,
            store: opened.view(),
        })
        .with_warnings(created.warnings.clone())
        .with_next(run);
        output.json(&document)?;
        return Ok(());
    }
    let mut stderr = output.stderr();
    stderr.receipt("Created", &worktree.id)?;
    if let Some(name) = &worktree.name {
        stderr.row("Name", name)?;
    }
    stderr.row("Snapshot", &worktree.snapshot)?;
    stderr.row("Path", &worktree.path)?;
    stderr.row("Backend", &backend_text(created.backend))?;
    stderr.row("Store", &opened.path().to_string_lossy())?;
    stderr.next("Run a command in it", &run.command)?;
    output.warnings(&created.warnings)?;
    output.identifier(&worktree.id)?;
    Ok(())
}

#[derive(Serialize)]
struct Listed<'a> {
    worktrees: &'a [Worktree],
    backend: Backend,
    store: StoreView,
}

fn list<O: Write, E: Write>(output: &mut Output<O, E>, opened: &Opened) -> Result<(), Failure> {
    let worktrees = opened.store.worktrees()?;
    let backend = Backend::detect();
    if output.mode().format == Format::Json {
        output.json(&Document::new(Listed {
            worktrees: &worktrees,
            backend,
            store: opened.view(),
        }))?;
        return Ok(());
    }
    opened.context(output)?;
    let terminal = output.mode().stdout.terminal;
    let header = ["ID", "Name", "Snapshot", "Mount", "Runs", "Created"];
    if worktrees.is_empty() {
        let mut stderr = output.stderr();
        stderr.line("No worktrees in this store.")?;
        let next = opened
            .store
            .next(&["beavy", "worktrees", "create", "<SNAPSHOT>"]);
        stderr.next("Create one from a snapshot", &next.command)?;
        if !terminal {
            output.stdout().table(&header, &[])?;
        }
        return Ok(());
    }
    let rows: Vec<Vec<String>> = worktrees
        .iter()
        .map(|worktree| {
            vec![
                worktree.id.clone(),
                worktree.name.clone().unwrap_or_else(|| "-".into()),
                worktree.snapshot.clone(),
                none_or(&worktree.mounts),
                worktree.runs.len().to_string(),
                format::when(&worktree.created_at, terminal),
            ]
        })
        .collect();
    output.stdout().table(&header, &rows)?;
    Ok(())
}

#[derive(Serialize)]
struct Inspected<'a> {
    worktree: &'a Worktree,
    backend: Backend,
    store: StoreView,
}

fn inspect<O: Write, E: Write>(
    output: &mut Output<O, E>,
    opened: &Opened,
    selector: &str,
) -> Result<(), Failure> {
    let worktree = opened.store.worktree(selector)?;
    let backend = Backend::detect();
    if output.mode().format == Format::Json {
        output.json(&Document::new(Inspected {
            worktree: &worktree,
            backend,
            store: opened.view(),
        }))?;
        return Ok(());
    }
    let runs: Vec<String> = worktree.runs.iter().map(u32::to_string).collect();
    let mut stdout = output.stdout();
    stdout.row("ID", &worktree.id)?;
    stdout.row("Name", worktree.name.as_deref().unwrap_or("-"))?;
    stdout.row("Snapshot", &worktree.snapshot)?;
    stdout.row("Path", &worktree.path)?;
    stdout.row("Mounts", &none_or(&worktree.mounts))?;
    stdout.row("Runs", &none_or(&runs))?;
    stdout.row("Created", &format::detail_time(&worktree.created_at))?;
    stdout.row("Backend", &backend_text(backend))?;
    Ok(())
}

/// Replace beavy with the command, inside the worktree. Returns only on failure.
fn exec<O: Write, E: Write>(
    output: &mut Output<O, E>,
    opened: &Opened,
    global: &Global,
    selector: &str,
    command: &[OsString],
) -> Result<(), Failure> {
    let cwd = working_directory(global)?;
    let (program, args) = command
        .split_first()
        .expect("clap requires at least one COMMAND value");
    let prepared = opened.store.prepare_run(selector, &cwd, program, args)?;
    if output.mode().format == Format::Human {
        output.warnings(&prepared.warnings)?;
    }
    Err(prepared.exec().into())
}

fn mount<O: Write, E: Write>(
    output: &mut Output<O, E>,
    opened: &Opened,
    selector: &str,
    dir: &Path,
) -> Result<(), Failure> {
    let mounted = opened.store.mount_worktree(selector, dir)?;
    let worktree = &mounted.worktree;
    let (outcome, verb) = if mounted.changed {
        ("mounted", "Mounted")
    } else {
        ("unchanged", "Already mounted")
    };
    if output.mode().format == Format::Json {
        let document = Document::new(Changed {
            worktree,
            outcome,
            backend: Backend::detect(),
            store: opened.view(),
        })
        .with_warnings(mounted.warnings.clone())
        .with_next(
            opened
                .store
                .next(&["beavy", "worktrees", "unmount", &worktree.id]),
        );
        output.json(&document)?;
        return Ok(());
    }
    let mut stderr = output.stderr();
    stderr.receipt(verb, &worktree.id)?;
    stderr.row("Mount", &none_or(&worktree.mounts))?;
    output.warnings(&mounted.warnings)?;
    output.identifier(&worktree.id)?;
    Ok(())
}

fn unmount<O: Write, E: Write>(
    output: &mut Output<O, E>,
    opened: &Opened,
    selector: &str,
) -> Result<(), Failure> {
    let unmounted = opened.store.unmount_worktree(selector)?;
    let worktree = &unmounted.worktree;
    let (outcome, verb) = if unmounted.changed {
        ("unmounted", "Unmounted")
    } else {
        ("unchanged", "Not mounted")
    };
    if output.mode().format == Format::Json {
        let document = Document::new(Changed {
            worktree,
            outcome,
            backend: Backend::detect(),
            store: opened.view(),
        })
        .with_warnings(unmounted.warnings.clone());
        output.json(&document)?;
        return Ok(());
    }
    output.stderr().receipt(verb, &worktree.id)?;
    output.warnings(&unmounted.warnings)?;
    output.identifier(&worktree.id)?;
    Ok(())
}

#[derive(Serialize)]
struct Deleted<'a> {
    worktree: &'a Worktree,
    outcome: &'static str,
    store: StoreView,
}

fn delete<O: Write, E: Write>(
    output: &mut Output<O, E>,
    opened: &Opened,
    selector: &str,
) -> Result<(), Failure> {
    let deleted = opened.store.delete_worktree(selector)?;
    let worktree = &deleted.worktree;
    let restore = opened
        .store
        .next(&["beavy", "trash", "restore", &worktree.id]);
    if output.mode().format == Format::Json {
        let document = Document::new(Deleted {
            worktree,
            outcome: "deleted",
            store: opened.view(),
        })
        .with_warnings(deleted.warnings.clone())
        .with_next(restore);
        output.json(&document)?;
        return Ok(());
    }
    let mut stderr = output.stderr();
    stderr.receipt("Deleted", &worktree.id)?;
    stderr.line("It's in the trash until you empty it.")?;
    stderr.next("Restore it", &restore.command)?;
    output.warnings(&deleted.warnings)?;
    output.identifier(&worktree.id)?;
    Ok(())
}
