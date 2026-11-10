//! Output: JSON or text, and the single place where everything printed is
//! sanitized. Mail content is untrusted, so no control characters, ANSI/OSC
//! escapes or bidi overrides may reach the terminal or the agent.

use std::io::{IsTerminal, Write};

use serde_json::{Map, Value, json};

use crate::error::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Json,
    Text,
}

impl Mode {
    /// `--json` / `--text` win; otherwise text in a TTY, JSON when piped.
    pub fn detect(json: bool, text: bool) -> Mode {
        if json {
            Mode::Json
        } else if text || std::io::stdout().is_terminal() {
            Mode::Text
        } else {
            Mode::Json
        }
    }
}

/// What a command produces: the JSON value and its human rendering.
pub struct View {
    pub json: Value,
    pub text: String,
}

/// Prints a command result to stdout. A closed pipe is not an error.
pub fn emit(mode: Mode, view: &View) {
    let out = match mode {
        Mode::Json => serde_json::to_string(&sanitize_value(&view.json))
            .unwrap_or_else(|_| "null".to_string()),
        Mode::Text => sanitize(view.text.trim_end_matches('\n')),
    };
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{out}");
    let _ = stdout.flush();
}

/// Prints an error to stderr: `{"error":…,"hint":…}` in JSON mode.
pub fn emit_error(mode: Mode, err: &Error) {
    let message = sanitize(&err.message);
    let hint = err.hint.as_deref().map(sanitize);
    let out = match mode {
        Mode::Json => {
            let mut obj = Map::new();
            obj.insert("error".into(), json!(message));
            if let Some(h) = hint {
                obj.insert("hint".into(), json!(h));
            }
            Value::Object(obj).to_string()
        }
        Mode::Text => match hint {
            Some(h) => format!("error: {message}\nhint: {h}"),
            None => format!("error: {message}"),
        },
    };
    let _ = writeln!(std::io::stderr().lock(), "{out}");
}

/// Removes C0/C1 control characters (except newline and tab), ANSI CSI/OSC/DCS
/// escape sequences and bidi override/isolate characters. CRLF becomes LF and
/// a lone CR becomes LF, so text can't overwrite itself on screen.
pub fn sanitize(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => skip_escape(&mut chars),
            '\u{9b}' => skip_csi(&mut chars),
            '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => skip_string(&mut chars),
            '\n' | '\t' => out.push(c),
            '\r' => {
                if chars.peek() != Some(&'\n') {
                    out.push('\n');
                }
            }
            c if c.is_control() || is_bidi_control(c) => {}
            c => out.push(c),
        }
    }
    out
}

type Chars<'a> = std::iter::Peekable<std::str::Chars<'a>>;

fn skip_escape(chars: &mut Chars) {
    match chars.peek() {
        Some('[') => {
            chars.next();
            skip_csi(chars);
        }
        Some(']' | 'P' | 'X' | '^' | '_') => {
            chars.next();
            skip_string(chars);
        }
        Some(_) => {
            // nF escape: intermediate bytes, then one final byte.
            while chars.peek().is_some_and(|c| ('\x20'..='\x2f').contains(c)) {
                chars.next();
            }
            chars.next();
        }
        None => {}
    }
}

fn skip_csi(chars: &mut Chars) {
    while chars.peek().is_some_and(|c| ('\x20'..='\x3f').contains(c)) {
        chars.next();
    }
    if chars.peek().is_some_and(|c| ('\x40'..='\x7e').contains(c)) {
        chars.next();
    }
}

/// OSC/DCS/SOS/PM/APC: everything up to BEL, ST (`ESC \`) or C1 ST.
fn skip_string(chars: &mut Chars) {
    while let Some(c) = chars.next() {
        match c {
            '\x07' | '\u{9c}' => return,
            '\x1b' => {
                if chars.peek() == Some(&'\\') {
                    chars.next();
                }
                return;
            }
            _ => {}
        }
    }
}

fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{061c}')
}

