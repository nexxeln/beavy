use std::fmt;

use serde::Serialize;

use crate::outcome::Outcome;

/// The failure class. It fixes the exit code and the JSON `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Unknown command or option, or a malformed value.
    Usage,
    /// Required input is missing and the run can't ask for it.
    MissingInput,
    /// A named resource doesn't exist.
    NotFound,
    /// The process isn't allowed to do what was asked.
    Permission,
    /// The operating system or a tool beavy runs failed.
    System,
    /// A defect in beavy itself.
    Internal,
}

impl ErrorClass {
    pub const fn outcome(self) -> Outcome {
        match self {
            ErrorClass::Usage | ErrorClass::MissingInput => Outcome::Usage,
            ErrorClass::NotFound
            | ErrorClass::Permission
            | ErrorClass::System
            | ErrorClass::Internal => Outcome::Failure,
        }
    }

    pub const fn status(self) -> Status {
        match self {
            ErrorClass::MissingInput => Status::ActionRequired,
            _ => Status::Error,
        }
    }
}

/// The JSON `status` of an error document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Blocked before any effect; supplying input or approval lets the command run.
    ActionRequired,
    /// Anything else.
    Error,
}

/// The registry of stable error reasons. Each serializes as its snake_case code and
/// belongs to exactly one class. Codes never change meaning once published.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    InvalidArguments,
    UnknownOption,
    UnknownCommand,
    MissingArgument,
    NotFound,
    PermissionDenied,
    IoError,
    InternalError,
}

impl Reason {
    pub const fn class(self) -> ErrorClass {
        match self {
            Reason::InvalidArguments | Reason::UnknownOption | Reason::UnknownCommand => {
                ErrorClass::Usage
            }
            Reason::MissingArgument => ErrorClass::MissingInput,
            Reason::NotFound => ErrorClass::NotFound,
            Reason::PermissionDenied => ErrorClass::Permission,
            Reason::IoError => ErrorClass::System,
            Reason::InternalError => ErrorClass::Internal,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Reason::InvalidArguments => "invalid_arguments",
            Reason::UnknownOption => "unknown_option",
            Reason::UnknownCommand => "unknown_command",
            Reason::MissingArgument => "missing_argument",
            Reason::NotFound => "not_found",
            Reason::PermissionDenied => "permission_denied",
            Reason::IoError => "io_error",
            Reason::InternalError => "internal_error",
        }
    }
}

/// One exact command to run next, as a display string and as argv.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Next {
    pub command: String,
    pub argv: Vec<String>,
    /// When to use the command, or how to fill its placeholders.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
}

impl Next {
    pub fn new<I, S>(argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let argv: Vec<String> = argv.into_iter().map(Into::into).collect();
        let command = argv
            .iter()
            .map(|arg| shell_quote(arg))
            .collect::<Vec<_>>()
            .join(" ");
        Next {
            command,
            argv,
            when: None,
        }
    }

    pub fn when(mut self, when: impl Into<String>) -> Self {
        self.when = Some(when.into());
        self
    }
}

