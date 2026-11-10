//! HTTP client for the Gmail API: bearer auth, retries with backoff on 429
//! and 5xx. Only the hardcoded Google hosts are ever contacted: no proxies,
//! no redirects, HTTPS only.

use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use ureq::Agent;

use crate::auth::{oauth, store};
use crate::error::{Error, Result};

pub const GMAIL_BASE: &str = "https://gmail.googleapis.com/gmail/v1/users/me";

const MAX_ATTEMPTS: u32 = 4;
/// Attachments can be 25 MB, which is ~34 MB as base64 inside JSON.
const MAX_BODY: u64 = 64 * 1024 * 1024;

pub fn agent() -> Agent {
    Agent::config_builder()
        .https_only(true)
        .max_redirects(0)
        .proxy(None)
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(120)))
        .build()
        .into()
}

/// An authorized Gmail client. The access token lives only in memory.
pub struct Gmail {
    agent: Agent,
    token: String,
}

#[derive(Deserialize)]
struct GoogleError {
    error: GoogleErrorBody,
}

#[derive(Deserialize)]
struct GoogleErrorBody {
    message: Option<String>,
}

impl Gmail {
    pub fn new(agent: Agent, token: String) -> Self {
        Gmail { agent, token }
    }

    /// Loads the client and refresh token and gets a fresh access token.
    pub fn connect() -> Result<Gmail> {
        let client = store::load_client()?;
        let account = store::load_config()?
            .account
            .ok_or_else(|| Error::auth("not logged in").hint("run `mg auth login`"))?;
        let refresh = store::get_refresh_token(&account)?.ok_or_else(|| {
            Error::auth(format!("no refresh token in the keyring for {account}"))
                .hint("run `mg auth login`")
        })?;
        let agent = agent();
        let token = oauth::refresh(&agent, &client, &refresh)?;
        Ok(Gmail::new(agent, token))
    }

    /// GET `GMAIL_BASE + path`. Ids in `path` must already be validated.
    pub fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<T> {
        let url = format!("{GMAIL_BASE}{path}");
        let auth = format!("Bearer {}", self.token);
        let mut attempt = 1;
        loop {
            let result = self
                .agent
                .get(&url)
                .header("Authorization", &auth)
                .query_pairs(query.iter().copied())
                .call();
            let mut resp = match result {
                Ok(resp) => resp,
                Err(_) if attempt < MAX_ATTEMPTS => {
                    backoff(attempt);
                    attempt += 1;
                    continue;
                }
                Err(e) => return Err(Error::api(format!("network error: {e}"))),
            };
            let status = resp.status().as_u16();
            if (status == 429 || status >= 500) && attempt < MAX_ATTEMPTS {
                backoff(attempt);
                attempt += 1;
                continue;
            }
            if !(200..300).contains(&status) {
                let body = resp
                    .body_mut()
                    .with_config()
                    .limit(64 * 1024)
                    .read_to_string()
                    .unwrap_or_default();
                return Err(api_failure(status, &body));
            }
            return resp
                .body_mut()
                .with_config()
                .limit(MAX_BODY)
                .read_json()
                .map_err(|e| Error::api(format!("reading the Gmail response: {e}")));
        }
    }
}

fn api_failure(status: u16, body: &str) -> Error {
    let message = serde_json::from_str::<GoogleError>(body)
        .ok()
        .and_then(|e| e.error.message)
        .unwrap_or_else(|| format!("HTTP {status}"));
    let err = match status {
        401 => Error::auth(format!("Gmail rejected the access token: {message}"))
            .hint("run `mg auth login`"),
        403 => Error::api(format!("Gmail API: {message}"))
            .hint("is the Gmail API enabled in the Google Cloud project?"),
        404 => Error::api(format!("not found: {message}")).hint("check the id"),
        429 => Error::api(format!("rate limited: {message}")).hint("wait a minute and retry"),
        _ => Error::api(format!("Gmail API error {status}: {message}")),
    };
    err.status(status)
}

/// Exponential backoff with jitter: ~0.5s, 1s, 2s (capped at 8s).
fn backoff(attempt: u32) {
    let base = (250u64 << attempt).min(8000);
    let mut b = [0u8; 2];
    let _ = getrandom::fill(&mut b);
    let jitter = u64::from(u16::from_le_bytes(b)) % 250;
    std::thread::sleep(Duration::from_millis(base + jitter));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_api_errors() {
        let body = r#"{"error":{"code":404,"message":"Requested entity was not found.","status":"NOT_FOUND"}}"#;
        let e = api_failure(404, body);
        assert_eq!(e.status, Some(404));
        assert_eq!(e.exit_code(), 3);
        assert!(e.message.contains("Requested entity was not found."));
        assert_eq!(api_failure(401, "").exit_code(), 2);
        assert!(api_failure(500, "<html>").message.contains("500"));
    }
}
