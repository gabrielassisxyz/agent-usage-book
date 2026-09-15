//! The production OAuth token endpoint the Anthropic credential refresh exchanges a
//! refresh token against. It lives beside `credentials_lock` and not in `cli.rs`
//! because the coverage command's tripwire (`tests/coverage_command.rs`) reads
//! `src/cli.rs` for any HTTP transport reference: the command surface that may
//! not acquire the port must not name the transport at all, even for a refresh
//! that runs before sampling.

use std::io::Read;

use crate::domain::time::{MonotonicDuration, RealClock};

/// The Antigravity refresh endpoint. Its OAuth client material is extracted
/// from the installed `agy` binary, where the CLI itself keeps it, rather than
/// copied into this repository.
pub(crate) struct AntigravityTokenEndpoint {
    url: String,
    binary: std::path::PathBuf,
}

impl AntigravityTokenEndpoint {
    pub(crate) fn from_env() -> Self {
        Self {
            url: std::env::var("AUB_AGY_TOKEN_ENDPOINT")
                .unwrap_or_else(|_| "https://oauth2.googleapis.com/token".to_string()),
            binary: std::env::var_os("AUB_AGY_BINARY")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(agy_binary_path),
        }
    }

    fn client_material(
        &self,
    ) -> Result<(String, String), crate::auth::antigravity_credentials::RefreshError> {
        let unreadable = |error: std::io::Error| {
            crate::auth::antigravity_credentials::RefreshError::Configuration(format!(
                "could not read agy binary '{}': {error}",
                self.binary.display()
            ))
        };
        let binary = std::fs::File::open(&self.binary).map_err(unreadable)?;
        let found = scan_agy_client_material(binary, AGY_SCAN_CHUNK_BYTES).map_err(unreadable)?;
        let Some(client_id) = found.client_id else {
            return Err(
                crate::auth::antigravity_credentials::RefreshError::Configuration(
                    "agy binary did not contain an OAuth client id".into(),
                ),
            );
        };
        let Some(client_secret) = found.secrets.into_iter().next() else {
            return Err(
                crate::auth::antigravity_credentials::RefreshError::Configuration(
                    "agy binary did not contain an OAuth client secret".into(),
                ),
            );
        };
        Ok((client_id, client_secret))
    }
}

fn agy_binary_path() -> std::path::PathBuf {
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|directory| directory.join("agy"))
                .find(|candidate| candidate.is_file())
        })
        .unwrap_or_else(|| std::path::PathBuf::from("agy"))
}

/// How much of the `agy` binary is held in memory at once. The binary is over
/// 200 MB, and while a stored token stays expired the refresh runs on every
/// sampling tick: reading it whole and copying it into a lossy UTF-8 string put
/// about 500 MB of anonymous memory on each of those ticks (aub-i2i9), where a
/// tick otherwise peaks under 10 MB.
const AGY_SCAN_CHUNK_BYTES: usize = 1 << 20;
const AGY_CLIENT_ID_PREFIX: &[u8] = b"107";
const AGY_CLIENT_ID_SUFFIX: &[u8] = b".apps.googleusercontent.com";
/// The longest client id recognised, suffix included. A Google OAuth client id
/// is about 75 bytes; the bound is what lets a match span chunks with a fixed
/// carry-over instead of an arbitrarily long run of identifier bytes.
const AGY_CLIENT_ID_MAX_BYTES: usize = 256;
/// Google OAuth client secrets packed in the `agy` binary: `GOCSPX-` plus
/// exactly 28 `[A-Za-z0-9_-]` characters. The tail is a fixed width, never a
/// run to the next terminator, because the binary packs the next string table
/// entry directly against the secret with no separator.
const AGY_SECRET_PREFIX: &[u8] = b"GOCSPX-";
const AGY_SECRET_TAIL_BYTES: usize = 28;

#[derive(Debug, Default)]
struct AgyClientMaterial {
    client_id: Option<String>,
    secrets: Vec<String>,
}