fn shell_quote(arg: &str) -> String {
    let is_placeholder = arg.len() > 2
        && arg.starts_with('<')
        && arg.ends_with('>')
        && arg[1..arg.len() - 1]
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == '_');
    let is_plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c));
    if is_placeholder || is_plain {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

type Cause = Box<dyn std::error::Error + Send + Sync + 'static>;

/// A failure that says what failed, why, and what to run next.
///
/// `message` is the human headline: the failed action, its object, and the known
/// cause. `cause` is the underlying error, when there is one. `next` is one exact
/// command that moves the caller forward, with the instruction that introduces it
/// in human output.
#[derive(Debug)]
pub struct Error(Box<Inner>);

#[derive(Debug)]
struct Inner {
    reason: Reason,
    message: String,
    cause: Option<Cause>,
    next: Option<(String, Next)>,
}

impl Error {
    pub fn new(reason: Reason, message: impl Into<String>) -> Self {
        Error(Box::new(Inner {
            reason,
            message: message.into(),
            cause: None,
            next: None,
        }))
    }

    pub fn with_cause(mut self, cause: impl Into<Cause>) -> Self {
        self.0.cause = Some(cause.into());
        self
    }

    /// Attach the next command. `instruction` introduces it in human output, such as
    /// "See all options".
    pub fn with_next(mut self, instruction: impl Into<String>, next: Next) -> Self {
        self.0.next = Some((instruction.into(), next));
        self
    }

    pub fn reason(&self) -> Reason {
        self.0.reason
    }

    pub fn class(&self) -> ErrorClass {
        self.0.reason.class()
    }

    pub fn outcome(&self) -> Outcome {
        self.0.reason.class().outcome()
    }

    pub fn message(&self) -> &str {
        &self.0.message
    }

    /// The underlying cause and its own sources, joined with ": ".
    pub fn cause(&self) -> Option<String> {
        let mut source: Option<&(dyn std::error::Error + 'static)> = Some(self.0.cause.as_deref()?);
        let mut parts = Vec::new();
        while let Some(err) = source {
            parts.push(err.to_string());
            source = err.source();
        }
        Some(parts.join(": "))
    }

    pub fn next(&self) -> Option<&Next> {
        self.0.next.as_ref().map(|(_, next)| next)
    }

    pub fn next_instruction(&self) -> Option<&str> {
        self.0
            .next
            .as_ref()
            .map(|(instruction, _)| instruction.as_str())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.message)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0
            .cause
            .as_deref()
            .map(|cause| cause as &(dyn std::error::Error + 'static))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_map_to_classes_outcomes_and_statuses() {
        let cases = [
            (Reason::InvalidArguments, 2, Status::Error),
            (Reason::UnknownOption, 2, Status::Error),
            (Reason::UnknownCommand, 2, Status::Error),
            (Reason::MissingArgument, 2, Status::ActionRequired),
            (Reason::NotFound, 1, Status::Error),
            (Reason::PermissionDenied, 1, Status::Error),
            (Reason::IoError, 1, Status::Error),
            (Reason::InternalError, 1, Status::Error),
        ];
        for (reason, exit, status) in cases {
            assert_eq!(reason.class().outcome().exit_code(), exit, "{reason:?}");
            assert_eq!(reason.class().status(), status, "{reason:?}");
        }
    }

    #[test]
    fn reason_codes_serialize_as_their_registry_strings() {
        for reason in [
            Reason::InvalidArguments,
            Reason::UnknownOption,
            Reason::UnknownCommand,
            Reason::MissingArgument,
            Reason::NotFound,
            Reason::PermissionDenied,
            Reason::IoError,
            Reason::InternalError,
        ] {
            assert_eq!(
                serde_json::to_value(reason).unwrap(),
                serde_json::Value::from(reason.as_str())
            );
        }
    }

    #[test]
    fn next_quotes_only_what_the_shell_needs() {
        let next = Next::new(["beavy", "host", "check", "--cwd", "<DIR>"]);
        assert_eq!(next.command, "beavy host check --cwd <DIR>");
        let next = Next::new(["beavy", "host", "check", "--cwd", "/tmp/it's here"]);
        assert_eq!(next.command, r"beavy host check --cwd '/tmp/it'\''s here'");
        assert_eq!(next.argv[4], "/tmp/it's here");
        assert_eq!(Next::new(["a", ""]).command, "a ''");
    }

    #[test]
    fn error_carries_cause_chain_and_next() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let err = Error::new(Reason::NotFound, "Couldn't find directory ./x.")
            .with_cause(io)
            .with_next("See all options", Next::new(["beavy", "--help"]));
        assert_eq!(err.to_string(), "Couldn't find directory ./x.");
        assert_eq!(err.cause().as_deref(), Some("no such file"));
        assert_eq!(err.next_instruction(), Some("See all options"));
        assert_eq!(err.next().unwrap().argv, ["beavy", "--help"]);
        assert_eq!(err.outcome(), Outcome::Failure);
        assert!(std::error::Error::source(&err).is_some());

        let bare = Error::new(Reason::InternalError, "Unexpected error.");
        assert_eq!(bare.cause(), None);
        assert!(bare.next().is_none());
    }
}
