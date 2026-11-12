//! The `beavy` command: parses arguments, calls the beavy library, and renders the
//! result under the contract in docs/cli-contract.md.

use std::ffi::OsString;
use std::io::{self, ErrorKind};
use std::path::PathBuf;
use std::process::ExitCode;

use beavy::output::{Format, Mode, ModeOptions, Output};
use beavy::{Document, Error, Next, Reason};
use clap::error::{ContextKind, ContextValue, ErrorKind as ClapErrorKind};
use clap::{ArgAction, Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde::Serialize;

#[cfg(target_os = "linux")]
mod format;
mod host;
#[cfg(target_os = "linux")]
mod snapshots;
#[cfg(target_os = "linux")]
mod store;
#[cfg(target_os = "linux")]
mod trash;
#[cfg(target_os = "linux")]
mod worktrees;

const HELP_TEMPLATE: &str = "{usage-heading} {usage}\n{about}\n\n{all-args}{after-help}";
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Root help groups commands by task. Every subcommand that isn't hidden must
/// appear in exactly one group.
const COMMAND_GROUPS: &[(&str, &[&str])] = &[
    ("Worktrees", &["worktrees", "snapshots", "trash"]),
    ("Machine", &["host"]),
];

/// Monorepo tooling for teams that work in parallel
#[derive(Parser)]
#[command(
    name = "beavy",
    disable_version_flag = true,
    disable_help_flag = true,
    disable_help_subcommand = true,
    subcommand_value_name = "COMMAND"
)]
struct Cli {
    #[command(flatten)]
    global: Global,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Args)]
#[command(next_help_heading = "Global Options")]
struct Global {
    /// Working directory for this invocation
    #[arg(long, global = true, value_name = "DIR")]
    cwd: Option<PathBuf>,
    /// Show help for the selected command
    #[arg(short = 'h', long, global = true, action = ArgAction::Help)]
    help: Option<bool>,
    /// Output the result as JSON
    #[arg(long, global = true)]
    json: bool,
    /// Disable color
    #[arg(long, global = true)]
    no_color: bool,
    /// Never prompt; fail with the required inputs instead
    #[arg(long, global = true)]
    non_interactive: bool,
    /// Print the beavy version
    #[arg(short = 'v', long, global = true)]
    version: bool,
}

/// Where snapshots and worktrees live.
#[derive(Args)]
struct StoreArgs {
    /// Store directory (default: one per project, under ~/.local/share/beavy/stores)
    #[arg(long, global = true, value_name = "DIR")]
    store: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Create worktrees of a snapshot and run commands in them
    #[command(help_template = HELP_TEMPLATE, subcommand_value_name = "COMMAND")]
    Worktrees {
        #[command(flatten)]
        store: StoreArgs,
        #[command(subcommand)]
        command: Option<WorktreesCommand>,
    },
    /// Create, inspect, and delete snapshots of a project
    #[command(help_template = HELP_TEMPLATE, subcommand_value_name = "COMMAND")]
    Snapshots {
        #[command(flatten)]
        store: StoreArgs,
        #[command(subcommand)]
        command: Option<SnapshotsCommand>,
    },
    /// Restore deleted snapshots and worktrees, or empty the trash
    #[command(help_template = HELP_TEMPLATE, subcommand_value_name = "COMMAND")]
    Trash {
        #[command(flatten)]
        store: StoreArgs,
        #[command(subcommand)]
        command: Option<TrashCommand>,
    },
    /// Inspect this machine
    #[command(help_template = HELP_TEMPLATE, subcommand_value_name = "COMMAND")]
    Host {
        #[command(subcommand)]
        command: Option<HostCommand>,
    },
    /// Show help for a command
    #[command(hide = true)]
    Help {
        #[arg(value_name = "COMMAND")]
        path: Vec<String>,
    },
}

