//! Configured credential acquisition.
//!
//! The one module that turns a configured credential source into authentication
//! material plus a [`CredentialContextId`] that is safe to persist. The defect
//! this module exists to end is three tools independently deriving credential
//! paths: each derivation is plausible, they disagree in edge cases, and the
//! disagreement shows up as observations filed under the wrong account with
//! nothing indicating that anything went wrong.
//!
//! May not depend on:
//! - provider semantics (credential code does not know provider semantics)
//! - SQLite
//! - presentation

use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::config::AccountConfig;
use crate::domain::ids::CredentialContextId;
use crate::error::Error;

/// The typed credential-source model, interpreted from the config's loose
/// `credential` table. Only the kinds the first release's providers need
/// (aub-86g): an explicit credential file, or no credential at all (Codex
/// reads its meter from the transcript).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSource {
    /// A credential file at an explicit path.
    File { path: PathBuf },
    /// No credential: the provider's meter is read from its transcript.
    None,
}

impl CredentialSource {
    /// Interprets the config's loose `credential` table. An absent table and
    /// the explicit kind `none` both mean no credential; `file` names the
    /// credential file. Any other kind fails explicitly, naming the account
    /// and the kind, because a silently ignored credential is exactly how an
    /// observation lands under the wrong account.
    pub fn from_account(account: &AccountConfig) -> Result<Self, Error> {
        match account.credential_kind.as_str() {
            "" => {
                if account.credential_detail.is_empty() {
                    Ok(CredentialSource::None)
                } else {
                    Err(Error::Usage(format!(
                        "account '{}': credential table names '{}' without a kind",
                        account.name, account.credential_detail
                    )))
                }
            }
            "none" => Ok(CredentialSource::None),
            "file" => {
                let path = PathBuf::from(&account.credential_detail);
                if path.as_os_str().is_empty() {
                    Err(Error::Usage(format!(
                        "account '{}': credential kind 'file' requires a path",
                        account.name
                    )))
                } else {
                    Ok(CredentialSource::File { path })
                }
            }
            other => Err(Error::Usage(format!(
                "account '{}': unsupported credential kind '{}' (supported kinds: file, none)",
                account.name, other
            ))),
        }
    }
}

/// A value wrapped so its contents cannot leak: no `Debug`, no `Display`, and
/// no `SafeDiagnosticValue`. The only way to read the value is the named
/// accessor, so a credential can be used on purpose but never printed by
/// accident.
pub struct Secret<T>(T);

impl<T> Secret<T> {
    pub(crate) fn new(value: T) -> Self {
        Self(value)
    }

    /// The wrapped value, for the provider adapter to interpret.
    #[allow(dead_code)] // the adapter (aub-eun.4) and this bead's tests are the only readers; a plain `cargo check` build never sees either
    pub(crate) fn into_inner(self) -> T {
        self.0
    }
}

/// Provider-agnostic authentication material: the secret string a configured
/// source resolves to. Auth deliberately knows nothing about provider
/// semantics, so it does not decide which part of a credential file is the
/// token; the provider adapter interprets the material (aub-eun.4).
#[allow(dead_code)] // the field is read only through as_str, whose only readers are the adapter (aub-eun.4) and this bead's tests
pub struct AuthMaterial(String);

impl AuthMaterial {
    pub(crate) fn new(raw: String) -> Self {
        Self(raw)
    }

    /// The material for the provider adapter to interpret.
    #[allow(dead_code)] // the adapter (aub-eun.4) and this bead's tests are the only readers; a plain `cargo check` build never sees either
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// The outcome of resolving a configured account's credential source.
///
/// The context id is the second half of the job and the less obvious one: it
/// identifies the credential revision without exposing credential bytes, and it
/// is what scopes a sticky authentication failure to the credentials that
/// actually produced it.
pub struct ResolvedCredential {
    #[allow(dead_code)]
    // the adapter (aub-eun.4) and this bead's tests are the only readers; a plain `cargo check` build never sees either
    pub(crate) material: Secret<AuthMaterial>,
    pub context_id: CredentialContextId,
}

/// Debug prints the context id and redacts the material: a resolved credential
/// is debuggable without ever revealing what it carries.
impl std::fmt::Debug for ResolvedCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedCredential")
            .field("material", &"[REDACTED]")
            .field("context_id", &self.context_id)
            .finish()
    }
}

/// The filesystem surface credential resolution reads through, so tests can
/// inject a fake instead of touching the real home directory.
pub trait CredentialFs {
    fn read_to_string(&self, path: &Path) -> io::Result<String>;
    fn modified(&self, path: &Path) -> io::Result<SystemTime>;
}

