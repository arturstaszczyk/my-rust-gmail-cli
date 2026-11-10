//! Gmail API resource types and small shared helpers.

pub mod attachment;
pub mod mime;
pub mod search;
pub mod thread;

use serde::Deserialize;

use crate::api::Gmail;
use crate::error::{Error, Result};

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Header {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub value: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Body {
    pub attachment_id: Option<String>,
    pub size: Option<u64>,
    pub data: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Part {
    pub part_id: Option<String>,
    pub mime_type: Option<String>,
    pub filename: Option<String>,
    #[serde(default)]
    pub headers: Vec<Header>,
    pub body: Option<Body>,
    #[serde(default)]
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: String,
    pub thread_id: Option<String>,
    #[serde(default)]
    pub label_ids: Vec<String>,
    pub internal_date: Option<String>,
    pub payload: Option<Part>,
}

impl Message {
    pub fn header(&self, name: &str) -> &str {
        self.payload
            .as_ref()
            .map_or("", |p| header(&p.headers, name))
    }

    /// `internalDate` as ISO 8601 UTC, or "" if missing.
    pub fn date(&self) -> String {
        self.internal_date
            .as_deref()
            .and_then(|d| d.parse::<i64>().ok())
            .map(format_date)
            .unwrap_or_default()
    }

    pub fn is_unread(&self) -> bool {
        self.label_ids.iter().any(|l| l == "UNREAD")
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Thread {
    pub id: String,
    #[serde(default)]
    pub messages: Vec<Message>,
}

/// Case-insensitive header lookup; "" if absent.
pub fn header<'a>(headers: &'a [Header], name: &str) -> &'a str {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map_or("", |h| h.value.as_str())
}

/// Ids go into URL paths, so they must be plain base64url-ish tokens.
pub fn validate_id<'a>(id: &'a str, what: &str) -> Result<&'a str> {
    let ok = !id.is_empty()
        && id.len() <= 1024
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok {
        Ok(id)
    } else {
        Err(Error::usage(format!("invalid {what} id"))
            .hint("ids contain only A-Z, a-z, 0-9, - and _"))
    }
}

/// Fetches a thread; if `id` is a message id instead, finds its thread.
pub fn get_thread(gmail: &Gmail, id: &str, query: &[(&str, &str)]) -> Result<Thread> {
    let id = validate_id(id, "thread")?;
    match gmail.get::<Thread>(&format!("/threads/{id}"), query) {
        Err(e) if e.status == Some(404) => {
            let msg: Message = gmail.get(&format!("/messages/{id}"), &[("format", "minimal")])?;
            let thread_id = msg.thread_id.unwrap_or_default();
            let thread_id = validate_id(&thread_id, "thread")?;
            gmail.get(&format!("/threads/{thread_id}"), query)
        }
        other => other,
    }
}

/// Milliseconds since the epoch to `YYYY-MM-DDTHH:MM:SSZ`.
pub fn format_date(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, sod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        sod / 3600,
        sod % 3600 / 60,
        sod % 60
    )
}

/// `2026-11-02T10:15:00Z` -> `2026-11-02 10:15` for tables.
pub fn short_date(iso: &str) -> String {
    iso.get(..16)
        .map(|s| s.replace('T', " "))
        .unwrap_or_else(|| iso.to_string())
}

// Howard Hinnant's days-to-civil algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// Decodes the HTML entities Gmail puts into snippets.
pub fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let decoded = rest.find(';').filter(|&j| j <= 10).and_then(|j| {
            let ent = &rest[1..j];
            let c = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                _ => ent
                    .strip_prefix("#x")
                    .or_else(|| ent.strip_prefix("#X"))
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                    .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                    .and_then(char::from_u32),
            }?;
            Some((c, j + 1))
        });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// `"Jan Kowalski" <jan@x.pl>` -> `Jan Kowalski`; a bare address stays as is.
pub fn display_name(from: &str) -> String {
    match from.find('<') {
        Some(i) if i > 0 => {
            let name = from[..i].trim().trim_matches('"').trim();
            if name.is_empty() {
                from.to_string()
            } else {
                name.to_string()
            }
        }
        _ => from
            .trim_matches(|c| c == '<' || c == '>' || c == ' ')
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};

    /// Serves `route(path)` -> (status, body) on 127.0.0.1; returns the base URL.
    fn stub(route: fn(&str) -> (u16, &'static str)) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for mut conn in listener.incoming().flatten() {
                let mut reader = BufReader::new(&conn);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line
                    .split(' ')
                    .nth(1)
                    .unwrap_or("")
                    .split('?')
                    .next()
                    .unwrap();
                let (status, body) = route(path);
                while reader.read_line(&mut line).unwrap() > 2 {
                    line.clear();
                }
                let _ = write!(
                    conn,
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        base
    }

    #[test]
    fn thread_fallback_keeps_message_error() {
        let gmail = Gmail::for_test(stub(|path| match path {
            "/threads/m1" => (404, ""),
            _ => (500, ""),
        }));
        let e = get_thread(&gmail, "m1", &[]).unwrap_err();
        assert_eq!(e.status, Some(500));
    }

    #[test]
    fn thread_fallback_follows_message() {
        let gmail = Gmail::for_test(stub(|path| match path {
            "/messages/m1" => (200, r#"{"id":"m1","threadId":"t1"}"#),
            "/threads/t1" => (200, r#"{"id":"t1"}"#),
            _ => (404, ""),
        }));
        assert_eq!(get_thread(&gmail, "m1", &[]).unwrap().id, "t1");
    }

    #[test]
    fn validates_ids() {
        assert!(validate_id("19a2b3c4d5e6f7a8", "message").is_ok());
        assert!(validate_id("ANGjdJ8_x-Yz", "attachment").is_ok());
        for bad in ["", "../x", "a/b", "a?b", "a b", "a%2F", "ą", "a\nb"] {
            assert!(validate_id(bad, "message").is_err(), "{bad:?} accepted");
        }
        assert_eq!(validate_id("x/y", "message").unwrap_err().exit_code(), 1);
    }

    #[test]
    fn formats_dates() {
        assert_eq!(format_date(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_date(1_709_210_096_000), "2024-02-29T12:34:56Z");
        assert_eq!(format_date(1_793_613_600_000), "2026-11-02T10:00:00Z");
        assert_eq!(short_date("2026-11-02T10:40:00Z"), "2026-11-02 10:40");
    }

    #[test]
    fn decodes_snippet_entities() {
        assert_eq!(
            decode_entities("it&#39;s &amp; &lt;b&gt; &quot;x&quot; &#x105;"),
            "it's & <b> \"x\" ą"
        );
        assert_eq!(decode_entities("AT&T & co;"), "AT&T & co;");
        assert_eq!(decode_entities("&#0;&#xD800;"), "\0&#xD800;");
    }

    #[test]
    fn extracts_display_names() {
        assert_eq!(display_name("\"Jan Kowalski\" <jan@x.pl>"), "Jan Kowalski");
        assert_eq!(display_name("Jan <jan@x.pl>"), "Jan");
        assert_eq!(display_name("<jan@x.pl>"), "jan@x.pl");
        assert_eq!(display_name("jan@x.pl"), "jan@x.pl");
    }
}
