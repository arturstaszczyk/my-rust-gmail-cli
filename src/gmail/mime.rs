//! Walks a Gmail MIME tree: finds the text and HTML bodies (decoded from their
//! original charset) and the attachments.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_PAD_INDIFFERENT;
use encoding_rs::{Encoding, UTF_8};

use super::{Part, header};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// MIME part id ("1", "1.2"). Stable across fetches, unlike `attachment_id`.
    pub part: String,
    pub attachment_id: Option<String>,
    /// Small parts may carry their bytes inline instead of an attachment id.
    pub inline_data: Option<String>,
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
}

#[derive(Debug, Default)]
pub struct Walked {
    pub text: Option<String>,
    pub html: Option<String>,
    pub attachments: Vec<Attachment>,
}

pub fn walk(payload: &Part) -> Walked {
    let mut acc = Walked::default();
    walk_into(payload, &mut acc);
    acc
}

fn walk_into(part: &Part, acc: &mut Walked) {
    let mime = part.mime_type.as_deref().unwrap_or("").to_ascii_lowercase();
    let filename = part.filename.as_deref().unwrap_or("");
    let body = part.body.as_ref();
    if !filename.is_empty() {
        acc.attachments.push(Attachment {
            part: part.part_id.clone().unwrap_or_default(),
            attachment_id: body.and_then(|b| b.attachment_id.clone()),
            inline_data: body.and_then(|b| b.data.clone()),
            filename: filename.to_string(),
            mime_type: mime,
            size: body.and_then(|b| b.size).unwrap_or(0),
        });
        return;
    }
    if mime == "text/plain" && acc.text.is_none() {
        acc.text = decode_part_text(part);
    } else if mime == "text/html" && acc.html.is_none() {
        acc.html = decode_part_text(part);
    }
    for child in &part.parts {
        walk_into(child, acc);
    }
}

fn decode_part_text(part: &Part) -> Option<String> {
    let data = part.body.as_ref()?.data.as_deref()?;
    let bytes = decode_b64(data)?;
    let charset = charset_of(header(&part.headers, "Content-Type"));
    Some(decode_bytes(&bytes, charset.as_deref()))
}

/// Gmail uses base64url, sometimes with padding.
pub fn decode_b64(data: &str) -> Option<Vec<u8>> {
    URL_SAFE_PAD_INDIFFERENT.decode(data.trim()).ok()
}

/// The `charset` parameter of a Content-Type header value.
pub fn charset_of(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|param| {
        let (k, v) = param.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| v.trim().trim_matches('"').trim().to_string())
            .filter(|v| !v.is_empty())
    })
}

/// Decodes bytes in the given charset; unknown or missing charset means UTF-8.
/// Invalid sequences become U+FFFD rather than failing.
pub fn decode_bytes(bytes: &[u8], charset: Option<&str>) -> String {
    let encoding = charset
        .and_then(|c| Encoding::for_label(c.as_bytes()))
        .unwrap_or(UTF_8);
    encoding.decode(bytes).0.into_owned()
}

/// Plain text of a message: text/plain if present, else HTML converted.
pub fn body_text(walked: &Walked) -> String {
    if let Some(text) = walked.text.as_deref().filter(|t| !t.trim().is_empty()) {
        return text.to_string();
    }
    match walked.html.as_deref() {
        Some(html) => html2text::from_read(html.as_bytes(), 100).unwrap_or_default(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gmail::Message;

    fn fixture(name: &str) -> Message {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(&path).expect("fixture exists");
        serde_json::from_str(&text).expect("fixture parses")
    }

    fn walked(name: &str) -> Walked {
        walk(fixture(name).payload.as_ref().unwrap())
    }

    #[test]
    fn multipart_alternative_with_pdf() {
        let w = walked("message_multipart.json");
        assert_eq!(w.text.as_deref(), Some("Dzień dobry,\nw załączniku faktura.\n"));
        assert!(w.html.as_deref().unwrap().contains("<b>faktura</b>"));
        assert_eq!(w.attachments.len(), 1);
        let a = &w.attachments[0];
        assert_eq!(a.part, "1");
        assert_eq!(a.filename, "Faktura 11/2026.pdf");
        assert_eq!(a.mime_type, "application/pdf");
        assert_eq!(a.attachment_id.as_deref(), Some("ANGjdJ_att1"));
        assert_eq!(a.size, 123_456);
    }

    #[test]
    fn nested_parts_and_inline_attachment() {
        let w = walked("message_nested.json");
        assert_eq!(w.text.as_deref(), Some("Outer text"));
        let names: Vec<_> = w.attachments.iter().map(|a| a.filename.as_str()).collect();
        assert_eq!(names, ["logo.png", "notes.txt"]);
        assert_eq!(w.attachments[0].part, "0.1.1");
        assert_eq!(w.attachments[1].inline_data.as_deref(), Some("aGVsbG8"));
        assert_eq!(w.attachments[1].attachment_id, None);
    }

    #[test]
    fn iso_8859_2_body() {
        let w = walked("message_iso8859_2.json");
        assert_eq!(w.text.as_deref(), Some("Zażółć gęślą jaźń"));
    }

    #[test]
    fn html_only_body_is_converted() {
        let w = walked("message_html_only.json");
        assert!(w.text.is_none());
        let text = body_text(&w);
        assert!(text.contains("Hello"), "{text}");
        assert!(text.contains("world"), "{text}");
        assert!(!text.contains("<p>"), "{text}");
    }

    #[test]
    fn charset_parsing() {
        assert_eq!(charset_of("text/plain; charset=\"ISO-8859-2\"").as_deref(), Some("ISO-8859-2"));
        assert_eq!(charset_of("text/plain; format=flowed; CHARSET=utf-8").as_deref(), Some("utf-8"));
        assert_eq!(charset_of("text/plain"), None);
        assert_eq!(decode_bytes(b"\xb1", Some("bogus-charset")), "\u{fffd}");
        assert_eq!(decode_bytes(b"\xb1", Some("iso-8859-2")), "ą");
    }
}
