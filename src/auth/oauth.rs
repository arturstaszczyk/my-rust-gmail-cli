//! OAuth 2.0 for installed apps: PKCE S256, random state, a one-shot loopback
//! listener on 127.0.0.1, token exchange, refresh and revoke.

use std::io::{BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use ureq::Agent;

use crate::auth::store::Client;
use crate::error::{Error, Result};

pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const REVOKE_URL: &str = "https://oauth2.googleapis.com/revoke";
pub const SCOPE: &str = "https://www.googleapis.com/auth/gmail.modify";

const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// `n` random bytes as base64url without padding.
pub fn random_b64(n: usize) -> String {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).expect("OS random number generator failed");
    URL_SAFE_NO_PAD.encode(&buf)
}

/// RFC 7636 S256: base64url(sha256(verifier)).
pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()).as_slice())
}

/// Percent-encodes everything except RFC 3986 unreserved characters.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Decodes `application/x-www-form-urlencoded` text. `None` on bad escapes.
pub fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = s.get(i + 1..i + 3)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8(out).ok()
}

pub fn auth_url(client_id: &str, redirect_uri: &str, challenge: &str, state: &str) -> String {
    let params = [
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("response_type", "code"),
        ("scope", SCOPE),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("access_type", "offline"),
        ("prompt", "consent"),
    ];
    let query: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{k}={}", percent_encode(v)))
        .collect();
    format!("{AUTH_URL}?{}", query.join("&"))
}

/// Splits `GET /path?query HTTP/1.1` into path and query.
pub fn parse_request_line(line: &str) -> Option<(&str, &str)> {
    let mut parts = line.split(' ');
    if parts.next()? != "GET" {
        return None;
    }
    let target = parts.next()?;
    parts.next()?.strip_prefix("HTTP/")?;
    Some(target.split_once('?').unwrap_or((target, "")))
}

pub fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            Some((percent_decode(k)?, percent_decode(v)?))
        })
        .collect()
}

/// What a loopback request means for the login.
#[derive(Debug, PartialEq, Eq)]
pub enum Callback {
    /// Not the redirect (favicon, preconnect, junk): answer and keep waiting.
    Ignore,
    Code(String),
    Fail(String),
}

pub fn classify_callback(request_line: &str, expected_state: &str) -> Callback {
    let Some((path, query)) = parse_request_line(request_line) else {
        return Callback::Ignore;
    };
    if path != "/" {
        return Callback::Ignore;
    }
    let params = parse_query(query);
    let get = |key: &str| params.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
    if let Some(err) = get("error") {
        return Callback::Fail(format!("authorization denied: {err}"));
    }
    if get("state") != Some(expected_state) {
        return Callback::Fail("OAuth state mismatch; aborting for safety".into());
    }
    match get("code") {
        Some(code) if !code.is_empty() => Callback::Code(code.to_string()),
        _ => Callback::Fail("no authorization code in the redirect".into()),
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    scope: Option<String>,
}

#[derive(Deserialize)]
struct OAuthError {
    error: Option<String>,
    error_description: Option<String>,
}

pub struct Tokens {
    pub access: String,
    pub refresh: String,
}

/// Runs the browser flow and returns fresh tokens.
pub fn login(agent: &Agent, client: &Client) -> Result<Tokens> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| Error::io("binding the loopback listener", e))?;
    let port = listener
        .local_addr()
        .map_err(|e| Error::io("reading the loopback port", e))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}");
    let verifier = random_b64(32);
    let state = random_b64(16);
    let url = auth_url(&client.client_id, &redirect_uri, &pkce_challenge(&verifier), &state);

    eprintln!(
        "Opening your browser for Google sign-in.\n\
         If it doesn't open, visit this URL:\n\n{url}\n\n\
         Waiting up to 5 minutes..."
    );
    let _ = Command::new("xdg-open")
        .arg(&url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();

    let code = wait_for_code(&listener, &state)?;
    drop(listener);
    exchange_code(agent, client, &code, &verifier, &redirect_uri)
}

