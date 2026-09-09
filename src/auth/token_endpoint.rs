//! The production OAuth token endpoint the Anthropic credential refresh exchanges a
//! refresh token against. It lives beside `credentials_lock` and not in `cli.rs`
//! because the coverage command's tripwire (`tests/coverage_command.rs`) reads
//! `src/cli.rs` for any HTTP transport reference: the command surface that may
//! not acquire the port must not name the transport at all, even for a refresh
//! that runs before sampling.

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
        let bytes = std::fs::read(&self.binary).map_err(|error| {
            crate::auth::antigravity_credentials::RefreshError::Configuration(format!(
                "could not read agy binary '{}': {error}",
                self.binary.display()
            ))
        })?;
        let text = String::from_utf8_lossy(&bytes);
        let ids = strings_with_prefix(&text, "107", ".apps.googleusercontent.com");
        let secrets = strings_with_prefix(&text, "GOCSPX-", "");
        let Some(client_id) = ids.first() else {
            return Err(
                crate::auth::antigravity_credentials::RefreshError::Configuration(
                    "agy binary did not contain an OAuth client id".into(),
                ),
            );
        };
        let Some(client_secret) = secrets.get(1) else {
            return Err(
                crate::auth::antigravity_credentials::RefreshError::Configuration(
                    "agy binary did not contain the expected second OAuth client secret".into(),
                ),
            );
        };
        Ok((client_id.clone(), client_secret.clone()))
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

fn strings_with_prefix(text: &str, prefix: &str, suffix: &str) -> Vec<String> {
    text.match_indices(prefix)
        .filter_map(|(start, _)| {
            let value: String = text[start..]
                .chars()
                .take_while(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
                })
                .collect();
            value.ends_with(suffix).then_some(value)
        })
        .collect()
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