#[derive(Subcommand)]
enum SnapshotsCommand {
    /// Create a snapshot of the project directory, or of a worktree
    #[command(help_template = HELP_TEMPLATE, after_help = SNAPSHOTS_CREATE_HELP)]
    Create {
        /// Name for the snapshot, unique in the store
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// Seal this worktree into the snapshot instead of importing the project
        #[arg(long, value_name = "WORKTREE")]
        worktree: Option<String>,
    },
    /// List snapshots
    #[command(help_template = HELP_TEMPLATE, after_help = SNAPSHOTS_LIST_HELP)]
    List,
    /// Show a snapshot
    #[command(help_template = HELP_TEMPLATE, after_help = SNAPSHOTS_INSPECT_HELP)]
    Inspect {
        /// Snapshot ID or name
        #[arg(value_name = "SNAPSHOT")]
        snapshot: String,
    },
    /// Move a snapshot to the trash
    #[command(help_template = HELP_TEMPLATE, after_help = SNAPSHOTS_DELETE_HELP)]
    Delete {
        /// Snapshot ID or name
        #[arg(value_name = "SNAPSHOT")]
        snapshot: String,
    },
}

#[derive(Subcommand)]
enum WorktreesCommand {
    /// Create a worktree of a snapshot
    #[command(help_template = HELP_TEMPLATE, after_help = WORKTREES_CREATE_HELP)]
    Create {
        /// Snapshot ID or name
        #[arg(value_name = "SNAPSHOT")]
        snapshot: String,
        /// Name for the worktree, unique in the store
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
    },
    /// List worktrees
    #[command(help_template = HELP_TEMPLATE, after_help = WORKTREES_LIST_HELP)]
    List,
    /// Show a worktree
    #[command(help_template = HELP_TEMPLATE, after_help = WORKTREES_INSPECT_HELP)]
    Inspect {
        /// Worktree ID or name
        #[arg(value_name = "WORKTREE")]
        worktree: String,
    },
    /// Run a command in a worktree
    #[command(help_template = HELP_TEMPLATE, after_help = WORKTREES_RUN_HELP)]
    Run {
        /// Worktree ID or name
        #[arg(value_name = "WORKTREE")]
        worktree: String,
        /// The command and its arguments, after --
        #[arg(
            value_name = "COMMAND",
            required = true,
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        command: Vec<OsString>,
    },
    /// Mount a worktree at a directory for every process (root only)
    #[command(help_template = HELP_TEMPLATE, after_help = WORKTREES_MOUNT_HELP)]
    Mount {
        /// Worktree ID or name
        #[arg(value_name = "WORKTREE")]
        worktree: String,
        /// Directory to mount it on
        #[arg(value_name = "DIR")]
        dir: PathBuf,
    },
    /// Unmount a worktree's persistent mount
    #[command(help_template = HELP_TEMPLATE, after_help = WORKTREES_UNMOUNT_HELP)]
    Unmount {
        /// Worktree ID or name
        #[arg(value_name = "WORKTREE")]
        worktree: String,
    },
    /// Move a worktree to the trash
    #[command(help_template = HELP_TEMPLATE, after_help = WORKTREES_DELETE_HELP)]
    Delete {
        /// Worktree ID or name
        #[arg(value_name = "WORKTREE")]
        worktree: String,
    },
}

