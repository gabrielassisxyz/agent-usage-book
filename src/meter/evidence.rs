//! Provider-response evidence captured before semantic normalization.
//!
//! A capsule contains only the sanitized JSON returned by a provider's quota
//! endpoint, its scalar source lexemes, and the hash of the original body. It
//! deliberately excludes request and response headers and never embeds a raw
//! response body. On a schema failure, [`CapturedProviderResponse::failed_body`]
//! carries the sanitized JSON body separately, for a later bead's bounded,
//! count-limited store (`aub-2r3`); it is `None` on every successful response.

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};

use crate::meter::adapter::ProviderObservation;
use crate::meter::transport::HttpResponse;

pub const JSON_CAPSULE_SCHEMA_VERSION: &str = "json-quota-capsule-v1";
pub const JSON_SANITIZER_VERSION: &str = "sensitive-json-v1";
const MAX_CAPSULE_BYTES: usize = 256 * 1024;

/// Secret material already known to the caller and therefore removable even
/// when a provider echoes it under an otherwise innocuous field name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SensitiveResponseMaterial {
    values: Vec<String>,
}

impl SensitiveResponseMaterial {
    pub fn new(values: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let mut values = values
            .into_iter()
            .map(Into::into)
            .filter(|value: &String| !value.is_empty())
            .collect::<Vec<_>>();
        values.sort();
        values.dedup();
        Self { values }
    }

    fn contains_known_secret(&self, value: &str) -> bool {
        self.values.iter().any(|secret| value.contains(secret))
    }
}

/// A canonical, sanitized response capsule ready for durable persistence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonEvidenceCapsule {
    serialized: String,
    body_hash: String,
    capture_truncated: bool,
    sanitized_body: Option<Vec<u8>>,
}

/// One adapter result with the response evidence kept separate from its
/// semantic interpretation. Pre-response failures have no evidence capsule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedProviderResponse<T> {
    pub observation: ProviderObservation<T>,
    pub evidence: Option<JsonEvidenceCapsule>,
    pub failed_body: Option<Vec<u8>>,
}

impl<T> CapturedProviderResponse<T> {
    pub fn without_response(observation: ProviderObservation<T>) -> Self {
        Self {
            observation,
            evidence: None,
            failed_body: None,
        }
    }
}

impl JsonEvidenceCapsule {
    pub fn serialized(&self) -> &str {
        &self.serialized
    }

    pub fn body_hash(&self) -> &str {
        &self.body_hash
    }

    pub const fn schema_version(&self) -> &'static str {
        JSON_CAPSULE_SCHEMA_VERSION
    }

    pub const fn sanitizer_version(&self) -> &'static str {
        JSON_SANITIZER_VERSION
    }

    pub const fn capture_truncated(&self) -> bool {
        self.capture_truncated
    }

    /// The complete sanitized JSON body, available for the failure-only
    /// circular buffer. A malformed or non-JSON body has no safely structured
    /// body to retain and is represented by its hash alone.
    pub fn sanitized_body_for_failure(&self) -> Option<&[u8]> {
        self.sanitized_body.as_deref()
    }
}

/// Captures only the response body. Headers are accepted as part of the
/// `HttpResponse` input so tests can prove they never enter the capsule.
pub fn capture_json_response(
    response: &HttpResponse,
    sensitive: &SensitiveResponseMaterial,
) -> JsonEvidenceCapsule {
    capture_json_body(response.body(), sensitive)
}

