//! beavy's capabilities as a library.
//!
//! Every capability the `beavy` CLI exposes lives here, so a sandbox provider or an
//! agent platform can link this crate instead of shelling out. The CLI is a thin
//! front end: it parses arguments, calls into this crate, and renders the result
//! with [`output`].
//!
//! The output contract (streams, the JSON envelope, exit codes, errors) is
//! documented in `docs/cli-contract.md`.

pub mod envelope;
pub mod error;
pub mod outcome;

pub use envelope::{Document, SCHEMA_VERSION, Warning, WarningReason};
pub use error::{Error, ErrorClass, Next, Reason, Status};
pub use outcome::Outcome;