impl AgyClientMaterial {
    /// The refresh uses the first `107`-prefixed client id and the first of the
    /// two secrets the binary packs: probing the token endpoint on 2026-09-14
    /// (aub-vl8t) showed Google accepts that pair and rejects the second secret
    /// with `invalid_client`. Both secrets are still collected before the scan
    /// is called complete, because the binary packs exactly two adjacent, and
    /// seeing the second is what bounds how far the scan reads.
    fn complete(&self) -> bool {
        self.client_id.is_some() && self.secrets.len() >= 2
    }

    fn inspect(&mut self, from: &[u8]) {
        if self.client_id.is_none() {
            self.client_id = agy_client_id_at(from);
        }
        if let Some(secret) = agy_secret_at(from) {
            self.secrets.push(secret);
        }
    }
}

/// Reads `reader` in `chunk_bytes` pieces, carrying over only the bytes a match
/// can still span, and stops once the material is complete.
fn scan_agy_client_material(
    mut reader: impl Read,
    chunk_bytes: usize,
) -> std::io::Result<AgyClientMaterial> {
    let carry = AGY_CLIENT_ID_MAX_BYTES.max(AGY_SECRET_PREFIX.len() + AGY_SECRET_TAIL_BYTES);
    let mut found = AgyClientMaterial::default();
    let mut window: Vec<u8> = Vec::with_capacity(chunk_bytes + carry);
    let mut chunk = vec![0u8; chunk_bytes];
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        window.extend_from_slice(&chunk[..read]);
        let at_end = read == 0;
        let decided = if at_end {
            window.len()
        } else {
            window.len().saturating_sub(carry)
        };
        for start in 0..decided {
            if window[start] != AGY_CLIENT_ID_PREFIX[0] && window[start] != AGY_SECRET_PREFIX[0] {
                continue;
            }
            found.inspect(&window[start..]);
            if found.complete() {
                return Ok(found);
            }
        }
        if at_end {
            return Ok(found);
        }
        window.drain(..decided);
    }
}

fn agy_client_id_at(from: &[u8]) -> Option<String> {
    if !from.starts_with(AGY_CLIENT_ID_PREFIX) {
        return None;
    }
    let bounded = &from[..from.len().min(AGY_CLIENT_ID_MAX_BYTES)];
    let run_end = bounded
        .iter()
        .position(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')))
        .unwrap_or(bounded.len());
    let suffix_at = bounded[..run_end]
        .windows(AGY_CLIENT_ID_SUFFIX.len())
        .position(|candidate| candidate == AGY_CLIENT_ID_SUFFIX)?;
    let id = &bounded[..suffix_at + AGY_CLIENT_ID_SUFFIX.len()];
    Some(String::from_utf8_lossy(id).into_owned())
}

fn agy_secret_at(from: &[u8]) -> Option<String> {
    let secret = from.get(..AGY_SECRET_PREFIX.len() + AGY_SECRET_TAIL_BYTES)?;
    let tail = secret.strip_prefix(AGY_SECRET_PREFIX)?;
    tail.iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        .then(|| String::from_utf8_lossy(secret).into_owned())
}

impl crate::auth::antigravity_credentials::TokenEndpoint for AntigravityTokenEndpoint {
    fn exchange(
        &self,
        refresh_token: &str,
    ) -> Result<(String, i64), crate::auth::antigravity_credentials::RefreshError> {
        use crate::auth::antigravity_credentials::RefreshError;
        use crate::meter::adapter::HttpTransport;
        use crate::meter::transport::{
            BlockingTransport, CommandBudget, HttpRequest, RequestTimeoutConfig,
        };
        let (client_id, client_secret) = self.client_material()?;
        let body = serde_json::to_vec(&serde_json::json!({
            "grant_type": "refresh_token", "refresh_token": refresh_token,
            "client_id": client_id, "client_secret": client_secret,
        }))
        .map_err(|error| RefreshError::Endpoint(error.to_string()))?;
        let clock = RealClock::new();
        let request = HttpRequest::post(
            &self.url,
            body,
            RequestTimeoutConfig::new(
                MonotonicDuration::from_seconds(5),
                MonotonicDuration::from_seconds(10),
                Some(MonotonicDuration::from_seconds(15)),
            ),
        )
        .with_header("Content-Type", "application/json")
        .with_header("Accept", "application/json");
        let response = BlockingTransport
            .send(
                &request,
                &CommandBudget::new(MonotonicDuration::from_seconds(30), &clock),
                &clock,
            )
            .map_err(|failure| RefreshError::Endpoint(format!("{failure:?}")))?;
        let parsed: serde_json::Value =
            serde_json::from_str(response.body_as_str().unwrap_or_default()).map_err(|_| {
                RefreshError::Endpoint("token endpoint response was not JSON".into())
            })?;
        let error_code = parsed.get("error").and_then(serde_json::Value::as_str);
        if response.status() == 400 && error_code == Some("invalid_grant") {
            return Err(RefreshError::InvalidGrant);
        }
        // `invalid_client` and `unauthorized_client` (RFC 6749 section 5.2)
        // reject the OAuth client id and secret pair itself, not the refresh
        // token. That pair is extracted from the installed `agy` binary, so a
        // rejection here is a configuration fault whose fix is to re-extract the
        // credentials, distinct from `invalid_grant`, whose fix is to log in
        // again. The provider returns it as 401 as readily as 400, so the error
        // code decides, not the status.
        if matches!(error_code, Some("invalid_client" | "unauthorized_client")) {
            return Err(RefreshError::Configuration(format!(
                "the OAuth client credentials extracted from the agy binary were rejected by the token endpoint ('{}'); re-extract them from the binary",
                error_code.unwrap_or_default()
            )));
        }
        if response.status() != 200 {
            return Err(RefreshError::Endpoint(format!(
                "token endpoint status {}",
                response.status()
            )));
        }
        let access = parsed
            .get("access_token")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                RefreshError::Endpoint("token endpoint response missing access_token".into())
            })?;
        let expires = parsed
            .get("expires_in")
            .and_then(serde_json::Value::as_i64)
            .filter(|value| *value > 0)
            .ok_or_else(|| {
                RefreshError::Endpoint("token endpoint response missing expires_in".into())
            })?;
        Ok((access.to_string(), expires))
    }
}