pub fn capture_json_body(
    body: &[u8],
    sensitive: &SensitiveResponseMaterial,
) -> JsonEvidenceCapsule {
    let body_hash = sha256_hex(body);
    let Ok(mut quota_response) = serde_json::from_slice::<serde_json::Value>(body) else {
        return JsonEvidenceCapsule {
            serialized: minimal_capsule(&body_hash, false),
            body_hash,
            capture_truncated: false,
            sanitized_body: None,
        };
    };

    let mut removed_prefixes = BTreeSet::new();
    let mut redacted_paths = BTreeSet::new();
    sanitize_value(
        &mut quota_response,
        "",
        sensitive,
        &mut removed_prefixes,
        &mut redacted_paths,
    );

    let sanitized_body = serde_json::to_vec(&quota_response).ok();
    let mut lexemes = scan_scalar_lexemes(body).unwrap_or_default();
    lexemes.retain(|path, lexeme| {
        if removed_prefixes
            .iter()
            .any(|prefix| path == prefix || path.starts_with(&format!("{prefix}/")))
        {
            return false;
        }
        if redacted_paths.contains(path) {
            *lexeme = serde_json::to_string("[REDACTED]").expect("a string always serializes");
        }
        true
    });

    let mut serialized = serialize_capsule(&body_hash, quota_response, lexemes);
    let capture_truncated = serialized.len() > MAX_CAPSULE_BYTES;
    if capture_truncated {
        serialized = minimal_capsule(&body_hash, true);
    }

    JsonEvidenceCapsule {
        serialized,
        body_hash,
        capture_truncated,
        sanitized_body: if capture_truncated {
            None
        } else {
            sanitized_body
        },
    }
}

/// Reads the sanitized quota subtree back from a stored capsule. Adapters use
/// this function for replay, so corrected semantics operate on retained
/// evidence rather than on an unavailable HTTP response.
pub fn quota_response_from_capsule(capsule: &str) -> Result<serde_json::Value, &'static str> {
    let value: serde_json::Value =
        serde_json::from_str(capsule).map_err(|_| "capsule is not valid JSON")?;
    value
        .get("quota_response")
        .cloned()
        .filter(|value| !value.is_null())
        .ok_or("capsule does not contain a quota response")
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn serialize_capsule(
    body_hash: &str,
    quota_response: serde_json::Value,
    lexemes: BTreeMap<String, String>,
) -> String {
    let raw_lexemes = lexemes
        .into_iter()
        .map(|(path, lexeme)| (path, serde_json::Value::String(lexeme)))
        .collect::<serde_json::Map<_, _>>();
    let value = serde_json::json!({
        "body_hash_sha256": body_hash,
        "quota_response": quota_response,
        "raw_lexemes": raw_lexemes,
    });
    serde_json::to_string(&value).expect("a JSON value always serializes")
}

fn minimal_capsule(body_hash: &str, truncated: bool) -> String {
    serde_json::to_string(&serde_json::json!({
        "body_hash_sha256": body_hash,
        "capture_truncated": truncated,
        "quota_response": serde_json::Value::Null,
        "raw_lexemes": {},
    }))
    .expect("a JSON value always serializes")
}

fn sanitize_value(
    value: &mut serde_json::Value,
    path: &str,
    sensitive: &SensitiveResponseMaterial,
    removed_prefixes: &mut BTreeSet<String>,
    redacted_paths: &mut BTreeSet<String>,
) {
    match value {
        serde_json::Value::Object(object) => {
            let keys = object.keys().cloned().collect::<Vec<_>>();
            for key in keys {
                let child_path = join_pointer(path, &key);
                if sensitive_key(&key) {
                    object.remove(&key);
                    removed_prefixes.insert(child_path);
                } else if let Some(child) = object.get_mut(&key) {
                    sanitize_value(
                        child,
                        &child_path,
                        sensitive,
                        removed_prefixes,
                        redacted_paths,
                    );
                }
            }
        }
        serde_json::Value::Array(array) => {
            for (index, child) in array.iter_mut().enumerate() {
                sanitize_value(
                    child,
                    &join_pointer(path, &index.to_string()),
                    sensitive,
                    removed_prefixes,
                    redacted_paths,
                );
            }
        }
        serde_json::Value::String(text) if sensitive_value(text, sensitive) => {
            *text = "[REDACTED]".to_owned();
            redacted_paths.insert(path.to_owned());
        }
        serde_json::Value::String(_)
        | serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_) => {}
    }
}