/// Sanitizes every string (and key) in a JSON value.
pub fn sanitize_value(value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(sanitize(s)),
        Value::Array(items) => Value::Array(items.iter().map(sanitize_value).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (sanitize(k), sanitize_value(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Cuts `text` to `max` characters and says how much was dropped.
pub fn truncate(text: &str, max: usize) -> (String, bool) {
    let total = text.chars().count();
    if total <= max {
        return (text.to_string(), false);
    }
    let kept: String = text.chars().take(max).collect();
    let dropped = total - max;
    (
        format!("{}\n[... {dropped} more characters, use --max-chars or --full]", kept.trim_end()),
        true,
    )
}

/// Shortens a single-line cell to `width` characters, ending in `…`.
pub fn fit(text: &str, width: usize) -> String {
    let flat: String = sanitize(text)
        .chars()
        .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
        .collect();
    if flat.chars().count() <= width {
        return flat;
    }
    let mut cut: String = flat.chars().take(width.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// Renders an aligned table. Cells are sanitized and flattened to one line.
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let cols = headers.len();
    let rows: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.iter().map(|c| fit(c, usize::MAX)).collect())
        .collect();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in &rows {
        for (i, cell) in row.iter().enumerate().take(cols) {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let line = |cells: Vec<&str>| {
        let mut s = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i + 1 == cells.len() {
                s.push_str(cell);
            } else {
                let pad = widths[i] - cell.chars().count();
                s.push_str(cell);
                s.push_str(&" ".repeat(pad + 2));
            }
        }
        s.trim_end().to_string()
    };
    let mut out = vec![line(headers.to_vec())];
    for row in &rows {
        out.push(line(row.iter().map(String::as_str).collect()));
    }
    out.join("\n")
}

pub fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_csi_and_sgr() {
        assert_eq!(sanitize("a\x1b[31mred\x1b[0m b"), "ared b");
        assert_eq!(sanitize("x\x1b[2J\x1b[1;1Hy"), "xy");
        assert_eq!(sanitize("x\u{9b}31my"), "xy");
    }

    #[test]
    fn strips_osc_hyperlinks_and_titles() {
        assert_eq!(sanitize("\x1b]0;evil title\x07ok"), "ok");
        assert_eq!(
            sanitize("\x1b]8;;https://evil.example\x1b\\click\x1b]8;;\x1b\\"),
            "click"
        );
        assert_eq!(sanitize("a\u{9d}52;c;payload\u{9c}b"), "ab");
    }

    #[test]
    fn strips_c0_c1_del_and_bidi() {
        assert_eq!(sanitize("a\x00b\x07c\x08d\x7fe\u{85}f"), "abcdef");
        assert_eq!(sanitize("file\u{202e}fdp.exe"), "filefdp.exe");
        assert_eq!(sanitize("x\u{2066}y\u{2069}"), "xy");
    }

    #[test]
    fn keeps_newlines_tabs_and_unicode() {
        assert_eq!(sanitize("Zażółć\tgęślą\njaźń"), "Zażółć\tgęślą\njaźń");
        assert_eq!(sanitize("a\r\nb\rc"), "a\nb\nc");
    }

    #[test]
    fn unterminated_osc_swallows_rest() {
        assert_eq!(sanitize("ok\x1b]52;c;AAAA"), "ok");
    }

    #[test]
    fn sanitizes_json_recursively() {
        let v = json!({"a": ["x\x1b[1my"], "b\x07": {"c": "\u{202e}z"}});
        assert_eq!(sanitize_value(&v), json!({"a": ["xy"], "b": {"c": "z"}}));
    }

    #[test]
    fn truncates_by_chars() {
        let (t, cut) = truncate("ąęśćż", 3);
        assert!(cut);
        assert!(t.starts_with("ąęś\n[... 2 more characters"));
        assert_eq!(truncate("abc", 3), ("abc".to_string(), false));
    }

    #[test]
    fn fits_and_aligns() {
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(fit("a\nb", 10), "a b");
        let t = table(&["ID", "NAME"], &[vec!["1".into(), "x".into()], vec!["22".into(), "y".into()]]);
        assert_eq!(t, "ID  NAME\n1   x\n22  y");
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(123_456), "120.6 KB");
    }
}
