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

mod host;

const HELP_TEMPLATE: &str = "{usage-heading} {usage}\n{about}\n\n{all-args}{after-help}";
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Monorepo tooling for teams that work in parallel
#[derive(Parser)]
#[command(
    name = "beavy",
    disable_version_flag = true,
    disable_help_flag = true,
    disable_help_subcommand = true,
    help_template = HELP_TEMPLATE,
    subcommand_help_heading = "Machine",
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

#[derive(Subcommand)]
enum Command {
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

fn run(args: Vec<OsString>) -> u8 {
    let mut command = Cli::command();
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
        }) => {
            let directory = match &cli.global.cwd {
                Some(dir) => dir.clone(),
                None => std::env::current_dir().map_err(|err| {
                    Error::new(Reason::IoError, "Failed to read the current directory.")
                        .with_cause(err)
                })?,
            };
            host::check(output, &directory)
        }
    }
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
