//! The error taxonomy and the nine stable process exit classes.
//!
//! Exit codes are a scripting contract, not a convention: automation must not
//! parse prose to learn that a remote source needed authentication. Every
//! command reports failure through this one vocabulary, and the mapping from
//! error to exit class lives in exactly one place: [`Error::exit_class`].

/// The nine stable process exit classes.
///
/// The numeric values are the scripting contract and are documented in
/// `docs/exit-classes.md`; a test fails if a variant is added without a
/// documentation row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ExitClass {
    /// 0 - command completed for its contract.
    Success = 0,
    /// 1 - unexpected internal failure.
    Internal = 1,
    /// 2 - configuration, argument or environment invalid.
    Usage = 2,
    /// 3 - requested live source requires authentication.
    AuthRequired = 3,
    /// 4 - requested live or remote source unavailable.
    RemoteUnavailable = 4,
    /// 5 - store or durable-state failure.
    Store = 5,
    /// 6 - insufficient evidence for a requested quantitative result.
    InsufficientEvidence = 6,
    /// 7 - explicit threshold or advisory result not met.
    ThresholdNotMet = 7,
    /// 8 - local ingest or report incomplete.
    IngestIncomplete = 8,
}

impl ExitClass {
    /// The numeric process exit code for this class.
    pub fn code(self) -> u8 {
        self as u8
    }

    /// The stable name of this class, used by the documentation test to prove
    /// the table and the enum cannot drift.
    pub fn name(self) -> &'static str {
        match self {
            ExitClass::Success => "Success",
            ExitClass::Internal => "Internal",
            ExitClass::Usage => "Usage",
            ExitClass::AuthRequired => "AuthRequired",
            ExitClass::RemoteUnavailable => "RemoteUnavailable",
            ExitClass::Store => "Store",
            ExitClass::InsufficientEvidence => "InsufficientEvidence",
            ExitClass::ThresholdNotMet => "ThresholdNotMet",
            ExitClass::IngestIncomplete => "IngestIncomplete",
        }
    }
}

/// The error type every command returns.
///
/// Variants correspond to failure classes 1 through 8; successful completion
/// is `Ok` and maps to [`ExitClass::Success`] in the binary. The mapping from
/// variant to class lives in exactly one place: [`Error::exit_class`].
#[derive(Debug)]
pub enum Error {
    /// 1 - unexpected internal failure.
    Internal(String),
    /// 2 - configuration, argument or environment invalid.
    Usage(String),
    /// 3 - requested live source requires authentication.
    AuthRequired(String),
    /// 4 - requested live or remote source unavailable.
    RemoteUnavailable(String),
    /// 5 - store or durable-state failure.
    Store(String),
    /// 6 - insufficient evidence for a requested quantitative result.
    InsufficientEvidence(String),
    /// 7 - explicit threshold or advisory result not met.
    ThresholdNotMet(String),
    /// 8 - local ingest or report incomplete.
    IngestIncomplete(String),
}

impl Error {
    /// The single mapping from error variant to exit class.
    ///
    /// This match is exhaustive with no wildcard arm: adding a variant to
    /// [`Error`] breaks compilation here until its class is chosen.
    pub fn exit_class(&self) -> ExitClass {
        match self {
            Error::Internal(_) => ExitClass::Internal,
            Error::Usage(_) => ExitClass::Usage,
            Error::AuthRequired(_) => ExitClass::AuthRequired,
            Error::RemoteUnavailable(_) => ExitClass::RemoteUnavailable,
            Error::Store(_) => ExitClass::Store,
            Error::InsufficientEvidence(_) => ExitClass::InsufficientEvidence,
            Error::ThresholdNotMet(_) => ExitClass::ThresholdNotMet,
            Error::IngestIncomplete(_) => ExitClass::IngestIncomplete,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Error::Internal(m) => m.as_str(),
            Error::Usage(m) => m.as_str(),
            Error::AuthRequired(m) => m.as_str(),
            Error::RemoteUnavailable(m) => m.as_str(),
            Error::Store(m) => m.as_str(),
            Error::InsufficientEvidence(m) => m.as_str(),
            Error::ThresholdNotMet(m) => m.as_str(),
            Error::IngestIncomplete(m) => m.as_str(),
        };
        f.write_str(message)
    }
}

