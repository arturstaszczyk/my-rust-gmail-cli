//! `mg attachment list|save`. Saved files get sanitized names, never
//! overwrite anything, and only their paths are printed.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

use super::mime::{self, Attachment};
use super::{Message, get_thread, validate_id};
use crate::api::Gmail;
use crate::error::{Error, Result};
use crate::output::{View, fit, format_bytes, table};

const MAX_NAME_CHARS: usize = 120;

/// Lists attachments of a thread, or of a single message.
pub fn list(gmail: &Gmail, id: &str) -> Result<View> {
    let thread = get_thread(gmail, id, &[("format", "full")])?;
    let rows: Vec<(String, Attachment)> = thread
        .messages
        .iter()
        .filter_map(|m| m.payload.as_ref().map(|p| (m.id.clone(), p)))
        .flat_map(|(mid, p)| mime::walk(p).attachments.into_iter().map(move |a| (mid.clone(), a)))
        .collect();
    Ok(render_list(&rows))
}

fn render_list(rows: &[(String, Attachment)]) -> View {
    let json = Value::Array(
        rows.iter()
            .map(|(mid, a)| {
                json!({
                    "message_id": mid,
                    "part": a.part,
                    "filename": a.filename,
                    "mime_type": a.mime_type,
                    "size": a.size,
                })
            })
            .collect(),
    );
    let text = if rows.is_empty() {
        "No attachments.".to_string()
    } else {
        let cells: Vec<Vec<String>> = rows
            .iter()
            .map(|(mid, a)| {
                vec![
                    mid.clone(),
                    a.part.clone(),
                    fit(&a.filename, 60),
                    a.mime_type.clone(),
                    format_bytes(a.size),
                ]
            })
            .collect();
        table(&["MESSAGE", "PART", "FILENAME", "TYPE", "SIZE"], &cells)
    };
    View { json, text }
}

#[derive(Deserialize)]
struct AttachmentBody {
    data: String,
}

/// Saves one attachment (selected by part, or attachment id) or all of them.
pub fn save(gmail: &Gmail, message_id: &str, select: Option<&str>, out: &Path) -> Result<View> {
    let message_id = validate_id(message_id, "message")?;
    let message: Message = gmail.get(&format!("/messages/{message_id}"), &[("format", "full")])?;
    let all = message.payload.as_ref().map(mime::walk).unwrap_or_default().attachments;
    if all.is_empty() {
        return Err(Error::usage("that message has no attachments"));
    }
    let targets: Vec<&Attachment> = match select {
        Some(sel) => all
            .iter()
            .filter(|a| a.part == sel || a.attachment_id.as_deref() == Some(sel))
            .collect(),
        None => all.iter().collect(),
    };
    if targets.is_empty() {
        return Err(Error::usage("no attachment with that part on the message")
            .hint(format!("run `mg attachment list {message_id}` and use the PART column")));
    }

    fs::create_dir_all(out).map_err(|e| Error::io(format!("creating {}", out.display()), e))?;
    let dir = out
        .canonicalize()
        .map_err(|e| Error::io(format!("resolving {}", out.display()), e))?;

    let mut saved = Vec::new();
    for a in targets {
        let bytes = fetch_bytes(gmail, message_id, a)?;
        let (path, file) = create_unique(&dir, &safe_filename(&a.filename))
            .map_err(|e| Error::io(format!("creating a file in {}", dir.display()), e))?;
        write_or_remove(&path, file, &bytes)?;
        saved.push((path, a.mime_type.clone(), bytes.len() as u64));
    }

    let json = json!({
        "saved": saved.iter().map(|(p, mime, size)| json!({
            "path": p.to_string_lossy(),
            "mime_type": mime,
            "size": size,
        })).collect::<Vec<Value>>(),
    });
    let text = saved
        .iter()
        .map(|(p, _, _)| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(View { json, text })
}

fn fetch_bytes(gmail: &Gmail, message_id: &str, a: &Attachment) -> Result<Vec<u8>> {
    let data = match (&a.attachment_id, &a.inline_data) {
        (Some(att_id), _) => {
            let att_id = validate_id(att_id, "attachment")?;
            gmail
                .get::<AttachmentBody>(&format!("/messages/{message_id}/attachments/{att_id}"), &[])?
                .data
        }
        (None, Some(data)) => data.clone(),
        (None, None) => String::new(),
    };
    mime::decode_b64(&data).ok_or_else(|| Error::api("attachment data is not valid base64"))
}

fn write_or_remove(path: &Path, mut file: File, bytes: &[u8]) -> Result<()> {
    if let Err(e) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(path);
        return Err(Error::io(format!("writing {}", path.display()), e));
    }
    Ok(())
}

