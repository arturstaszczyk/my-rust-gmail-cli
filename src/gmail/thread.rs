//! `mg thread`: a whole conversation as plain text.

use serde_json::{Value, json};

use super::mime::{self, Attachment};
use super::{Message, Thread, get_thread};
use crate::api::Gmail;
use crate::error::Result;
use crate::output::{self, View};

pub fn run(gmail: &Gmail, id: &str, full: bool, max_chars: usize) -> Result<View> {
    let thread = get_thread(gmail, id, &[("format", "full")])?;
    Ok(render(&thread, full, max_chars))
}

pub fn render(thread: &Thread, full: bool, max_chars: usize) -> View {
    let messages: Vec<Rendered> = thread.messages.iter().map(|m| prepare(m, full, max_chars)).collect();
    let subject = thread.messages.first().map(|m| m.header("Subject")).unwrap_or("");

    let json = json!({
        "id": thread.id,
        "subject": subject,
        "messages": messages.iter().map(|r| r.json.clone()).collect::<Vec<Value>>(),
    });

    let total = messages.len();
    let mut text = format!(
        "{subject}\nthread {} · {total} message{}\n",
        thread.id,
        if total == 1 { "" } else { "s" }
    );
    for (i, r) in messages.iter().enumerate() {
        text.push_str(&format!("\n── [{}/{total}] {}\n{}", i + 1, r.heading, r.body));
        if !text.ends_with('\n') {
            text.push('\n');
        }
    }
    View { json, text }
}

struct Rendered {
    json: Value,
    heading: String,
    body: String,
}

fn prepare(m: &Message, full: bool, max_chars: usize) -> Rendered {
    let walked = m.payload.as_ref().map(mime::walk).unwrap_or_default();
    let raw = mime::body_text(&walked);
    let cleaned = if full { raw.trim_end().to_string() } else { strip_quoted(&raw) };
    let (body, truncated) = output::truncate(&cleaned, max_chars);
    let date = m.date();

    let json = json!({
        "id": m.id,
        "date": date,
        "from": m.header("From"),
        "to": m.header("To"),
        "cc": m.header("Cc"),
        "subject": m.header("Subject"),
        "labels": m.label_ids,
        "unread": m.is_unread(),
        "attachments": walked.attachments.iter().map(attachment_json).collect::<Vec<Value>>(),
        "body": body,
        "truncated": truncated,
    });

    let mut heading = format!("{} UTC  {}", super::short_date(&date), m.header("From"));
    if m.is_unread() {
        heading.push_str("  [unread]");
    }
    let mut meta = format!("id: {}\nto: {}\n", m.id, m.header("To"));
    let cc = m.header("Cc");
    if !cc.is_empty() {
        meta.push_str(&format!("cc: {cc}\n"));
    }
    for a in &walked.attachments {
        meta.push_str(&format!(
            "attachment [part {}]: {} ({}, {})\n",
            a.part,
            a.filename,
            a.mime_type,
            output::format_bytes(a.size)
        ));
    }
    Rendered {
        json,
        heading,
        body: format!("{meta}\n{body}"),
    }
}

pub fn attachment_json(a: &Attachment) -> Value {
    json!({
        "part": a.part,
        "filename": a.filename,
        "mime_type": a.mime_type,
        "size": a.size,
    })
}

/// Drops quoted history: `>` lines, and everything from a reply attribution
/// ("On … wrote:", "W dniu … napisał(a):") or an "Original Message" divider.
pub fn strip_quoted(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if is_divider(t) {
            break;
        }
        if is_attribution_end(t) {
            // Long attributions get wrapped onto two lines.
            if !starts_attribution(t)
                && out.last().is_some_and(|prev| starts_attribution(prev.trim()))
            {
                out.pop();
            }
            break;
        }
        if t.starts_with('>') {
            continue;
        }
        out.push(line);
    }
    collapse_blank_lines(&out.join("\n"))
}

fn starts_attribution(t: &str) -> bool {
    t.starts_with("On ") || t.starts_with("W dniu ") || t.starts_with("Dnia ")
}

fn is_attribution_end(t: &str) -> bool {
    t.ends_with("wrote:")
        || t.ends_with("pisze:")
        || t.ends_with("napisał:")
        || t.ends_with("napisała:")
        || t.ends_with("napisał(a):")
}

fn is_divider(t: &str) -> bool {
    let lower = t.to_lowercase();
    lower.starts_with("-----original message-----")
        || lower.starts_with("-----oryginalna wiadomość-----")
        || lower.starts_with("-------- original message --------")
}

fn collapse_blank_lines(text: &str) -> String {
    let mut out = String::new();
    let mut blanks = 0;
    for line in text.trim().lines() {
        if line.trim().is_empty() {
            blanks += 1;
            if blanks > 1 {
                continue;
            }
        } else {
            blanks = 0;
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread_fixture() -> Thread {
        let path = format!("{}/tests/fixtures/thread.json", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn strips_english_attribution_and_quotes() {
        let t = "Thanks!\n\nOn Mon, Nov 2, 2026 at 10:15 AM Jan <jan@x.pl> wrote:\n> old\n> text";
        assert_eq!(strip_quoted(t), "Thanks!");
    }

    #[test]
    fn strips_wrapped_attribution() {
        let t = "Ok.\n\nOn Mon, Nov 2, 2026 at 10:15 AM Jan Kowalski <\njan@example.pl> wrote:\n> x";
        assert_eq!(strip_quoted(t), "Ok.");
    }

    #[test]
    fn strips_polish_attribution_and_outlook_divider() {
        let t = "Dzięki\nW dniu 2.11.2026 o 11:00 Jan napisał(a):\n> a";
        assert_eq!(strip_quoted(t), "Dzięki");
        let t = "Hi\n\n-----Original Message-----\nFrom: x";
        assert_eq!(strip_quoted(t), "Hi");
    }

    #[test]
    fn keeps_inline_text_and_collapses_blanks() {
        let t = "a\n> quoted inline\nb\n\n\n\nc";
        assert_eq!(strip_quoted(t), "a\nb\n\nc");
    }

    #[test]
    fn renders_thread_fixture() {
        let view = render(&thread_fixture(), false, 4000);
        let msgs = view.json["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(view.json["subject"], "NALICZENIE Kazimierza 11/2026");
        assert_eq!(msgs[0]["date"], "2026-11-02T10:00:00Z");
        assert_eq!(msgs[0]["unread"], true);
        assert_eq!(msgs[0]["attachments"][0]["part"], "1");
        assert_eq!(msgs[1]["body"], "Dziękuję, potwierdzam.");
        assert!(view.text.contains("── [2/2] 2026-11-02 11:00 UTC  Artur <artur@example.com>"));
        assert!(view.text.contains("attachment [part 1]: Faktura 11/2026.pdf (application/pdf, 120.6 KB)"));

        let full = render(&thread_fixture(), true, 4000);
        assert!(full.json["messages"][1]["body"].as_str().unwrap().contains("> w załączniku"));

        let short = render(&thread_fixture(), false, 5);
        assert_eq!(short.json["messages"][0]["truncated"], true);
    }
}
