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
/// `_issue id_` emphasis, but not `pull_request` or a code span's `_links`.
static EMPHASIS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(^|[\s("'])_([^_\s]|[^_\s][^_]*[^_\s])_($|[\s).,;:!?"'])"#).unwrap()
});

/// GitHub's Markdown descriptions as plain text: links become their text, alert markers, quote marks and emphasis go.
pub fn plain(markdown: &str) -> String {
    let text = LINK.replace_all(markdown, "$1");
    let text = ALERT.replace_all(&text, "");
    let text = QUOTE.replace_all(&text, "");
    let text = BREAK.replace_all(&text, " ");
    let text = EMPHASIS.replace_all(&text, "$1$2$3");
    TAG.replace_all(&text, "")
        .replace("**", "")
        .replace('`', "")
}

/// Abbreviations whose period does not end a sentence.
const ABBREVIATIONS: [&str; 4] = ["e.g.", "i.e.", "etc.", "vs."];

/// Split one line of plain text into sentences: after `.`, `!` or `?` (and any closing quote or parenthesis), at a
/// space followed by anything but a lowercase letter, except after an abbreviation such as `e.g.`.
pub fn sentences(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if !matches!(bytes[i], b'.' | b'!' | b'?') {
            i += 1;
            continue;
        }
        let mut end = i + 1;
        while end < bytes.len() && matches!(bytes[end], b'"' | b'\'' | b')') {
            end += 1;
        }
        let word = text[..=i]
            .split_whitespace()
            .last()
            .unwrap_or("")
            .trim_start_matches('(')
            .to_lowercase();
        let next = text[end..].trim_start();
        let boundary = text[end..].starts_with(char::is_whitespace)
            && !next.is_empty()
            && !next.starts_with(|c: char| c.is_lowercase())
            && !ABBREVIATIONS.contains(&word.as_str());
        if boundary {
            out.push(text[start..end].trim());
            start = end;
        }
        i = end;
    }
    let rest = text[start..].trim();
    if !rest.is_empty() {
        out.push(rest);
    }
    out
}

/// A sentence that only points to documentation whose link was stripped ("For more information, see X.").
fn is_reference(sentence: &str) -> bool {
    [
        "For more information",
        "For more details",
        "For an example",
        "See ",
    ]
    .iter()
    .any(|p| sentence.starts_with(p))
}

static CLASSIC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)OAuth (?:app tokens|tokens|apps) and personal access tokens \(classic\)")
        .unwrap()
});
static USE_THIS_ENDPOINT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r" to use this endpoint( with| on| if|\.|$)").unwrap());

/// GitHub's "OAuth app tokens and personal access tokens (classic) need the repo scope to use this endpoint." as
/// "Classic tokens need the repo scope."; `None` when the sentence is not about classic token scopes.
fn classic_scope(sentence: &str) -> Option<String> {
    if !CLASSIC.is_match(sentence) && !sentence.contains("public_repo scope instead") {
        return None;
    }
    let text = CLASSIC.replace_all(sentence, "classic tokens");
    let text = USE_THIS_ENDPOINT.replace_all(&text, "$1");
    let mut chars = text.chars();
    Some(match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    })
}

/// Words that do not make a sentence say more than a summary ("Lists all workflow runs for a repository").
const FILLER: [&str; 22] = [
    "a",
    "an",
    "the",
    "all",
    "specific",
    "specified",
    "single",
    "given",
    "that",
    "are",
    "is",
    "on",
    "for",
    "in",
    "of",
    "to",
    "and",
    "by",
    "from",
    "with",
    "its",
    "this",
];

/// Lowercase words with a trailing `s` dropped, so "Gets" and "Get", "comments" and "comment" compare equal.
fn stems(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty() && !FILLER.contains(w))
        .map(|w| {
            w.strip_suffix('s')
                .filter(|s| s.len() > 2)
                .unwrap_or(w)
                .to_string()
        })
        .collect()
}

/// Whether a sentence says nothing its summary does not ("Gets a repository." after "Get a repository").
pub fn restates(summary: &str, sentence: &str) -> bool {
    let summary = stems(summary);
    let words = stems(sentence);
    !words.is_empty() && words.iter().all(|w| summary.contains(w))
}

/// One line of plain text without pointers to stripped links, with classic-token scope sentences shortened, and
/// without a first sentence that restates `summary` (pass `""` to keep it).
pub fn tidy(text: &str, summary: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = Vec::new();
    for (i, sentence) in sentences(&line).into_iter().enumerate() {
        if is_reference(sentence) || (i == 0 && !summary.is_empty() && restates(summary, sentence))
        {
            continue;
        }
        out.push(classic_scope(sentence).unwrap_or_else(|| sentence.to_string()));
    }
    out.join(" ")
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