/// Headless flow: nothing listens. The browser (on any machine) fails to load
/// the loopback redirect, and the user pastes that URL back on stdin.
pub fn login_paste(agent: &Agent, client: &Client) -> Result<Tokens> {
    let mut port = [0u8; 2];
    getrandom::fill(&mut port).expect("OS random number generator failed");
    let port = 49152 + u16::from_le_bytes(port) % 16384;
    let redirect_uri = format!("http://127.0.0.1:{port}");
    let verifier = random_b64(32);
    let state = random_b64(16);
    let url = auth_url(&client.client_id, &redirect_uri, &pkce_challenge(&verifier), &state);

    eprintln!(
        "Open this URL in a browser on any machine and sign in:\n\n{url}\n\n\
         The browser then fails to load {redirect_uri}/... That is expected.\n\
         Copy the full URL from the address bar and paste it here (5 minutes):"
    );
    let code = match classify_pasted(&read_pasted_line()?, &redirect_uri, &state) {
        Callback::Code(code) => code,
        Callback::Fail(msg) => return Err(Error::auth(msg)),
        Callback::Ignore => unreachable!("classify_pasted never ignores"),
    };
    exchange_code(agent, client, &code, &verifier, &redirect_uri)
}

/// One line from stdin, at most 4 KB, within the login timeout.
fn read_pasted_line() -> Result<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    // shortcut: on timeout the reader thread stays blocked; the process exits right after.
    std::thread::spawn(move || {
        let mut line = String::new();
        let res = std::io::stdin().lock().take(4097).read_line(&mut line).map(|_| line);
        let _ = tx.send(res);
    });
    let line = rx
        .recv_timeout(LOGIN_TIMEOUT)
        .map_err(|_| Error::auth("timed out after 5 minutes waiting for the pasted URL"))?
        .map_err(|e| Error::io("reading the pasted URL", e))?;
    if line.len() > 4096 {
        return Err(Error::auth("the pasted URL is longer than 4 KB"));
    }
    Ok(line)
}

/// Checks a pasted redirect URL with the same rules as the loopback request.
pub fn classify_pasted(input: &str, redirect_uri: &str, state: &str) -> Callback {
    let not_redirect = || {
        Callback::Fail(format!(
            "that is not the redirect URL; paste the full address that starts with {redirect_uri}/"
        ))
    };
    let Some(rest) = input.trim().strip_prefix(redirect_uri) else {
        return not_redirect();
    };
    let target = if rest.starts_with('?') { format!("/{rest}") } else { rest.to_string() };
    match classify_callback(&format!("GET {target} HTTP/1.1"), state) {
        Callback::Ignore => not_redirect(),
        other => other,
    }
}

fn wait_for_code(listener: &TcpListener, state: &str) -> Result<String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| Error::io("configuring the loopback listener", e))?;
    let deadline = Instant::now() + LOGIN_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            return Err(Error::auth("timed out after 5 minutes waiting for the browser sign-in"));
        }
        match listener.accept() {
            Ok((stream, _)) => match handle_connection(stream, state) {
                Callback::Ignore => continue,
                Callback::Code(code) => return Ok(code),
                Callback::Fail(msg) => return Err(Error::auth(msg)),
            },
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(Error::io("accepting the browser redirect", e)),
        }
    }
}

fn handle_connection(mut stream: TcpStream, state: &str) -> Callback {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    // Read the request head (bounded). Browsers may open idle preconnect
    // sockets; those time out here and are ignored.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    while buf.len() < 16 * 1024 && !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let line = head.lines().next().unwrap_or("");
    let result = classify_callback(line, state);

    let (status, title, body) = match &result {
        Callback::Ignore => ("404 Not Found", "Not found", ""),
        Callback::Code(_) => (
            "200 OK",
            "mg: signed in",
            "Sign-in complete. You can close this tab and return to the terminal.",
        ),
        Callback::Fail(_) => (
            "400 Bad Request",
            "mg: sign-in failed",
            "Sign-in failed. See the terminal for details.",
        ),
    };
    let html = format!(
        "<!doctype html><meta charset=utf-8><title>{title}</title>\
         <body style=\"font-family:system-ui;text-align:center;padding-top:4rem\">\
         <h2>{title}</h2><p>{body}</p></body>"
    );
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
    result
}