/// The real filesystem.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealFs;

impl CredentialFs for RealFs {
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn modified(&self, path: &Path) -> io::Result<SystemTime> {
        std::fs::metadata(path)?.modified()
    }
}

/// Resolves a configured account's credential source into authentication
/// material and the persistable context id.
///
/// `verbose` controls whether error messages name the full credential path or
/// only the file name: a full home-directory path in an error is a
/// machine-identifying detail nobody asked for at default verbosity.
pub fn resolve(
    account: &AccountConfig,
    fs: &dyn CredentialFs,
    verbose: bool,
) -> Result<ResolvedCredential, Error> {
    let source = CredentialSource::from_account(account)?;
    match source {
        CredentialSource::None => Ok(ResolvedCredential {
            material: Secret::new(AuthMaterial::new(String::new())),
            context_id: CredentialContextId::new("none"),
        }),
        CredentialSource::File { path } => {
            let material = fs.read_to_string(&path).map_err(|cause| {
                Error::AuthRequired(format!(
                    "account '{}': credential file '{}' could not be read: {cause}",
                    account.name,
                    source_label(&path, verbose)
                ))
            })?;
            if material.trim().is_empty() {
                return Err(Error::AuthRequired(format!(
                    "account '{}': credential file '{}' is empty",
                    account.name,
                    source_label(&path, verbose)
                )));
            }
            let modified = fs.modified(&path).map_err(|cause| {
                Error::AuthRequired(format!(
                    "account '{}': credential file '{}' metadata could not be read: {cause}",
                    account.name,
                    source_label(&path, verbose)
                ))
            })?;
            let context_id = CredentialContextId::new(format!(
                "file:{}:{}",
                path.display(),
                mtime_nanos(modified)
            ));
            Ok(ResolvedCredential {
                material: Secret::new(AuthMaterial::new(material)),
                context_id,
            })
        }
    }
}

/// The path as an error message should name it: the file name at default
/// verbosity, the full path when verbose diagnostics were requested.
fn source_label(path: &Path, verbose: bool) -> String {
    if verbose {
        path.display().to_string()
    } else {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string())
    }
}