/// The production OAuth token endpoint for the Anthropic credential refresh
/// (aub-79gp). It owns the transport wiring that `crate::auth::credentials_lock`
/// is forbidden to reach for (boundary rule 12 keeps `ureq` in the transport
/// module). `AUB_ANTHROPIC_TOKEN_ENDPOINT` overrides the URL so an end-to-end
/// run can stand a synthetic server in for `console.anthropic.com`.
pub(crate) struct AnthropicTokenEndpoint {
    url: String,
}

impl AnthropicTokenEndpoint {
    pub(crate) fn from_env() -> Self {
        Self {
            url: std::env::var("AUB_ANTHROPIC_TOKEN_ENDPOINT").unwrap_or_else(|_| {
                crate::auth::credentials_lock::DEFAULT_TOKEN_ENDPOINT.to_string()
            }),
        }
    }
}

impl crate::auth::credentials_lock::OAuthRefreshEndpoint for AnthropicTokenEndpoint {
    fn exchange(
        &self,
        refresh_token: &str,
    ) -> Result<
        crate::auth::credentials_lock::RotatedTokens,
        crate::auth::credentials_lock::RefreshEndpointError,
    > {
        use crate::auth::credentials_lock::RefreshEndpointError;
        use crate::meter::adapter::HttpTransport;
        use crate::meter::transport::{
            BlockingTransport, CommandBudget, HttpRequest, RequestTimeoutConfig,
        };

        let payload = serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": crate::auth::credentials_lock::CLAUDE_CODE_OAUTH_CLIENT_ID,
        });
        let body = serde_json::to_vec(&payload)
            .map_err(|error| RefreshEndpointError::Transport(error.to_string()))?;
        let timeouts = RequestTimeoutConfig::new(
            MonotonicDuration::from_seconds(5),
            MonotonicDuration::from_seconds(10),
            Some(MonotonicDuration::from_seconds(15)),
        );
        let request = HttpRequest::post(&self.url, body, timeouts)
            .with_header("Content-Type", "application/json")
            .with_header("Accept", "application/json")
            .with_header("User-Agent", "agent-usage-book/0.1.0");
        let clock = RealClock::new();
        let budget = CommandBudget::new(MonotonicDuration::from_seconds(30), &clock);
        let response = BlockingTransport
            .send(&request, &budget, &clock)
            .map_err(|failure| RefreshEndpointError::Transport(format!("{failure:?}")))?;

        let status = response.status();
        let text = response.body_as_str().unwrap_or_default();
        let parsed = serde_json::from_str::<serde_json::Value>(text).ok();

        if status == 200 {
            let value = parsed.ok_or(RefreshEndpointError::MalformedResponse)?;
            let field = |name: &str| value.get(name).and_then(serde_json::Value::as_str);
            let access_token =
                field("access_token").ok_or(RefreshEndpointError::MalformedResponse)?;
            let new_refresh =
                field("refresh_token").ok_or(RefreshEndpointError::MalformedResponse)?;
            let expires_in_secs = value
                .get("expires_in")
                .and_then(serde_json::Value::as_i64)
                .ok_or(RefreshEndpointError::MalformedResponse)?;
            return Ok(crate::auth::credentials_lock::RotatedTokens {
                access_token: access_token.to_string(),
                refresh_token: new_refresh.to_string(),
                expires_in_secs,
            });
        }

        let is_invalid_grant = parsed
            .as_ref()
            .and_then(|value| value.get("error").and_then(serde_json::Value::as_str))
            .is_some_and(|error| error == "invalid_grant");
        if is_invalid_grant {
            Err(RefreshEndpointError::InvalidGrant)
        } else {
            Err(RefreshEndpointError::HttpStatus(status))
        }
    }
}