fn oauth_failure(what: &str, status: u16, body: &str) -> Error {
    let detail = serde_json::from_str::<OAuthError>(body)
        .ok()
        .map(|e| {
            let code = e.error.unwrap_or_default();
            match e.error_description {
                Some(d) => format!("{code}: {d}"),
                None => code,
            }
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("HTTP {status}"));
    let err = if detail.starts_with("invalid_grant") {
        Error::auth(format!("{what} failed: {detail}"))
            .hint("the sign-in expired or was revoked; run `mg auth login`")
    } else if status == 401 || detail.starts_with("invalid_client") {
        Error::auth(format!("{what} failed: {detail}"))
            .hint("check the OAuth client; run `mg auth setup --client-json <path>` again")
    } else {
        Error::api(format!("{what} failed: {detail}"))
    };
    err.status(status)
}

fn post_token(agent: &Agent, what: &str, form: &[(&str, &str)]) -> Result<TokenResponse> {
    let mut resp = agent
        .post(TOKEN_URL)
        .send_form(form.iter().copied())
        .map_err(|e| Error::api(format!("{what} failed: {e}")))?;
    let status = resp.status().as_u16();
    let body = resp
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_to_string()
        .map_err(|e| Error::api(format!("{what} failed: {e}")))?;
    if !(200..300).contains(&status) {
        return Err(oauth_failure(what, status, &body));
    }
    serde_json::from_str(&body)
        .map_err(|e| Error::api(format!("{what}: unexpected response: {e}")))
}

fn check_scope(scope: Option<&str>) -> Result<()> {
    match scope {
        Some(s) if !s.split(' ').any(|x| x == SCOPE) => Err(Error::auth(
            "Google did not grant Gmail access (gmail.modify)",
        )
        .hint("run `mg auth login` again and allow the Gmail permission on the consent screen")),
        _ => Ok(()),
    }
}

pub fn exchange_code(
    agent: &Agent,
    client: &Client,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<Tokens> {
    let resp = post_token(
        agent,
        "token exchange",
        &[
            ("client_id", &client.client_id),
            ("client_secret", &client.client_secret),
            ("code", code),
            ("code_verifier", verifier),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect_uri),
        ],
    )?;
    check_scope(resp.scope.as_deref())?;
    let refresh = resp.refresh_token.ok_or_else(|| {
        Error::auth("Google did not return a refresh token").hint("run `mg auth login` again")
    })?;
    Ok(Tokens {
        access: resp.access_token,
        refresh,
    })
}

/// Trades the stored refresh token for a short-lived access token.
pub fn refresh(agent: &Agent, client: &Client, refresh_token: &str) -> Result<String> {
    let resp = post_token(
        agent,
        "token refresh",
        &[
            ("client_id", &client.client_id),
            ("client_secret", &client.client_secret),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ],
    )?;
    check_scope(resp.scope.as_deref())?;
    Ok(resp.access_token)
}

/// Revokes a token at Google. Revoking a refresh token ends the whole grant.
pub fn revoke(agent: &Agent, token: &str) -> Result<()> {
    let mut resp = agent
        .post(REVOKE_URL)
        .send_form([("token", token)])
        .map_err(|e| Error::api(format!("revoke failed: {e}")))?;
    let status = resp.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(());
    }
    let body = resp
        .body_mut()
        .with_config()
        .limit(64 * 1024)
        .read_to_string()
        .unwrap_or_default();
    Err(oauth_failure("revoke", status, &body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_redirect_url() {
        let r = "http://127.0.0.1:50000";
        let code = Callback::Code("4/abc".into());
        assert_eq!(classify_pasted(&format!("{r}/?state=S1&code=4%2Fabc&scope=x\n"), r, "S1"), code);
        assert_eq!(classify_pasted(&format!("  {r}?state=S1&code=4/abc  "), r, "S1"), code);

        let fails = [
            format!("{r}/?state=WRONG&code=4/abc"),
            format!("{r}/?error=access_denied&state=S1"),
            format!("{r}/?state=S1"),
            format!("{r}0/?state=S1&code=4/abc"),
            format!("{r}/other?state=S1&code=4/abc"),
            "http://127.0.0.1:1/?state=S1&code=4/abc".into(),
            "4/abc".into(),
            String::new(),
        ];
        for input in fails {
            assert!(matches!(classify_pasted(&input, r, "S1"), Callback::Fail(_)), "{input}");
        }
    }

    #[test]
    fn pkce_rfc7636_vector() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn random_values_are_urlsafe_and_distinct() {
        let a = random_b64(32);
        assert_eq!(a.len(), 43);
        assert!(a.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        assert_ne!(a, random_b64(32));
    }

    #[test]
    fn auth_url_has_required_params() {
        let url = auth_url("id.apps.googleusercontent.com", "http://127.0.0.1:5555", "ch", "st");
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        for p in [
            "redirect_uri=http%3A%2F%2F127.0.0.1%3A5555",
            "scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fgmail.modify",
            "code_challenge_method=S256",
            "access_type=offline",
            "prompt=consent",
            "state=st",
        ] {
            assert!(url.contains(p), "missing {p}");
        }
    }

    #[test]
    fn percent_round_trip() {
        assert_eq!(percent_encode("a b/ą"), "a%20b%2F%C4%85");
        assert_eq!(percent_decode("a%20b+c%2F%C4%85").unwrap(), "a b c/ą");
        assert_eq!(percent_decode("%zz"), None);
        assert_eq!(percent_decode("%4"), None);
    }

    #[test]
    fn callback_classification() {
        let ok = "GET /?state=S1&code=4%2F0Abc&scope=x HTTP/1.1";
        assert_eq!(classify_callback(ok, "S1"), Callback::Code("4/0Abc".into()));
        assert!(matches!(
            classify_callback("GET /?state=WRONG&code=x HTTP/1.1", "S1"),
            Callback::Fail(_)
        ));
        assert!(matches!(
            classify_callback("GET /?error=access_denied&state=S1 HTTP/1.1", "S1"),
            Callback::Fail(m) if m.contains("access_denied")
        ));
        assert_eq!(classify_callback("GET /favicon.ico HTTP/1.1", "S1"), Callback::Ignore);
        assert_eq!(classify_callback("", "S1"), Callback::Ignore);
        assert_eq!(classify_callback("POST /?code=x HTTP/1.1", "S1"), Callback::Ignore);
    }

    #[test]
    fn loopback_ignores_noise_then_takes_the_redirect() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let browser = std::thread::spawn(move || {
            let send = |req: &str| {
                let mut s = TcpStream::connect(addr).unwrap();
                s.write_all(req.as_bytes()).unwrap();
                let mut resp = String::new();
                let _ = s.read_to_string(&mut resp);
                resp
            };
            drop(TcpStream::connect(addr).unwrap()); // idle preconnect
            let favicon = send("GET /favicon.ico HTTP/1.1\r\nHost: x\r\n\r\n");
            let done = send("GET /?state=S1&code=abc HTTP/1.1\r\nHost: x\r\n\r\n");
            (favicon, done)
        });
        assert_eq!(wait_for_code(&listener, "S1").unwrap(), "abc");
        let (favicon, done) = browser.join().unwrap();
        assert!(favicon.starts_with("HTTP/1.1 404"));
        assert!(done.starts_with("HTTP/1.1 200"));
        assert!(!done.contains("abc"), "code must not be echoed back");
    }

    #[test]
    fn scope_check() {
        assert!(check_scope(Some(SCOPE)).is_ok());
        assert!(check_scope(Some("openid https://www.googleapis.com/auth/gmail.readonly")).is_err());
        assert!(check_scope(None).is_ok());
    }

    #[test]
    fn oauth_errors_map_to_auth() {
        let e = oauth_failure("token refresh", 400, r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#);
        assert_eq!(e.exit_code(), 2);
        assert!(e.message.contains("expired or revoked"));
        assert_eq!(oauth_failure("x", 503, "oops").exit_code(), 3);
    }
}