fn sensitive_key(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    ["authorization", "cookie", "token", "credential"]
        .iter()
        .any(|needle| normalized.contains(needle))
}

fn sensitive_value(value: &str, sensitive: &SensitiveResponseMaterial) -> bool {
    let lowercase = value.to_ascii_lowercase();
    sensitive.contains_known_secret(value)
        || lowercase.starts_with("bearer ")
        || lowercase.starts_with("basic ")
        || lowercase.contains("sk-ant-")
        || lowercase.contains("session_token=")
}

fn join_pointer(parent: &str, segment: &str) -> String {
    let escaped = segment.replace('~', "~0").replace('/', "~1");
    format!("{parent}/{escaped}")
}

fn scan_scalar_lexemes(body: &[u8]) -> Result<BTreeMap<String, String>, ()> {
    let mut scanner = JsonLexemeScanner {
        body,
        cursor: 0,
        lexemes: BTreeMap::new(),
    };
    scanner.scan_value("")?;
    scanner.skip_whitespace();
    (scanner.cursor == body.len())
        .then_some(scanner.lexemes)
        .ok_or(())
}

struct JsonLexemeScanner<'a> {
    body: &'a [u8],
    cursor: usize,
    lexemes: BTreeMap<String, String>,
}

impl JsonLexemeScanner<'_> {
    fn scan_value(&mut self, path: &str) -> Result<(), ()> {
        self.skip_whitespace();
        match self.body.get(self.cursor) {
            Some(b'{') => self.scan_object(path),
            Some(b'[') => self.scan_array(path),
            Some(b'"') => self.scan_scalar_string(path),
            Some(_) => self.scan_unquoted_scalar(path),
            None => Err(()),
        }
    }

    fn scan_object(&mut self, path: &str) -> Result<(), ()> {
        self.cursor += 1;
        self.skip_whitespace();
        if self.take(b'}') {
            return Ok(());
        }
        loop {
            self.skip_whitespace();
            let key_start = self.cursor;
            self.scan_string_end()?;
            let key = serde_json::from_slice::<String>(&self.body[key_start..self.cursor])
                .map_err(|_| ())?;
            self.skip_whitespace();
            self.expect(b':')?;
            self.scan_value(&join_pointer(path, &key))?;
            self.skip_whitespace();
            if self.take(b'}') {
                return Ok(());
            }
            self.expect(b',')?;
        }
    }

    fn scan_array(&mut self, path: &str) -> Result<(), ()> {
        self.cursor += 1;
        self.skip_whitespace();
        if self.take(b']') {
            return Ok(());
        }
        let mut index = 0usize;
        loop {
            self.scan_value(&join_pointer(path, &index.to_string()))?;
            index += 1;
            self.skip_whitespace();
            if self.take(b']') {
                return Ok(());
            }
            self.expect(b',')?;
        }
    }

    fn scan_scalar_string(&mut self, path: &str) -> Result<(), ()> {
        let start = self.cursor;
        self.scan_string_end()?;
        self.record(path, start)
    }

    fn scan_unquoted_scalar(&mut self, path: &str) -> Result<(), ()> {
        let start = self.cursor;
        while let Some(byte) = self.body.get(self.cursor) {
            if byte.is_ascii_whitespace() || matches!(byte, b',' | b']' | b'}') {
                break;
            }
            self.cursor += 1;
        }
        (self.cursor > start).then_some(()).ok_or(())?;
        self.record(path, start)
    }

    fn scan_string_end(&mut self) -> Result<(), ()> {
        self.expect(b'"')?;
        let mut escaped = false;
        while let Some(byte) = self.body.get(self.cursor).copied() {
            self.cursor += 1;
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                return Ok(());
            }
        }
        Err(())
    }

    fn record(&mut self, path: &str, start: usize) -> Result<(), ()> {
        let lexeme = std::str::from_utf8(&self.body[start..self.cursor]).map_err(|_| ())?;
        self.lexemes.insert(path.to_owned(), lexeme.to_owned());
        Ok(())
    }

    fn skip_whitespace(&mut self) {
        while self
            .body
            .get(self.cursor)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.cursor += 1;
        }
    }

    fn expect(&mut self, expected: u8) -> Result<(), ()> {
        self.take(expected).then_some(()).ok_or(())
    }

    fn take(&mut self, expected: u8) -> bool {
        if self.body.get(self.cursor) == Some(&expected) {
            self.cursor += 1;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn response(body: impl Into<Vec<u8>>) -> HttpResponse {
        HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    #[test]
    fn canonical_capsule_preserves_scalar_source_lexemes() {
        let body = br#"{"b":41.00,"a":4.1e1,"escaped":"4\u002e1"}"#;
        let capsule = capture_json_response(
            &response(body.as_slice()),
            &SensitiveResponseMaterial::default(),
        );
        let parsed: serde_json::Value = serde_json::from_str(capsule.serialized()).unwrap();

        assert_eq!(parsed["quota_response"]["a"], serde_json::json!(41.0));
        assert_eq!(parsed["raw_lexemes"]["/a"], "4.1e1");
        assert_eq!(parsed["raw_lexemes"]["/b"], "41.00");
        assert_eq!(parsed["raw_lexemes"]["/escaped"], r#""4\u002e1""#);
        assert_eq!(capsule.body_hash(), sha256_hex(body));
        assert!(!capsule.capture_truncated());
    }

    #[test]
    fn malformed_json_retains_only_the_original_body_hash() {
        let body = br#"{"five_hour":{"utilization":41.0}"#;
        let capsule = capture_json_response(
            &response(body.as_slice()),
            &SensitiveResponseMaterial::default(),
        );
        let parsed: serde_json::Value = serde_json::from_str(capsule.serialized()).unwrap();

        assert_eq!(parsed["body_hash_sha256"], sha256_hex(body));
        assert!(parsed["quota_response"].is_null());
        assert!(capsule.sanitized_body_for_failure().is_none());
        assert!(!capsule.capture_truncated());
    }

    proptest! {
        #[test]
        fn response_capsule_excludes_headers_cookies_tokens_and_credential_content(
            authorization in "[A-Za-z0-9]{8,24}",
            cookie in "[A-Za-z0-9]{8,24}",
            token in "[A-Za-z0-9]{8,24}",
            credential_file in "[A-Za-z0-9]{8,24}",
        ) {
            let body = serde_json::json!({
                "five_hour": {"utilization": 8.0, "resets_at": "2026-08-30T19:00:00Z"},
                "seven_day": {"utilization": 91.0, "resets_at": "2026-09-06T12:00:00Z"},
                "authorization": authorization,
                "cookie": cookie,
                "future": {
                    "token": token,
                    "credential_file_content": credential_file,
                },
            });
            let mut synthetic_response = response(serde_json::to_vec(&body).unwrap());
            synthetic_response.headers = vec![
                ("Authorization".into(), authorization.clone()),
                ("Set-Cookie".into(), cookie.clone()),
            ];
            let sensitive = SensitiveResponseMaterial::new([
                authorization.clone(),
                cookie.clone(),
                token.clone(),
                credential_file.clone(),
            ]);

            let capsule = capture_json_response(&synthetic_response, &sensitive);
            let mut retained = capsule.serialized().as_bytes().to_vec();
            retained.extend_from_slice(capsule.sanitized_body_for_failure().unwrap_or_default());
            let retained = String::from_utf8(retained).unwrap();

            for forbidden in [&authorization, &cookie, &token, &credential_file] {
                prop_assert!(!retained.contains(forbidden));
            }
            prop_assert!(!retained.to_ascii_lowercase().contains("authorization"));
            prop_assert!(!retained.to_ascii_lowercase().contains("cookie"));
            prop_assert!(!retained.to_ascii_lowercase().contains("credential_file"));
        }
    }
}
