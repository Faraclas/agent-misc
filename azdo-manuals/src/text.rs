//! Text and JSON helpers. Output must stay byte-identical to the earlier Python generator, so these follow Python's
//! rules where Rust's differ (whitespace set, character-based lengths, ASCII-escaped compact JSON).

use std::fs;
use std::io;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

/// Python's `str.isspace()`: Unicode White_Space plus the four information separators U+001C..U+001F.
pub fn is_py_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

pub fn py_strip(s: &str) -> &str {
    s.trim_matches(is_py_space)
}

/// Collapse whitespace runs to one space, trim, and cut to `limit` characters at a word boundary with `...`.
pub fn one_line(text: &str, limit: usize) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for c in text.chars() {
        if is_py_space(c) {
            pending_space = true;
        } else {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push(c);
        }
    }
    if out.chars().count() > limit {
        let head: String = out.chars().take(limit.saturating_sub(3)).collect();
        let cut = match head.rfind(' ') {
            Some(i) => head[..i].to_string(),
            None => head,
        };
        out = cut + "...";
    }
    out
}

pub const MAX_DESCRIPTION: usize = 400;

pub fn one_line_default(text: &str) -> String {
    one_line(text, MAX_DESCRIPTION)
}

static SENTENCE_END: LazyLock<Regex> = LazyLock::new(|| Regex::new("[.!?] ").unwrap());

/// Byte length of the first sentence of one-line `text`: up to its first `.`, `!` or `?` that a space follows, else
/// all of it.
pub fn first_sentence_len(text: &str) -> usize {
    SENTENCE_END
        .find(text)
        .map_or(text.len(), |m| m.start() + 1)
}

/// A sentence made from a snake_case tool name: `get_role_definitions` -> "Get role definitions". A last word `async`
/// (the suffix of .NET method names) is left out. No full stop, as the client-library tools without a doc comment have
/// always been described ("Get policies").
pub fn name_sentence(name: &str) -> String {
    let mut words: Vec<&str> = name.split('_').filter(|w| !w.is_empty()).collect();
    if words.len() > 1 && words.last() == Some(&"async") {
        words.pop();
    }
    let text = words.join(" ");
    let mut chars = text.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

static CAMEL: LazyLock<Regex> = LazyLock::new(|| Regex::new("([a-z0-9])([A-Z])").unwrap());
static NON_WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new("[^a-z0-9]+").unwrap());

pub fn snake(text: &str) -> String {
    let split = CAMEL.replace_all(text, "${1}_${2}").to_lowercase();
    NON_WORD
        .replace_all(&split, "_")
        .trim_matches('_')
        .to_string()
}

/// Python's `str(value)` for the JSON values the read test formats into strings.
pub fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Null => "None".into(),
        other => other.to_string(),
    }
}

/// A field that may be missing, shown the way Python shows `None`.
pub fn shown(v: Option<&Value>) -> String {
    v.map(py_str).unwrap_or_else(|| "None".into())
}

/// Python truthiness of a JSON value (missing counts as false).
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

/// `json.dumps(value, separators=(",", ":"))`: compact, with every non-ASCII character escaped as `\uXXXX`.
pub fn compact_ascii(value: &Value) -> String {
    let text = serde_json::to_string(value).expect("JSON values always serialize");
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for unit in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
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
