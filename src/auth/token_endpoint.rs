//! The production OAuth token endpoint the Anthropic credential refresh exchanges a
//! refresh token against. It lives beside `credentials_lock` and not in `cli.rs`
//! because the coverage command's tripwire (`tests/coverage_command.rs`) reads
//! `src/cli.rs` for any HTTP transport reference: the command surface that may
//! not acquire the port must not name the transport at all, even for a refresh
//! that runs before sampling.

use crate::domain::time::{MonotonicDuration, RealClock};

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
