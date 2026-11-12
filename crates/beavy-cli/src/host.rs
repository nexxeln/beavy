use std::io::Write;
use std::path::Path;

use beavy::Document;
use beavy::host::Report;
use beavy::output::{Format, Output};
use serde::Serialize;

use crate::Failure;

pub const CHECK_HELP: &str = "\
Examples:
  Check the current directory
    beavy host check

  Check a directory and read the result as JSON
    beavy host check --cwd <DIR> --json

Notes:
  Each probe runs in a temporary directory inside the checked directory, and
  the command removes it before exiting. No probe needs root. A status is
  unknown when the evidence doesn't settle it.

Learn more: https://github.com/nexxeln/beavy/blob/main/docs/cli-contract.md#beavy-host-check";

#[derive(Serialize)]
struct HostResult<'a> {
    host: &'a Report,
}

pub fn check<O: Write, E: Write>(
    output: &mut Output<O, E>,
    directory: &Path,
) -> Result<(), Failure> {
    let report = beavy::host::check(directory)?;
    if output.mode().format == Format::Json {
        let document =
            Document::new(HostResult { host: &report }).with_warnings(report.warnings.clone());
        output.json(&document)?;
        return Ok(());
    }

    let unknown = || "unknown".to_string();
    let os = &report.os;
    let mut stdout = output.stdout();
    stdout.row(
        "OS",
        &format!(
            "{} {} ({})",
            os.name,
            os.version.clone().unwrap_or_else(unknown),
            os.arch
        ),
    )?;
    stdout.row("Directory", &report.directory.path)?;
    stdout.row(
        "Filesystem",
        &report.directory.filesystem.clone().unwrap_or_else(unknown),
    )?;
    stdout.row("Root", if report.root { "yes" } else { "no" })?;
    stdout.row(
        "User Namespaces",
        report.checks.user_namespaces.status.as_str(),
    )?;
    stdout.blank()?;
    let rows: Vec<Vec<String>> = report
        .backends
        .iter()
        .map(|backend| {
            vec![
                backend.name.as_str().to_string(),
                backend.status.as_str().to_string(),
                backend.reason.clone(),
            ]
        })
        .collect();
    stdout.table(&["Backend", "Status", "Reason"], &rows)?;
    output.warnings(&report.warnings)?;
    Ok(())
}
