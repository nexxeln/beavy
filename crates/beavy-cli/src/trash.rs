use std::io::{self, Write};

use beavy::output::{Format, Output};
use beavy::store::{Kind, TrashEntry};
use beavy::{Document, Error, Reason};
use serde::Serialize;

use crate::store::{self, Opened, StoreView};
use crate::{Failure, Global, StoreArgs, TrashCommand, format};

pub fn run<O: Write, E: Write>(
    output: &mut Output<O, E>,
    global: &Global,
    args: &StoreArgs,
    command: &TrashCommand,
) -> Result<(), Failure> {
    let opened = store::open(global, args)?;
    match command {
        TrashCommand::List => list(output, &opened),
        TrashCommand::Restore { id } => restore(output, &opened, id),
        TrashCommand::Empty { yes } => empty(output, &opened, *yes),
    }
}

#[derive(Serialize)]
struct Listed<'a> {
    entries: &'a [TrashEntry],
    store: StoreView,
}

fn list<O: Write, E: Write>(output: &mut Output<O, E>, opened: &Opened) -> Result<(), Failure> {
    let entries = opened.store.trash()?;
    if output.mode().format == Format::Json {
        output.json(&Document::new(Listed {
            entries: &entries,
            store: opened.view(),
        }))?;
        return Ok(());
    }
    opened.context(output)?;
    let terminal = output.mode().stdout.terminal;
    let header = ["ID", "Kind", "Name", "Deleted"];
    if entries.is_empty() {
        output.stderr().line("The trash is empty.")?;
        if !terminal {
            output.stdout().table(&header, &[])?;
        }
        return Ok(());
    }
    let rows: Vec<Vec<String>> = entries
        .iter()
        .map(|entry| {
            vec![
                entry.id.clone(),
                entry.kind.as_str().to_string(),
                entry.name.clone().unwrap_or_else(|| "-".into()),
                format::when(&entry.deleted_at, terminal),
            ]
        })
        .collect();
    output.stdout().table(&header, &rows)?;
    Ok(())
}

#[derive(Serialize)]
struct Restored<'a> {
    entry: &'a TrashEntry,
    outcome: &'static str,
    store: StoreView,
}

fn restore<O: Write, E: Write>(
    output: &mut Output<O, E>,
    opened: &Opened,
    id: &str,
) -> Result<(), Failure> {
    let restored = opened.store.restore(id)?;
    let entry = &restored.entry;
    let inspect = match entry.kind {
        Kind::Snapshot => opened
            .store
            .next(&["beavy", "snapshots", "inspect", &entry.id]),
        Kind::Worktree => opened
            .store
            .next(&["beavy", "worktrees", "inspect", &entry.id]),
    };
    if output.mode().format == Format::Json {
        let document = Document::new(Restored {
            entry,
            outcome: "restored",
            store: opened.view(),
        })
        .with_warnings(restored.warnings.clone())
        .with_next(inspect);
        output.json(&document)?;
        return Ok(());
    }
    output.stderr().receipt("Restored", &entry.id)?;
    output.warnings(&restored.warnings)?;
    output.identifier(&entry.id)?;
    Ok(())
}

#[derive(Serialize)]
struct Emptied<'a> {
    entries: &'a [TrashEntry],
    outcome: &'static str,
    store: StoreView,
}

/// What emptying the trash destroys, such as `2 snapshots and 1 worktree`.
fn contents(entries: &[TrashEntry]) -> String {
    let count = |kind| entries.iter().filter(|e| e.kind == kind).count();
    let parts: Vec<String> = [
        (count(Kind::Snapshot), "snapshot", "snapshots"),
        (count(Kind::Worktree), "worktree", "worktrees"),
    ]
    .into_iter()
    .filter(|(n, _, _)| *n > 0)
    .map(|(n, one, many)| format::plural(n, one, many))
    .collect();
    if parts.is_empty() {
        "nothing".into()
    } else {
        parts.join(" and ")
    }
}

fn empty<O: Write, E: Write>(
    output: &mut Output<O, E>,
    opened: &Opened,
    yes: bool,
) -> Result<(), Failure> {
    let entries = opened.store.trash()?;
    if entries.is_empty() {
        if output.mode().format == Format::Json {
            output.json(&Document::new(Emptied {
                entries: &[],
                outcome: "unchanged",
                store: opened.view(),
            }))?;
        } else {
            output.stderr().line("The trash is already empty.")?;
        }
        return Ok(());
    }
    let contents = contents(&entries);
    if !yes {
        if !output.mode().interactive {
            let next = opened
                .store
                .next(&["beavy", "trash", "empty", "--yes"])
                .when(format!(
                    "Only after a human approves: deleting {contents} for good"
                ));
            return Err(Error::new(
                Reason::ConfirmationRequired,
                format!("Can't empty the trash without confirmation in non-interactive mode. It holds {contents}."),
            )
            .with_next("Approve and run", next)
            .into());
        }
        opened.context(output)?;
        {
            let mut stderr = output.stderr();
            stderr.line(&format!(
                "Deletes {contents} for good, with the data only they use."
            ))?;
            stderr.line("This cannot be undone.")?;
        }
        if !output.confirm("Empty the trash?", &mut io::stdin().lock())? {
            return Err(Error::new(Reason::Canceled, "Canceled. Nothing was deleted.").into());
        }
    }
    let emptied = opened.store.empty_trash()?;
    if output.mode().format == Format::Json {
        let document = Document::new(Emptied {
            entries: &emptied.entries,
            outcome: "emptied",
            store: opened.view(),
        })
        .with_warnings(emptied.warnings.clone());
        output.json(&document)?;
        return Ok(());
    }
    let mut stderr = output.stderr();
    stderr.receipt("Emptied", "the trash")?;
    stderr.row("Deleted", &self::contents(&emptied.entries))?;
    output.warnings(&emptied.warnings)?;
    Ok(())
}
