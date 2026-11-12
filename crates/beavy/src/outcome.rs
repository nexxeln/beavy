/// How an invocation ended, and the process exit code that reports it.
///
/// Exit codes are part of the public contract; see `docs/cli-contract.md`.
/// Termination by a signal is left to the operating system, which reports
/// 130 for Ctrl+C and 128 + n for signal n.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The promised operation completed: a result, an empty result, a no-op, help, or the version.
    Success,
    /// The input was accepted and the operation failed.
    Failure,
    /// The input was invalid or incomplete, so no work started.
    Usage,
}

impl Outcome {
    pub const fn exit_code(self) -> u8 {
        match self {
            Outcome::Success => 0,
            Outcome::Failure => 1,
            Outcome::Usage => 2,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_the_contract() {
        assert_eq!(Outcome::Success.exit_code(), 0);
        assert_eq!(Outcome::Failure.exit_code(), 1);
        assert_eq!(Outcome::Usage.exit_code(), 2);
    }
}
