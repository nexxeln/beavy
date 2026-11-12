//! The versioned JSON envelope every `--json` document uses.

use serde::Serialize;

use crate::error::{Error, Next, Reason, Status};

/// Version of the JSON envelope and of every result shape inside it. It changes only
/// with a breaking change to a documented field.
pub const SCHEMA_VERSION: u32 = 1;

/// A nonfatal condition that needs attention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Warning {
    pub reason: WarningReason,
    pub message: String,
}

/// The registry of stable warning reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WarningReason {
    /// A temporary file or directory beavy created couldn't be removed.
    CleanupFailed,
    /// An import left out entries it can't copy, such as sockets.
    EntriesSkipped,
    /// The store is on overlayfs, where worktrees are slow or refused.
    StoreOnOverlayfs,
    /// A sealed worktree couldn't be mounted again where it was mounted before.
    RemountFailed,
}

/// A successful result: `schemaVersion`, the result's own top-level keys, then
/// `warnings` and `next` when they apply.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Document<T> {
    schema_version: u32,
    #[serde(flatten)]
    result: T,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<Warning>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    next: Vec<Next>,
}

impl<T: Serialize> Document<T> {
    /// `result` must serialize as a JSON object keyed by noun, such as
    /// `{ "host": { ... } }`.
    pub fn new(result: T) -> Self {
        Document {
            schema_version: SCHEMA_VERSION,
            result,
            warnings: Vec::new(),
            next: Vec::new(),
        }
    }

    pub fn with_warnings(mut self, warnings: Vec<Warning>) -> Self {
        self.warnings = warnings;
        self
    }

    pub fn with_next(mut self, next: Next) -> Self {
        self.next.push(next);
        self
    }
}

/// The document written to stderr when a command fails in JSON mode.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorDocument<'a> {
    schema_version: u32,
    status: Status,
    reason: Reason,
    message: &'a str,
    cause: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    next: Vec<&'a Next>,
}

impl<'a> From<&'a Error> for ErrorDocument<'a> {
    fn from(err: &'a Error) -> Self {
        ErrorDocument {
            schema_version: SCHEMA_VERSION,
            status: err.class().status(),
            reason: err.reason(),
            message: err.message(),
            cause: err.cause(),
            next: err.next().into_iter().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    #[derive(Serialize)]
    struct Thing {
        thing: u32,
    }

    #[test]
    fn document_flattens_the_result_beside_the_schema_version() {
        let doc = serde_json::to_value(Document::new(Thing { thing: 7 })).unwrap();
        assert_eq!(doc, json!({ "schemaVersion": 1, "thing": 7 }));
    }

    #[test]
    fn document_adds_warnings_and_next_only_when_they_apply() {
        let doc = Document::new(Thing { thing: 7 })
            .with_warnings(vec![Warning {
                reason: WarningReason::CleanupFailed,
                message: "Couldn't remove /tmp/x.".into(),
            }])
            .with_next(Next::new(["beavy", "host", "check"]));
        let doc = serde_json::to_value(doc).unwrap();
        assert_eq!(
            doc,
            json!({
                "schemaVersion": 1,
                "thing": 7,
                "warnings": [{ "reason": "cleanup_failed", "message": "Couldn't remove /tmp/x." }],
                "next": [{ "command": "beavy host check", "argv": ["beavy", "host", "check"] }]
            })
        );
    }

    #[test]
    fn error_document_has_every_required_field() {
        let err = Error::new(Reason::UnknownOption, "Unknown option --x for beavy.");
        let doc = serde_json::to_value(ErrorDocument::from(&err)).unwrap();
        assert_eq!(
            doc,
            json!({
                "schemaVersion": 1,
                "status": "error",
                "reason": "unknown_option",
                "message": "Unknown option --x for beavy.",
                "cause": null
            })
        );
    }

    #[test]
    fn error_document_carries_cause_and_next() {
        let err = Error::new(Reason::MissingArgument, "Can't check without a directory.")
            .with_cause(std::io::Error::other("boom"))
            .with_next(
                "Pass one",
                Next::new(["beavy", "host", "check", "--cwd", "<DIR>"]).when("Replace <DIR>"),
            );
        let doc = serde_json::to_value(ErrorDocument::from(&err)).unwrap();
        assert_eq!(doc["status"], "action_required");
        assert_eq!(doc["cause"], "boom");
        assert_eq!(
            doc["next"],
            json!([{
                "command": "beavy host check --cwd <DIR>",
                "argv": ["beavy", "host", "check", "--cwd", "<DIR>"],
                "when": "Replace <DIR>"
            }])
        );
        assert!(matches!(doc, Value::Object(_)));
    }
}