/// The file's modification time as nanoseconds since the Unix epoch. A
/// pre-epoch timestamp is impossible for a credential file in practice and
/// maps to zero rather than failing the resolution.
fn mtime_nanos(modified: SystemTime) -> u128 {
    modified
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ExitClass;
    use std::collections::BTreeMap;
    use std::time::{Duration, UNIX_EPOCH};

    struct FakeFs(BTreeMap<PathBuf, (String, SystemTime)>);

    impl FakeFs {
        fn new() -> Self {
            Self(BTreeMap::new())
        }

        fn file(
            mut self,
            path: impl Into<PathBuf>,
            content: impl Into<String>,
            modified: SystemTime,
        ) -> Self {
            self.0.insert(path.into(), (content.into(), modified));
            self
        }
    }

    impl CredentialFs for FakeFs {
        fn read_to_string(&self, path: &Path) -> io::Result<String> {
            self.0
                .get(path)
                .map(|(content, _)| content.clone())
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file"))
        }

        fn modified(&self, path: &Path) -> io::Result<SystemTime> {
            self.0
                .get(path)
                .map(|(_, modified)| *modified)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file"))
        }
    }

    fn account(name: &str, kind: &str, detail: &str) -> AccountConfig {
        AccountConfig {
            name: name.to_string(),
            provider: "test-provider".to_string(),
            credential_kind: kind.to_string(),
            credential_detail: detail.to_string(),
        }
    }

    fn at(nanos: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_nanos(nanos)
    }

    #[test]
    fn file_kind_resolves_to_material_and_context_id() {
        let fs = FakeFs::new().file("creds.json", "the-token", at(1_000));
        let resolved = resolve(&account("primary", "file", "creds.json"), &fs, false).unwrap();

        assert_eq!(resolved.material.into_inner().as_str(), "the-token");
        assert_eq!(resolved.context_id.as_str(), "file:creds.json:1000");
    }

    #[test]
    fn none_kind_resolves_without_material() {
        let fs = FakeFs::new();
        let resolved = resolve(&account("codex", "", ""), &fs, false).unwrap();

        assert_eq!(resolved.material.into_inner().as_str(), "");
        assert_eq!(resolved.context_id.as_str(), "none");
    }

    #[test]
    fn explicit_none_kind_resolves_without_material() {
        let fs = FakeFs::new();
        let resolved = resolve(&account("codex", "none", ""), &fs, false).unwrap();

        assert_eq!(resolved.context_id.as_str(), "none");
    }

    #[test]
    fn unsupported_kind_fails_explicitly_naming_account_and_kind() {
        let err = resolve(
            &account("primary", "profile", "work-primary"),
            &FakeFs::new(),
            false,
        )
        .unwrap_err();

        let message = err.to_string();
        assert!(message.contains("primary"), "{message}");
        assert!(message.contains("profile"), "{message}");
        assert_eq!(err.exit_class(), ExitClass::Usage);
    }

    #[test]
    fn a_credential_table_without_a_kind_fails_explicitly() {
        let err =
            resolve(&account("primary", "", "creds.json"), &FakeFs::new(), false).unwrap_err();

        let message = err.to_string();
        assert!(message.contains("primary"), "{message}");
        assert!(message.contains("creds.json"), "{message}");
        assert_eq!(err.exit_class(), ExitClass::Usage);
    }

    #[test]
    fn file_kind_without_a_path_fails_explicitly() {
        let err = resolve(&account("primary", "file", ""), &FakeFs::new(), false).unwrap_err();

        let message = err.to_string();
        assert!(message.contains("primary"), "{message}");
        assert_eq!(err.exit_class(), ExitClass::Usage);
    }

    #[test]
    fn empty_credential_file_fails_with_auth_required() {
        let fs = FakeFs::new().file("creds.json", "  \n", at(1_000));
        let err = resolve(&account("primary", "file", "creds.json"), &fs, false).unwrap_err();

        let message = err.to_string();
        assert!(message.contains("primary"), "{message}");
        assert!(message.contains("creds.json"), "{message}");
        assert_eq!(err.exit_class(), ExitClass::AuthRequired);
    }

    #[test]
    fn missing_credential_file_error_names_the_file_not_the_home_path() {
        let err = resolve(
            &account("primary", "file", "/home/someone/.config/creds.json"),
            &FakeFs::new(),
            false,
        )
        .unwrap_err();

        let message = err.to_string();
        assert!(message.contains("primary"), "{message}");
        assert!(message.contains("creds.json"), "{message}");
        assert!(!message.contains("/home/someone"), "{message}");
        assert_eq!(err.exit_class(), ExitClass::AuthRequired);
    }

    #[test]
    fn verbose_errors_name_the_full_path() {
        let err = resolve(
            &account("primary", "file", "/home/someone/.config/creds.json"),
            &FakeFs::new(),
            true,
        )
        .unwrap_err();

        let message = err.to_string();
        assert!(
            message.contains("/home/someone/.config/creds.json"),
            "{message}"
        );
    }

    #[test]
    fn context_id_is_stable_across_re_reads() {
        let fs = FakeFs::new().file("creds.json", "token-a", at(1_000));
        let first = resolve(&account("primary", "file", "creds.json"), &fs, false).unwrap();
        let second = resolve(&account("primary", "file", "creds.json"), &fs, false).unwrap();

        assert_eq!(first.context_id, second.context_id);
    }

    #[test]
    fn context_id_changes_after_credential_replacement() {
        let before = resolve(
            &account("primary", "file", "creds.json"),
            &FakeFs::new().file("creds.json", "token-a", at(1_000)),
            false,
        )
        .unwrap();
        let after = resolve(
            &account("primary", "file", "creds.json"),
            &FakeFs::new().file("creds.json", "token-b", at(2_000)),
            false,
        )
        .unwrap();

        assert_ne!(before.context_id, after.context_id);
    }

    #[test]
    fn context_id_never_contains_credential_bytes() {
        let fs = FakeFs::new().file("creds.json", "super-secret-token", at(1_000));
        let resolved = resolve(&account("primary", "file", "creds.json"), &fs, false).unwrap();

        assert!(!resolved.context_id.as_str().contains("super-secret-token"));
    }

    #[test]
    fn context_id_differs_across_distinct_sources() {
        let first = resolve(
            &account("primary", "file", "creds-a.json"),
            &FakeFs::new().file("creds-a.json", "token-a", at(1_000)),
            false,
        )
        .unwrap();
        let second = resolve(
            &account("primary", "file", "creds-b.json"),
            &FakeFs::new().file("creds-b.json", "token-b", at(1_000)),
            false,
        )
        .unwrap();

        assert_ne!(first.context_id, second.context_id);
    }
}