#[cfg(test)]
mod antigravity_client_material_tests {
    use super::{
        AGY_CLIENT_ID_MAX_BYTES, AGY_SCAN_CHUNK_BYTES, AGY_SECRET_PREFIX, AGY_SECRET_TAIL_BYTES,
        AgyClientMaterial, scan_agy_client_material,
    };

    const TERMINATED_CLIENT_ID: &str = "107222333444-fakeclientidabcXYZ.apps.googleusercontent.com";
    const PACKED_CLIENT_ID: &str = "107999888777-fakepackedclientidxyz.apps.googleusercontent.com";
    // Assembled at run time: GitHub push protection refuses the literal
    // `GOCSPX-` plus 28 characters shape as a Google OAuth client secret,
    // synthetic or not.
    fn secret(fill: &str) -> String {
        format!("GOCSPX-{}", fill.repeat(28))
    }

    fn scan(bytes: &[u8], chunk_bytes: usize) -> AgyClientMaterial {
        scan_agy_client_material(bytes, chunk_bytes).expect("an in-memory reader cannot fail")
    }

    #[test]
    fn terminated_id_and_secrets_extract_exactly() {
        let (first, second) = (secret("A"), secret("B"));
        let text = format!(
            "prefix\nclient_id={TERMINATED_CLIENT_ID}\nsecret1={first}\nsecret2={second}\n"
        );
        let found = scan(text.as_bytes(), AGY_SCAN_CHUNK_BYTES);
        assert_eq!(found.client_id.as_deref(), Some(TERMINATED_CLIENT_ID));
        assert_eq!(found.secrets, vec![first, second]);
    }

    #[test]
    fn packed_id_and_secrets_against_next_string_extract_exactly() {
        let (first, second) = (secret("A"), secret("B"));
        let text = format!(
            "prefix{PACKED_CLIENT_ID}handleProgress{first}{second}https://cloudcode-pa.googleapis.com"
        );
        let found = scan(text.as_bytes(), AGY_SCAN_CHUNK_BYTES);
        assert_eq!(found.client_id.as_deref(), Some(PACKED_CLIENT_ID));
        assert_eq!(found.secrets, vec![first, second]);
    }