#[derive(Subcommand)]
enum TrashCommand {
    /// List deleted snapshots and worktrees
    #[command(help_template = HELP_TEMPLATE, after_help = TRASH_LIST_HELP)]
    List,
    /// Restore a deleted snapshot or worktree
    #[command(help_template = HELP_TEMPLATE, after_help = TRASH_RESTORE_HELP)]
    Restore {
        /// ID of the deleted snapshot or worktree
        #[arg(value_name = "ID")]
        id: String,
    },
    /// Delete everything in the trash for good
    #[command(help_template = HELP_TEMPLATE, after_help = TRASH_EMPTY_HELP)]
    Empty {
        /// Confirm deleting the trash's contents
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

const SNAPSHOTS_CREATE_HELP: &str = "\
Examples:
  Snapshot the current project
    beavy snapshots create

  Snapshot a project by name and read the result as JSON
    beavy snapshots create --cwd <DIR> --name <NAME> --json

  Seal a worktree's changes into a snapshot
    beavy snapshots create --worktree <WORKTREE>

Notes:
  The project is the nearest directory above --cwd that has a store, else the
  root of its Git working tree, else --cwd itself. The command copies it into
  the store once, cloning files when the filesystem supports reflinks, and
  never changes it. In the copy, Git's HEAD is detached at the current commit.
  It refuses linked worktrees and repositories in the middle of a merge,
  rebase, cherry-pick, revert, or bisect.

  With --worktree, the worktree's changes become the new snapshot's top layer
  without a copy, and the worktree carries on from the new snapshot. Past a
  depth of 8 layers, the layers above the base are merged into one.

Learn more: https://github.com/nexxeln/beavy/blob/main/docs/cli-contract.md#beavy-snapshots-create";

const WORKTREES_CREATE_HELP: &str = "\
Examples:
  Create a worktree of a snapshot
    beavy worktrees create <SNAPSHOT>

  Create a named worktree and read its ID as JSON
    beavy worktrees create <SNAPSHOT> --name <NAME> --json

Notes:
  A worktree is a snapshot plus a private layer that holds its changes. Creating
  one copies nothing. beavy mounts it once with overlayfs first, so it fails
  here, with the fix, when this machine can't run worktrees.

Learn more: https://github.com/nexxeln/beavy/blob/main/docs/cli-contract.md#beavy-worktrees-create";

const WORKTREES_LIST_HELP: &str = "\
Examples:
  List worktrees, newest first
    beavy worktrees list

Learn more: https://github.com/nexxeln/beavy/blob/main/docs/cli-contract.md#beavy-worktrees-list";

const WORKTREES_INSPECT_HELP: &str = "\
Examples:
  Show a worktree, its mounts, and the commands running in it, as JSON
    beavy worktrees inspect <WORKTREE> --json

Learn more: https://github.com/nexxeln/beavy/blob/main/docs/cli-contract.md#beavy-worktrees-inspect";

const WORKTREES_RUN_HELP: &str = "\
Examples:
  Run the tests in a worktree
    beavy worktrees run <WORKTREE> -- pnpm test

  Open a shell in a worktree
    beavy worktrees run <WORKTREE> -- bash

Notes:
  The command runs in its own mount namespace, with the worktree mounted over
  the directory its snapshot was imported from, so it sees the project at its
  usual path. Nothing outside the command sees the worktree. The command starts
  in the current directory when that's inside the project, and at the project
  root otherwise. beavy replaces itself with the command, so its output, signals,
  user, and exit status are the command's own. BEAVY_WORKTREE holds the
  worktree's ID. Commands in one worktree can run at the same time; they share
  one mount.

Learn more: https://github.com/nexxeln/beavy/blob/main/docs/cli-contract.md#beavy-worktrees-run";

const WORKTREES_MOUNT_HELP: &str = "\
Examples:
  Mount a worktree where every process can see it
    sudo beavy worktrees mount <WORKTREE> <DIR>

Notes:
  Needs root. A worktree has one mount at a time, and mounting is refused while
  commands run in it.

Learn more: https://github.com/nexxeln/beavy/blob/main/docs/cli-contract.md#beavy-worktrees-mount";

const WORKTREES_UNMOUNT_HELP: &str = "\
Examples:
  Unmount a worktree
    sudo beavy worktrees unmount <WORKTREE>

Learn more: https://github.com/nexxeln/beavy/blob/main/docs/cli-contract.md#beavy-worktrees-unmount";

const WORKTREES_DELETE_HELP: &str = "\
Examples:
  Delete a worktree
    beavy worktrees delete <WORKTREE>

Notes:
  The worktree is unmounted and moves to the trash; its changes stay on disk
  until beavy trash empty. Refused while commands run in it.

Learn more: https://github.com/nexxeln/beavy/blob/main/docs/cli-contract.md#beavy-worktrees-delete";

const SNAPSHOTS_LIST_HELP: &str = "\
Examples:
  List snapshots, newest first
    beavy snapshots list

Learn more: https://github.com/nexxeln/beavy/blob/main/docs/cli-contract.md#beavy-snapshots-list";

const SNAPSHOTS_INSPECT_HELP: &str = "\
Examples:
  Show a snapshot as JSON
    beavy snapshots inspect <SNAPSHOT> --json

Learn more: https://github.com/nexxeln/beavy/blob/main/docs/cli-contract.md#beavy-snapshots-inspect";

const SNAPSHOTS_DELETE_HELP: &str = "\
Examples:
  Delete a snapshot
    beavy snapshots delete <SNAPSHOT>

Notes:
  The snapshot moves to the trash and its data stays on disk until
  beavy trash empty. Refused while another snapshot was made from it or a
  worktree uses it.

Learn more: https://github.com/nexxeln/beavy/blob/main/docs/cli-contract.md#beavy-snapshots-delete";

const TRASH_LIST_HELP: &str = "\
Examples:
  List deleted snapshots and worktrees, most recently deleted first
    beavy trash list

Learn more: https://github.com/nexxeln/beavy/blob/main/docs/cli-contract.md#beavy-trash-list";

const TRASH_RESTORE_HELP: &str = "\
Examples:
  Restore a deleted snapshot or worktree
    beavy trash restore <ID>

Learn more: https://github.com/nexxeln/beavy/blob/main/docs/cli-contract.md#beavy-trash-restore";

const TRASH_EMPTY_HELP: &str = "\
Examples:
  Empty the trash in a script or agent, after a human approves
    beavy trash empty --yes

Notes:
  Deletes every snapshot and worktree in the trash, with the data only they
  use. This can't be undone, so a run without a terminal needs --yes.

Learn more: https://github.com/nexxeln/beavy/blob/main/docs/cli-contract.md#beavy-trash-empty";

#[derive(Subcommand)]
enum HostCommand {
    /// Check which worktree backends this machine supports
    #[command(help_template = HELP_TEMPLATE, after_help = host::CHECK_HELP)]
    Check,
}

/// Why a command didn't complete.
enum Failure {
    Command(Error),
    Output(io::Error),
}

impl From<Error> for Failure {
    fn from(err: Error) -> Self {
        Failure::Command(err)
    }
}

impl From<io::Error> for Failure {
    fn from(err: io::Error) -> Self {
        Failure::Output(err)
    }
}

fn main() -> ExitCode {
    ExitCode::from(run(std::env::args_os().collect()))
}

/// The command tree, with root help grouped by task.
fn cli_command() -> clap::Command {
    let mut command = Cli::command();
    command.build();
    let width = command
        .get_subcommands()
        .filter(|sub| !sub.is_hide_set())
        .map(|sub| sub.get_name().len())
        .max()
        .unwrap_or(0);
    let mut groups = String::new();
    for (heading, names) in COMMAND_GROUPS {
        groups.push_str(&format!("{heading}:\n"));
        for name in *names {
            let about = command
                .find_subcommand(name)
                .and_then(|sub| sub.get_about())
                .map(|about| about.to_string())
                .unwrap_or_default();
            groups.push_str(&format!("  {name:<width$}  {about}\n"));
        }
        groups.push('\n');
    }
    command.help_template(format!(
        "{{usage-heading}} {{usage}}\n{{about}}\n\n{groups}Global Options:\n{{options}}{{after-help}}"
    ))
}

fn run(args: Vec<OsString>) -> u8 {
    let mut command = cli_command();
    let parsed = command
        .try_get_matches_from_mut(&args)
        .and_then(|matches| Cli::from_arg_matches(&matches));
    let cli = match parsed {
        Ok(cli) => cli,
        Err(err) => return parse_failure(&command, &args, &err),
    };
    let mode = Mode::detect(ModeOptions {
        json: cli.global.json,
        non_interactive: cli.global.non_interactive,
        no_color: cli.global.no_color,
    });
    let mut output = Output::stdio(mode);
    let result = dispatch(&command, &cli, &mut output);
    finish(&mut output, result)
}

fn dispatch(
    root: &clap::Command,
    cli: &Cli,
    output: &mut Output<io::Stdout, io::Stderr>,
) -> Result<(), Failure> {
    if cli.global.version {
        return version(output);
    }
    match &cli.command {
        None => help(root, &[], output),
        Some(Command::Help { path }) => help(root, path, output),
        Some(Command::Host { command: None }) => help(root, &["host".into()], output),
        Some(Command::Host {
            command: Some(HostCommand::Check),
        }) => host::check(output, &working_directory(&cli.global)?),
        Some(Command::Snapshots { command: None, .. }) => help(root, &["snapshots".into()], output),
        Some(Command::Trash { command: None, .. }) => help(root, &["trash".into()], output),
        Some(Command::Worktrees { command: None, .. }) => help(root, &["worktrees".into()], output),
        #[cfg(target_os = "linux")]
        Some(Command::Worktrees {
            store,
            command: Some(command),
        }) => worktrees::run(output, &cli.global, store, command),
        #[cfg(target_os = "linux")]
        Some(Command::Snapshots {
            store,
            command: Some(command),
        }) => snapshots::run(output, &cli.global, store, command),
        #[cfg(target_os = "linux")]
        Some(Command::Trash {
            store,
            command: Some(command),
        }) => trash::run(output, &cli.global, store, command),
        #[cfg(not(target_os = "linux"))]
        Some(Command::Snapshots { .. } | Command::Trash { .. } | Command::Worktrees { .. }) => {
            Err(unavailable().into())
        }
    }
}

/// The directory an invocation works in: `--cwd`, or the current directory.
fn working_directory(global: &Global) -> Result<PathBuf, Error> {
    match &global.cwd {
        Some(dir) => Ok(dir.clone()),
        None => std::env::current_dir().map_err(|err| {
            Error::new(Reason::IoError, "Failed to read the current directory.").with_cause(err)
        }),
    }
}

/// Snapshots and worktrees need overlayfs, so every platform but Linux refuses them.
#[cfg(not(target_os = "linux"))]
fn unavailable() -> Error {
    Error::new(
        Reason::BackendUnavailable,
        "Can't use snapshots or worktrees on this system yet. beavy's worktree backend is overlayfs, which only Linux has.",
    )
    .with_next(
        "See which backends this machine supports",
        Next::new(["beavy", "host", "check"]),
    )
}

fn finish(output: &mut Output<io::Stdout, io::Stderr>, result: Result<(), Failure>) -> u8 {
    let err = match result {
        Ok(()) => return 0,
        Err(Failure::Output(err)) if err.kind() == ErrorKind::BrokenPipe => return 0,
        Err(Failure::Output(err)) => {
            Error::new(Reason::IoError, "Failed to write output.").with_cause(err)
        }
        Err(Failure::Command(err)) => err,
    };
    let _ = output.error(&err);
    err.outcome().exit_code()
}

fn version(output: &mut Output<io::Stdout, io::Stderr>) -> Result<(), Failure> {
    #[derive(Serialize)]
    struct Version {
        version: &'static str,
    }
    match output.mode().format {
        Format::Json => output.json(&Document::new(Version { version: VERSION }))?,
        Format::Human => output.stdout().raw(&format!("{VERSION}\n"))?,
    }
    Ok(())
}

/// Print help for the command at `path` (below the root) to stdout.
fn help(
    root: &clap::Command,
    path: &[String],
    output: &mut Output<io::Stdout, io::Stderr>,
) -> Result<(), Failure> {
    let mut command = root.clone();
    command.build();
    let mut walked = vec!["beavy".to_string()];
    for segment in path {
        let Some(child) = command
            .get_subcommands()
            .find(|c| c.get_name() == segment && !c.is_hide_set())
            .cloned()
        else {
            return Err(unknown_command(segment, &walked, None).into());
        };
        walked.push(segment.clone());
        command = child;
    }
    let text = command.render_help().to_string();
    output.stdout().raw(&text)?;
    Ok(())
}

fn unknown_command(segment: &str, path: &[String], suggestion: Option<String>) -> Error {
    let mut message = format!("Unknown command {segment} for {}.", path.join(" "));
    if let Some(suggestion) = suggestion {
        message.push_str(&format!(" Did you mean {suggestion}?"));
    }
    let help = path.iter().cloned().chain(["--help".to_string()]);
    Error::new(Reason::UnknownCommand, message).with_next("See all commands", Next::new(help))
}

/// Handle a parse that ended before dispatch: help requests print help and exit 0,
/// everything else is a usage error rendered under the contract.
fn parse_failure(root: &clap::Command, args: &[OsString], err: &clap::Error) -> u8 {
    let flag = |name: &str| {
        args.iter()
            .skip(1)
            .take_while(|arg| *arg != "--")
            .any(|arg| arg == name)
    };
    let mode = Mode::detect(ModeOptions {
        json: flag("--json"),
        non_interactive: flag("--non-interactive"),
        no_color: flag("--no-color"),
    });
    let mut output = Output::stdio(mode);
    let result = match err.kind() {
        ClapErrorKind::DisplayHelp | ClapErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
            output
                .stdout()
                .raw(&err.render().to_string())
                .map_err(Failure::from)
        }
        _ => Err(Failure::Command(usage_error(root, args, err))),
    };
    finish(&mut output, result)
}

fn context(err: &clap::Error, kind: ContextKind) -> Option<String> {
    match err.get(kind)? {
        ContextValue::String(value) => Some(value.clone()),
        ContextValue::Strings(values) => values.first().cloned(),
        _ => None,
    }
}

fn usage_error(root: &clap::Command, args: &[OsString], err: &clap::Error) -> Error {
    let path = command_path(root, args);
    let shown = path.join(" ");
    let help = Next::new(path.iter().cloned().chain(["--help".to_string()]));
    let did_you_mean = |kind| {
        context(err, kind)
            .map(|suggestion| format!(" Did you mean {suggestion}?"))
            .unwrap_or_default()
    };
    match err.kind() {
        ClapErrorKind::UnknownArgument => {
            let arg = context(err, ContextKind::InvalidArg).unwrap_or_default();
            let suggestion = did_you_mean(ContextKind::SuggestedArg);
            let (reason, message) = if arg.starts_with('-') {
                (
                    Reason::UnknownOption,
                    format!("Unknown option {arg} for {shown}.{suggestion}"),
                )
            } else {
                (
                    Reason::InvalidArguments,
                    format!("Unexpected argument {arg} for {shown}."),
                )
            };
            Error::new(reason, message).with_next("See all options", help)
        }
        ClapErrorKind::InvalidSubcommand => {
            let segment = context(err, ContextKind::InvalidSubcommand).unwrap_or_default();
            unknown_command(
                &segment,
                &path,
                context(err, ContextKind::SuggestedSubcommand),
            )
        }
        ClapErrorKind::MissingRequiredArgument => {
            let missing = context(err, ContextKind::InvalidArg).unwrap_or_default();
            Error::new(
                Reason::MissingArgument,
                format!("Can't run {shown} without {missing}."),
            )
            .with_next("See all options", help)
        }
        _ => {
            let detail = clap_detail(err);
            Error::new(
                Reason::InvalidArguments,
                format!("Invalid arguments for {shown}. {detail}"),
            )
            .with_next("See all options", help)
        }
    }
}

/// clap's own description of the problem, as one sentence.
fn clap_detail(err: &clap::Error) -> String {
    let rendered = err.render().to_string();
    let line = rendered.lines().next().unwrap_or_default();
    let line = line.strip_prefix("error: ").unwrap_or(line).trim();
    let mut chars = line.chars();
    let mut detail: String = match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => "The arguments couldn't be parsed".to_string(),
    };
    if !detail.ends_with('.') {
        detail.push('.');
    }
    detail
}

/// The command path an argv selects, as far as it names real subcommands, such as
/// `["beavy", "host", "check"]`.
fn command_path(root: &clap::Command, args: &[OsString]) -> Vec<String> {
    let mut command = root;
    let mut path = vec!["beavy".to_string()];
    let mut tokens = args.iter().skip(1).filter_map(|arg| arg.to_str());
    while let Some(token) = tokens.next() {
        if token == "--" {
            break;
        }
        if let Some(name) = token.strip_prefix("--") {
            let takes_value = !name.contains('=')
                && command
                    .get_arguments()
                    .any(|arg| arg.get_long() == Some(name) && arg.get_action().takes_values());
            if takes_value {
                tokens.next();
            }
            continue;
        }
        if token.starts_with('-') {
            continue;
        }
        match command.find_subcommand(token) {
            Some(child) if !child.is_hide_set() => {
                path.push(child.get_name().to_string());
                command = child;
            }
            _ => break,
        }
    }
    path
}