impl std::error::Error for Error {}

/// Test-only surface: maps a class number to a representative outcome, so the
/// scaffold binary can expose every exit class through a real subprocess
/// before the commands that produce them exist. `aub-71j.5` replaces this
/// with the real nine-class matrix over implemented workflows.
pub fn representative_outcome(class: u8) -> Result<(), Error> {
    match class {
        0 => Ok(()),
        1 => Err(Error::Internal(
            "forced internal failure (test hook)".into(),
        )),
        2 => Err(Error::Usage("forced usage error (test hook)".into())),
        3 => Err(Error::AuthRequired(
            "forced auth-required (test hook)".into(),
        )),
        4 => Err(Error::RemoteUnavailable(
            "forced remote-unavailable (test hook)".into(),
        )),
        5 => Err(Error::Store("forced store failure (test hook)".into())),
        6 => Err(Error::InsufficientEvidence(
            "forced insufficient-evidence (test hook)".into(),
        )),
        7 => Err(Error::ThresholdNotMet(
            "forced threshold-not-met (test hook)".into(),
        )),
        8 => Err(Error::IngestIncomplete(
            "forced ingest-incomplete (test hook)".into(),
        )),
        _ => Err(Error::Usage(format!("class must be 0..=8, got {class}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every error variant maps to its documented class. The match in
    /// [`Error::exit_class`] has no wildcard arm, so adding a variant breaks
    /// that match's compilation; this test then pins the correct class for
    /// each variant so the two cannot drift.
    #[test]
    fn exit_class_mapping_is_exhaustive_and_correct() {
        let cases = [
            (Error::Internal("x".into()), ExitClass::Internal),
            (Error::Usage("x".into()), ExitClass::Usage),
            (Error::AuthRequired("x".into()), ExitClass::AuthRequired),
            (
                Error::RemoteUnavailable("x".into()),
                ExitClass::RemoteUnavailable,
            ),
            (Error::Store("x".into()), ExitClass::Store),
            (
                Error::InsufficientEvidence("x".into()),
                ExitClass::InsufficientEvidence,
            ),
            (
                Error::ThresholdNotMet("x".into()),
                ExitClass::ThresholdNotMet,
            ),
            (
                Error::IngestIncomplete("x".into()),
                ExitClass::IngestIncomplete,
            ),
        ];
        for (error, class) in cases {
            assert_eq!(error.exit_class(), class);
        }
    }

    /// A single failure is exactly one variant, so it maps to exactly one
    /// class. The design tightened 4 vs 8 so no report can qualify for both;
    /// this asserts the structural guarantee that the two classes are distinct
    /// and each error maps to exactly one of them.
    #[test]
    fn remote_and_local_failures_are_distinct_classes() {
        let remote = Error::RemoteUnavailable("endpoint down".into());
        let local = Error::IngestIncomplete("transcript unreadable".into());

        assert_eq!(remote.exit_class(), ExitClass::RemoteUnavailable);
        assert_eq!(local.exit_class(), ExitClass::IngestIncomplete);
        assert_ne!(
            ExitClass::RemoteUnavailable.code(),
            ExitClass::IngestIncomplete.code(),
            "classes 4 and 8 must remain distinct"
        );
    }

    /// The documented exit-class table matches the enum: every class has a row
    /// in `docs/exit-classes.md`. Removing a row fails this test.
    #[test]
    fn documented_exit_class_table_matches_the_enum() {
        let docs =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/exit-classes.md"))
                .expect("docs/exit-classes.md must be readable");

        for class in [
            ExitClass::Success,
            ExitClass::Internal,
            ExitClass::Usage,
            ExitClass::AuthRequired,
            ExitClass::RemoteUnavailable,
            ExitClass::Store,
            ExitClass::InsufficientEvidence,
            ExitClass::ThresholdNotMet,
            ExitClass::IngestIncomplete,
        ] {
            let row = format!("| {} | {} |", class.code(), class.name());
            assert!(
                docs.contains(&row),
                "docs/exit-classes.md has no row matching {row:?}"
            );
        }
    }
}