/// Letters, digits, `.`, `-`, `_` only. Path separators become `_`, so the
/// result is always a plain basename. No leading dots or dashes; at most 120
/// characters with the extension kept.
pub fn safe_filename(name: &str) -> String {
    let mapped: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect();
    let trimmed = mapped.trim_start_matches(['.', '-', '_']).trim_end_matches(['.', '_']);
    let name = if trimmed.chars().count() > MAX_NAME_CHARS {
        let (stem, ext) = split_ext(trimmed);
        let ext = if ext.chars().count() <= 16 { ext } else { "" };
        let keep = MAX_NAME_CHARS - ext.chars().count();
        format!("{}{ext}", stem.chars().take(keep).collect::<String>())
    } else {
        trimmed.to_string()
    };
    if name.is_empty() || name.chars().all(|c| c == '.' || c == '_') {
        "attachment".to_string()
    } else {
        name
    }
}

/// `report.pdf` -> (`report`, `.pdf`); `README` -> (`README`, ``).
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    }
}

/// Creates `dir/name`, or `name-1`, `name-2`… if taken. `create_new` is atomic
/// and refuses existing files and symlinks, so nothing is ever overwritten.
pub fn create_unique(dir: &Path, name: &str) -> std::io::Result<(PathBuf, File)> {
    let (stem, ext) = split_ext(name);
    for i in 0..1000 {
        let candidate = if i == 0 { name.to_string() } else { format!("{stem}-{i}{ext}") };
        let path = dir.join(candidate);
        match OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other(format!("too many files named like {name}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_filenames() {
        assert_eq!(safe_filename("Faktura 11/2026.pdf"), "Faktura_11_2026.pdf");
        assert_eq!(safe_filename("Faktura VAT 11-2026.pdf"), "Faktura_VAT_11-2026.pdf");
        assert_eq!(safe_filename("../../.bashrc"), "bashrc");
        assert_eq!(safe_filename("/etc/passwd"), "etc_passwd");
        assert_eq!(safe_filename("C:\\Users\\x\\evil.exe"), "C__Users_x_evil.exe");
        assert_eq!(safe_filename("zażółć.txt"), "zażółć.txt");
        assert_eq!(safe_filename("a$(rm -rf ~);`x`.pdf"), "a__rm_-rf_____x_.pdf");
        assert_eq!(safe_filename("--help"), "help");
        assert_eq!(safe_filename("doc\u{202e}fdp.exe"), "doc_fdp.exe");
        assert_eq!(safe_filename("x\x1b[31m.pdf"), "x__31m.pdf");
        for empty in ["", "..", "/", "...", "___"] {
            assert_eq!(safe_filename(empty), "attachment", "{empty:?}");
        }
        let long = format!("{}.pdf", "a".repeat(300));
        let s = safe_filename(&long);
        assert_eq!(s.chars().count(), 120);
        assert!(s.ends_with(".pdf"));
    }

    #[test]
    fn never_overwrites() {
        let dir = std::env::temp_dir().join(format!("mg-test-{}", crate::auth::oauth::random_b64(8)));
        fs::create_dir(&dir).unwrap();
        let (p1, f1) = create_unique(&dir, "a.pdf").unwrap();
        write_or_remove(&p1, f1, b"one").unwrap();
        let (p2, f2) = create_unique(&dir, "a.pdf").unwrap();
        write_or_remove(&p2, f2, b"two").unwrap();
        let (p3, _) = create_unique(&dir, "a.pdf").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", dir.join("b")).unwrap();
        let (p4, _) = create_unique(&dir, "b").unwrap();

        assert_eq!(p1.file_name().unwrap(), "a.pdf");
        assert_eq!(p2.file_name().unwrap(), "a-1.pdf");
        assert_eq!(p3.file_name().unwrap(), "a-2.pdf");
        assert_eq!(p4.file_name().unwrap(), "b-1");
        assert_eq!(fs::read(&p1).unwrap(), b"one");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&p1).unwrap().permissions().mode() & 0o777, 0o600);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lists_attachments() {
        let path = format!("{}/tests/fixtures/message_nested.json", env!("CARGO_MANIFEST_DIR"));
        let m: Message = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        let rows: Vec<_> = mime::walk(m.payload.as_ref().unwrap())
            .attachments
            .into_iter()
            .map(|a| (m.id.clone(), a))
            .collect();
        let view = render_list(&rows);
        assert_eq!(view.json[0]["part"], "0.1.1");
        assert_eq!(view.json[1]["filename"], "notes.txt");
        assert!(view.text.lines().next().unwrap().starts_with("MESSAGE"));
    }
}
