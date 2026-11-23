//! Text and JSON helpers.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

/// Collapse whitespace runs to one space, trim, and cut to `limit` characters at a word boundary with `...`.
pub fn one_line(text: &str, limit: usize) -> String {
    let mut out = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if out.chars().count() > limit {
        let head: String = out.chars().take(limit.saturating_sub(3)).collect();
        let cut = match head.rfind(' ') {
            Some(i) => head[..i].to_string(),
            None => head,
        };
        out = cut.trim_end_matches([',', ';', ':', '.']).to_string() + "...";
    }
    out
}

static LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\([^)\s]+\)").unwrap());
static ALERT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*>\s*\[!(NOTE|WARNING|IMPORTANT|TIP|CAUTION)\]\s*$").unwrap()
});
static QUOTE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\s*>\s?").unwrap());
static BREAK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<br\s*/?>").unwrap());
static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"</?[a-zA-Z][^>]*>").unwrap());

/// GitHub's Markdown descriptions as plain text: links become their text, alert markers and quote marks go.
pub fn plain(markdown: &str) -> String {
    let text = LINK.replace_all(markdown, "$1");
    let text = ALERT.replace_all(&text, "");
    let text = QUOTE.replace_all(&text, "");
    let text = BREAK.replace_all(&text, " ");
    TAG.replace_all(&text, "")
        .replace("**", "")
        .replace('`', "")
}

static NON_WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new("[^a-z0-9]+").unwrap());

/// `create-or-update-file-contents` -> `create_or_update_file_contents`.
pub fn snake(text: &str) -> String {
    NON_WORD
        .replace_all(&text.to_lowercase(), "_")
        .trim_matches('_')
        .to_string()
}

/// JSON truthiness of an optional value (missing, null, false, 0, "", [] and {} are false).
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::Bool(true)) => true,
    }
}

pub fn sorted_map(map: Map<String, Value>) -> Map<String, Value> {
    let mut items: Vec<(String, Value)> = map.into_iter().collect();
    items.sort_by(|a, b| a.0.cmp(&b.0));
    items.into_iter().collect()
}

/// Two-space indented JSON with a trailing newline, written through a temporary file.
pub fn write_json(path: &Path, value: &Value) -> io::Result<()> {
    let text = serde_json::to_string_pretty(value).expect("JSON values always serialize") + "\n";
    write_atomic(path, text.as_bytes())
}

pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)
}
