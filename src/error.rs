//! The error taxonomy and the nine stable process exit classes.
//!
//! Exit codes are a scripting contract, not a convention: automation must not
//! parse prose to learn that a remote source needed authentication. Every
//! command reports failure through this one vocabulary.
//!
//! There is one derivation from a failure to its exit class and it runs through
//! the symbolic problem code: [`Error::problem_code`] names the stable
//! [`crate::problem_code::ProblemCode`] for a failure, and
//! [`ExitClass`]'s `From<ProblemCode>` is the single place a code is coarsened to
//! a class. [`Error::exit_class`] is the only entry point callers use, and it is
//! that composition with no mapping of its own, so the class the binary returns
//! and the code the JSON envelope carries can never disagree.

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
/// is `Ok` and maps to [`ExitClass::Success`] in the binary. A variant is
/// coarsened to a class only through its [`crate::problem_code::ProblemCode`]
/// (see [`Error::problem_code`]); [`Error::exit_class`] is that composition and
/// carries no class mapping of its own.
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
    /// The stable symbolic problem code for this failure.
    ///
    /// This is the one derivation from an [`Error`] to a
    /// [`crate::problem_code::ProblemCode`]; the JSON error envelope and the
    /// process exit class are both read from it, so they cannot drift. The
    /// conversion match lives with the other `ProblemCode` derivations in
    /// [`crate::problem_code`] and is exhaustive with no wildcard arm.
    pub fn problem_code(&self) -> crate::problem_code::ProblemCode {
        self.into()
    }

    /// The process exit class for this failure.
    ///
    /// This is the only entry point for turning a failure into a class, and the
    /// only method of its name in the tree. It is `problem_code` composed with
    /// `ExitClass`'s `From<ProblemCode>`, and carries no class mapping of its own.
    pub fn exit_class(&self) -> ExitClass {
        ExitClass::from(self.problem_code())
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

    /// One instance of every [`Error`] variant, so a test that must cover the
    /// whole taxonomy drives from this rather than keeping its own list.
    fn every_variant() -> [Error; 8] {
        [
            Error::Internal("x".into()),
            Error::Usage("x".into()),
            Error::AuthRequired("x".into()),
            Error::RemoteUnavailable("x".into()),
            Error::Store("x".into()),
            Error::InsufficientEvidence("x".into()),
            Error::ThresholdNotMet("x".into()),
            Error::IngestIncomplete("x".into()),
        ]
    }

    /// Every error variant derives a problem code and, through it, its
    /// documented class. `Error::problem_code`'s conversion match and
    /// `ExitClass`'s `From<ProblemCode>` are both exhaustive with no wildcard
    /// arm, so adding a variant to either enum breaks compilation until it is
    /// handled; this test then pins the correct pairing for each variant so the
    /// two cannot drift.
    ///
    /// The planted negative is the `assert_ne!` block: an implementation that
    /// collapsed distinct failures onto one code, or routed every code to
    /// `Internal`, would still pass the positive rows above and fails here.
    #[test]
    fn every_error_derives_a_code_and_a_class_that_agree() {
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
            assert_eq!(ExitClass::from(error.problem_code()), class);
        }

        let codes: std::collections::BTreeSet<&str> = every_variant()
            .iter()
            .map(|error| error.problem_code().code())
            .collect();
        assert_eq!(
            codes.len(),
            8,
            "each error variant must derive a distinct problem code"
        );
        assert_ne!(
            Error::Usage("x".into()).problem_code().code(),
            Error::Internal("x".into()).problem_code().code(),
        );
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