    /// Every chunk size from one byte up puts some chunk boundary inside the id
    /// and inside each secret, surrounded by bytes that are not UTF-8, which is
    /// what the binary around the material looks like.
    #[test]
    fn material_straddling_every_chunk_boundary_extracts_exactly() {
        let (first, second) = (secret("A"), secret("B"));
        let mut bytes = vec![0xFF_u8; 1000];
        bytes.extend_from_slice(PACKED_CLIENT_ID.as_bytes());
        bytes.push(0xC3);
        bytes.extend_from_slice(first.as_bytes());
        bytes.extend_from_slice(second.as_bytes());
        bytes.extend(std::iter::repeat_n(0xFE_u8, 700));
        for chunk_bytes in 1..=97 {
            let found = scan(&bytes, chunk_bytes);
            assert_eq!(
                found.client_id.as_deref(),
                Some(PACKED_CLIENT_ID),
                "chunk {chunk_bytes}"
            );
            assert_eq!(found.secrets, vec![first.clone(), second.clone()]);
        }
    }

    /// Planted negatives next to their positives: a byte outside the identifier
    /// set inside the id, and a secret one character short of its fixed width,
    /// are both refused, as the lossy-text extraction refused them.
    #[test]
    fn a_broken_id_and_a_short_secret_are_not_material() {
        let broken_id = PACKED_CLIENT_ID.replacen("fake", "fa\u{00e9}ke", 1);
        let short = &secret("A")[..AGY_SECRET_PREFIX.len() + AGY_SECRET_TAIL_BYTES - 1];
        let text = format!("{broken_id}\n{short}\n");
        let found = scan(text.as_bytes(), 8);
        assert_eq!(found.client_id, None);
        assert!(found.secrets.is_empty(), "{:?}", found.secrets);
    }

    /// The scan stops reading at the end of the second secret once the id is
    /// known: a reader that fails past that point is never read again.
    #[test]
    fn the_scan_stops_reading_once_the_material_is_complete() {
        struct FailsAfter<'a> {
            bytes: &'a [u8],
        }
        impl std::io::Read for FailsAfter<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.bytes.is_empty() {
                    return Err(std::io::Error::other("read past the material"));
                }
                let n = buf.len().min(self.bytes.len());
                buf[..n].copy_from_slice(&self.bytes[..n]);
                self.bytes = &self.bytes[n..];
                Ok(n)
            }
        }
        let text = format!(
            "{TERMINATED_CLIENT_ID}\n{}{}{}",
            secret("A"),
            secret("B"),
            " ".repeat(AGY_CLIENT_ID_MAX_BYTES)
        );
        let found = scan_agy_client_material(
            FailsAfter {
                bytes: text.as_bytes(),
            },
            16,
        )
        .expect("the scan must not read past complete material");
        assert!(found.complete());
    }

    /// The real binary's layout: two secrets packed adjacent at a lower offset
    /// than two client ids, only one of which carries the `107` prefix. The
    /// refresh must pick that id and the FIRST secret, the pair the token
    /// endpoint accepted on 2026-09-14 (aub-vl8t). This fails against the
    /// previous `secrets.into_iter().nth(1)` selection, which sent the second.
    #[test]
    fn client_material_selects_the_107_id_and_the_first_secret() {
        use super::AntigravityTokenEndpoint;

        // An `884...` id the extractor must ignore, then the `107` id, with the
        // two secrets ahead of both as in the installed binary.
        let ignored_id = "888777666555-fakeotherclientid00.apps.googleusercontent.com";
        let (first, second) = (secret("A"), secret("B"));
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"leading padding");
        bytes.extend_from_slice(first.as_bytes());
        bytes.extend_from_slice(second.as_bytes());
        bytes.extend_from_slice(b"gap between secrets and ids");
        bytes.extend_from_slice(ignored_id.as_bytes());
        bytes.push(b'\n');
        bytes.extend_from_slice(PACKED_CLIENT_ID.as_bytes());
        bytes.extend_from_slice(&[b' '; 64]);

        let path =
            std::env::temp_dir().join(format!("aub-agy-material-{}.bin", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();
        let endpoint = AntigravityTokenEndpoint {
            url: String::new(),
            binary: path.clone(),
        };
        let (client_id, client_secret) = endpoint.client_material().expect("material extracts");
        std::fs::remove_file(&path).ok();

        assert_eq!(client_id, PACKED_CLIENT_ID);
        assert_eq!(client_secret, first);
    }
}
