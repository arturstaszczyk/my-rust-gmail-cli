//! `mg search` and `mg box`: thread summaries for a Gmail query.

use clap::ValueEnum;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Thread, decode_entities, display_name, short_date, validate_id};
use crate::api::Gmail;
use crate::error::Result;
use crate::output::{View, fit, table};

/// Parallel requests for thread details.
const WORKERS: usize = 8;

/// Only what the summary needs: headers, labels, dates and attachment markers
/// (four levels of MIME nesting). Bodies are not downloaded.
const SUMMARY_FIELDS: &str = "id,messages(id,internalDate,labelIds,payload(headers,filename,\
body/attachmentId,parts(filename,body/attachmentId,parts(filename,body/attachmentId,\
parts(filename,body/attachmentId,parts(filename,body/attachmentId))))))";

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Mailbox {
    Inbox,
    Unread,
    Starred,
    Sent,
}

impl Mailbox {
    pub fn query(self) -> &'static str {
        match self {
            Mailbox::Inbox => "in:inbox",
            Mailbox::Unread => "in:inbox is:unread",
            Mailbox::Starred => "is:starred",
            Mailbox::Sent => "in:sent",
        }
    }
}

#[derive(Deserialize)]
struct ThreadList {
    #[serde(default)]
    threads: Vec<ThreadRef>,
}

#[derive(Deserialize)]
struct ThreadRef {
    id: String,
    snippet: Option<String>,
}

pub fn run(gmail: &Gmail, query: &str, limit: u32) -> Result<View> {
    let limit = limit.to_string();
    let list: ThreadList = gmail.get("/threads", &[("q", query), ("maxResults", &limit)])?;
    for t in &list.threads {
        validate_id(&t.id, "thread")?;
    }

    let chunk = list.threads.len().div_ceil(WORKERS).max(1);
    let details: Vec<Result<Thread>> = std::thread::scope(|s| {
        let handles: Vec<_> = list
            .threads
            .chunks(chunk)
            .map(|refs| {
                s.spawn(move || {
                    refs.iter()
                        .map(|t| {
                            gmail.get::<Thread>(
                                &format!("/threads/{}", t.id),
                                &[("format", "full"), ("fields", SUMMARY_FIELDS)],
                            )
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("worker thread panicked"))
            .collect()
    });

    let mut items = Vec::new();
    for (r, detail) in list.threads.iter().zip(details) {
        items.push(summarize(&detail?, r.snippet.as_deref().unwrap_or("")));
    }
    Ok(render(items))
}

pub struct Summary {
    pub id: String,
    pub date: String,
    pub from: String,
    pub subject: String,
    pub snippet: String,
    pub messages: usize,
    pub attachments: usize,
    pub unread: bool,
}

pub fn summarize(thread: &Thread, snippet: &str) -> Summary {
    let first = thread.messages.first();
    let last = thread.messages.last();
    Summary {
        id: thread.id.clone(),
        date: last.map(|m| m.date()).unwrap_or_default(),
        from: first.map(|m| m.header("From").to_string()).unwrap_or_default(),
        subject: first.map(|m| m.header("Subject").to_string()).unwrap_or_default(),
        snippet: decode_entities(snippet),
        messages: thread.messages.len(),
        attachments: thread
            .messages
            .iter()
            .filter_map(|m| m.payload.as_ref())
            .map(|p| super::mime::walk(p).attachments.len())
            .sum(),
        unread: thread.messages.iter().any(|m| m.is_unread()),
    }
}

fn render(items: Vec<Summary>) -> View {
    let json = Value::Array(
        items
            .iter()
            .map(|s| {
                json!({
                    "id": s.id,
                    "date": s.date,
                    "from": s.from,
                    "subject": s.subject,
                    "snippet": s.snippet,
                    "messages": s.messages,
                    "attachments": s.attachments,
                    "unread": s.unread,
                })
            })
            .collect(),
    );
    let text = if items.is_empty() {
        "No threads found.".to_string()
    } else {
        let rows: Vec<Vec<String>> = items
            .iter()
            .map(|s| {
                vec![
                    if s.unread { "*".into() } else { String::new() },
                    s.id.clone(),
                    short_date(&s.date),
                    fit(&display_name(&s.from), 24),
                    fit(&s.subject, 60),
                    s.messages.to_string(),
                    if s.attachments > 0 { s.attachments.to_string() } else { String::new() },
                ]
            })
            .collect();
        table(&["", "ID", "DATE (UTC)", "FROM", "SUBJECT", "MSGS", "ATT"], &rows)
    };
    View { json, text }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes_thread_fixture() {
        let path = format!("{}/tests/fixtures/thread.json", env!("CARGO_MANIFEST_DIR"));
        let thread: Thread = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let s = summarize(&thread, "Dzi&#281;kuj&#281;, potwierdzam");
        assert_eq!(s.from, "\"Jan Kowalski\" <jan@example.pl>");
        assert_eq!(s.date, "2026-11-02T11:00:00Z");
        assert_eq!((s.messages, s.attachments, s.unread), (2, 1, true));
        assert_eq!(s.snippet, "Dziękuję, potwierdzam");

        let view = render(vec![s]);
        assert_eq!(view.json[0]["attachments"], 1);
        assert!(view.text.contains("Jan Kowalski"));
        assert!(view.text.starts_with("   ID"));
    }

    #[test]
    fn empty_result() {
        let view = render(vec![]);
        assert_eq!(view.json, json!([]));
        assert_eq!(view.text, "No threads found.");
    }
}
