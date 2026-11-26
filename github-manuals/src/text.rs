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

/// A Markdown image, `![alt](url "title")`, becomes its alt text.
static IMAGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"!\[([^\]]*)\]\([^)\s]+(?:\s+"[^"]*")?\)"#).unwrap());
static LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\[([^\]]+)\]\([^)\s]+(?:\s+"[^"]*")?\)"#).unwrap());
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

/// A table's rule row, `| --- | :---: |`.
static TABLE_RULE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\|?\s*:?-{3,}:?\s*(\|\s*:?-{3,}:?\s*)*\|?$").unwrap());
static BULLET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*[*-]\s+(.*)$").unwrap());
/// A line that is only bold text, used as a heading (`**Signature verification object**`).
static HEADING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\*\*([^*]+)\*\*$").unwrap());

/// Markdown blocks as paragraphs (separated by a blank line): each list item (with its wrapped lines) and table row
/// is one; a table row is `first cell: other cells`, without the header and rule rows; a heading line is dropped
/// unless it ends with `:` ("For a parent or child team:"); code keeps its text, without the fences. A list after a
/// line ending with `:` joins that line instead when its items are short ("values: ref, tool and category.") or
/// lowercase phrases ("one of: failure if ...; pending if ...; or success if ...").
fn blocks(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = String::new();
    // A list item or table row is a sentence of its own: it ends with a period unless it has its own punctuation.
    let close = |out: &mut String| {
        if !out.trim_end().ends_with(['.', '!', '?', ':', ';']) {
            out.push('.');
        }
        out.push_str("\n\n");
    };
    let paragraph = |out: &mut String, item: &str| {
        out.push_str("\n\n");
        out.push_str(item.trim());
        close(out);
    };
    let flush = |out: &mut String, items: &mut Vec<String>| {
        if items.is_empty() {
            return;
        }
        let short = |item: &String| {
            item.split_whitespace().count() <= 3 && !item.contains(['.', ':', ';', '!', '?', ','])
        };
        // "**failure** if any of the contexts report as `error`": a phrase that continues the lead, not a sentence.
        let phrase = |item: &String| {
            !item.contains(['.', ':', ';', '!', '?'])
                && item
                    .trim_start_matches(['*', '`', '_'])
                    .starts_with(|c: char| c.is_lowercase())
        };
        let lead_line = out.trim_end().ends_with(':');
        let joins = items.len() >= 2 && lead_line;
        if joins && (items.iter().all(short) || items.iter().all(phrase)) {
            let (last, rest) = items.split_last().unwrap();
            // "one of these reasons: off-topic, resolved or spam", but "values: ref, tool and category".
            let lead = out
                .trim_end()
                .rsplit(['.', '\n'])
                .next()
                .unwrap_or("")
                .to_lowercase();
            let word = if lead.contains("one of") || lead.contains("either") {
                "or"
            } else {
                "and"
            };
            let joined = if items.iter().all(short) {
                format!("{} {word} {last}", rest.join(", "))
            } else {
                format!("{}; {word} {last}", rest.join("; "))
            };
            out.truncate(out.trim_end().len());
            out.push(' ');
            out.push_str(&joined);
            close(out);
        } else {
            for item in items.iter() {
                paragraph(out, item);
            }
        }
        items.clear();
    };
    let mut code = false;
    let mut items: Vec<String> = Vec::new();
    // A blank line after a list item: another item continues the list, anything else ends it.
    let mut gap = false;
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        // A list item's wrapped line is indented or starts with a lowercase word.
        let wrapped = !items.is_empty()
            && !gap
            && !t.is_empty()
            && !BULLET.is_match(line)
            && !t.starts_with("```")
            && !t.starts_with('|')
            && (line.starts_with(char::is_whitespace) || t.starts_with(char::is_lowercase));
        if wrapped {
            let last = items.last_mut().unwrap();
            last.push(' ');
            last.push_str(t);
            continue;
        }
        if let Some(c) = BULLET.captures(line).filter(|_| !code) {
            items.push(c[1].trim().to_string());
            gap = false;
            continue;
        }
        if t.is_empty() && !items.is_empty() {
            gap = true;
            continue;
        }
        gap = false;
        flush(&mut out, &mut items);
        if t.starts_with("```") {
            code = !code;
            out.push_str("\n\n");
        } else if code {
            out.push_str(line);
            out.push('\n');
        } else if t.starts_with('|') {
            let header = lines
                .get(i + 1)
                .is_some_and(|next| TABLE_RULE.is_match(next.trim()));
            if TABLE_RULE.is_match(t) || header {
                continue;
            }
            let cells: Vec<&str> = t
                .trim_matches('|')
                .split('|')
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .collect();
            if let Some((first, rest)) = cells.split_first() {
                let row = if rest.is_empty() {
                    first.to_string()
                } else {
                    format!("{first}: {}", rest.join(", "))
                };
                paragraph(&mut out, &row);
            }
        } else if let Some(c) = HEADING.captures(t) {
            if c[1].trim_end().ends_with(':') {
                paragraph(&mut out, &c[1]);
            } else {
                out.push_str("\n\n");
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    flush(&mut out, &mut items);
    out
}

/// Stand-ins for `<`, `>` and `\` inside code (`refs/heads/<branch name>`, `\A|[^0-9A-Za-z]`), so [`TAG`] does not
/// take them for HTML and [`ESCAPE`] leaves them.
const LT: char = '\u{E000}';
const GT: char = '\u{E001}';
const BACKSLASH: char = '\u{E002}';
static CODE_SPAN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`[^`\n]+`").unwrap());
/// A Markdown backslash escape outside code (`ISSUE\_TEMPLATE`).
static ESCAPE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\\([\\`*_{}\[\]()#+\-.!<>|])").unwrap());

/// A code span that is URL query arguments: a search's (`q=tetris+language:assembly&sort=stars`), or several
/// (`all=true&is_done=false`).
static QUERY_STRING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:q=[^\s&=]+(?:&[a-z_]+=[^\s&=]+)*|[a-z_]+=[^\s&=]+(?:&[a-z_]+=[^\s&=]+)+)$")
        .unwrap()
});

/// URL query arguments as the JSON arguments a tool takes (`{"q": "tetris language:assembly", "sort": "stars"}`),
/// with `+` and `%XX` decoded; numbers and booleans keep their type. Other text as it is.
fn query_arguments(code: &str) -> String {
    if !QUERY_STRING.is_match(code) {
        return code.to_string();
    }
    let decode = |s: &str| {
        let bytes = s.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            let hex = bytes
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            match (bytes[i], hex) {
                (b'%', Some(b)) => {
                    out.push(b);
                    i += 3;
                }
                (b'+', _) => {
                    out.push(b' ');
                    i += 1;
                }
                (b, _) => {
                    out.push(b);
                    i += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    };
    let pairs: Vec<String> = code
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| {
            let value = decode(value);
            let value = if value.parse::<u64>().is_ok() || value == "true" || value == "false" {
                value
            } else {
                Value::String(value).to_string()
            };
            format!("\"{key}\": {value}")
        })
        .collect();
    format!("{{{}}}", pairs.join(", "))
}

/// Markdown with `<`, `>` and `\` in code spans and fenced code blocks replaced by [`LT`], [`GT`] and [`BACKSLASH`],
/// and a search's URL query in a code span written as JSON arguments ([`query_arguments`]).
fn protect_code(markdown: &str) -> String {
    let hide = |s: &str| {
        s.replace('<', &LT.to_string())
            .replace('>', &GT.to_string())
            .replace('\\', &BACKSLASH.to_string())
    };
    let mut fenced = false;
    let mut out: Vec<String> = Vec::new();
    for line in markdown.split('\n') {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            out.push(line.to_string());
        } else if fenced {
            out.push(hide(line));
        } else {
            out.push(
                CODE_SPAN
                    .replace_all(line, |c: &regex::Captures| {
                        let code = &c[0][1..c[0].len() - 1];
                        hide(&format!("`{}`", query_arguments(code)))
                    })
                    .into_owned(),
            );
        }
    }
    out.join("\n")
}

/// GitHub's description of the `verification` object a commit or tag answer has: a heading, a table of its fields
/// and a table of every `reason`, some 1,500 characters on each tool that returns one. [`VERIFICATION_SUMMARY`]
/// says it in one sentence; the answer has the object itself.
static VERIFICATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?s)(?:\*\*Signature verification object\*\*\s*)?The response will include a `verification` object.*?These are the possible values for `reason`[^\n]*\n(?:[ \t]*\n)*(?:[ \t]*\|[^\n]*(?:\n|$))+",
    )
    .unwrap()
});
const VERIFICATION_SUMMARY: &str = "The answer's verification object says whether GitHub verified the signature: \
     verified, reason (valid, or why not, such as unsigned or expired_key), signature, payload and verified_at.\n";

/// A search example GitHub splits in two ("For example, if you want to find X. Your query might look like this:").
static EXAMPLE_LEAD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(For example, if you[^\n]*?)\.\s+Your query").unwrap());
/// The lead-in to an example ("would look something like this."), ending with a colon.
static LIKE_THIS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:something )?like this[.:](\s+[`{])").unwrap());

/// GitHub's Markdown descriptions as plain text: links become their text, images their alt text, lists, tables,
/// headings and code blocks paragraphs ([`blocks`]), and alert markers, quote marks and emphasis go.
pub fn plain(markdown: &str) -> String {
    let text = VERIFICATION.replace_all(markdown, VERIFICATION_SUMMARY);
    let text = protect_code(&text);
    let text = IMAGE.replace_all(&text, "$1");
    let text = LINK.replace_all(&text, "$1");
    let text = ALERT.replace_all(&text, "");
    let text = QUOTE.replace_all(&text, "");
    let text = blocks(&text);
    let text = BREAK.replace_all(&text, " ");
    let text = EMPHASIS.replace_all(&text, "$1$2$3");
    let text = TAG.replace_all(&text, "");
    let text = EXAMPLE_LEAD.replace_all(&text, "$1, your query");
    let text = LIKE_THIS.replace_all(&text, "like this:$1");
    ESCAPE
        .replace_all(&text, "$1")
        .replace("**", "")
        .replace('`', "")
        .replace(LT, "<")
        .replace(GT, ">")
        .replace(BACKSLASH, "\\")
        .trim()
        .to_string()
}

/// Abbreviations whose period does not end a sentence.
const ABBREVIATIONS: [&str; 4] = ["e.g.", "i.e.", "etc.", "vs."];

/// Split one line of plain text into sentences: after `.`, `!` or `?` (and any closing quote or parenthesis) and a
/// space, except after an abbreviation such as `e.g.`. Before a lowercase word (a value or a field name:
/// "... the cache. created_at means ...") only a word of three letters or more ends a sentence, not `v2.` or `No.`.
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
        let plain_word = word.len() > 3 && word[..word.len() - 1].chars().all(char::is_alphabetic);
        let boundary = text[end..].starts_with(char::is_whitespace)
            && !next.is_empty()
            && (plain_word || !next.starts_with(|c: char| c.is_lowercase()))
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

/// A tool name the link rewriting wrote (`github_repos.get_commit`); every manual's name starts with `github_`.
static TOOL_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bgithub_[a-z_]+\.[a-z][a-z0-9_]*").unwrap());

/// The tool names (`github_repos.get_commit`) a text names.
pub fn named_tools(text: &str) -> impl Iterator<Item = &str> {
    TOOL_NAME.find_iter(text).map(|m| m.as_str())
}
/// "For the list of these registries, see X." and "To create secrets for an environment, see X."
static POINTER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:For|To) [^,]+, see ").unwrap());

/// A sentence that only points to documentation whose link was stripped ("For more information, see X."), or to a
/// route no tool has ("You can also specify a repository by repository_id using the route DELETE /repositories/...").
/// One that points to a tool is kept.
fn is_reference(sentence: &str) -> bool {
    if TOOL_NAME.is_match(sentence) {
        return false;
    }
    let pointer = [
        "For more information",
        "For more details",
        "For details",
        "For an example",
        "To learn more",
        "See ",
        "Refer to ",
    ]
    .iter()
    .any(|p| sentence.starts_with(p));
    pointer
        || POINTER.is_match(sentence)
        || sentence.contains("using the route ")
        || sentence.contains("via a legacy route")
}

/// HTTP details utcp takes care of, or that an agent cannot act on: headers and media types (a tool's `Accept` is
/// fixed), HTTP client advice, redirect and upload URLs (utcp follows redirects and the upload tool takes a
/// release_id), and the Link header (utcp prints the next page's arguments as next_page).
static HTTP_DETAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:Accept|Content-Type|Location) header|Content-Length|media type|HTTP (?:client|framework)|\bSNI\b|hypermedia|the Link header|raw binary|pass your authentication|response header|^(?:This|The) (?:link|URL) expires|links are temporary|upload_url|^By default,? (?:this (?:endpoint|tool) returns|.* returned in) JSON|^Example: https?://api\.github\.com/|\b(?:example|following) response\b",
    )
    .unwrap()
});
/// Rewrites that turn an HTTP detail into what an agent does with utcp, applied before [`HTTP_DETAIL`], and GitHub's
/// wordings that [`THIS_ENDPOINT`] would get wrong or that say less than they could.
const HTTP_REWRITES: [(&str, &str); 13] = [
    ("as given in the Link header", "as given in next_page"),
    (
        "Use the Link header to get the URL for the next page of",
        "Call again with next_page for the next page of",
    ),
    (
        "The response is paginated and you can use the page and per_page parameters to list the analyses you're \
         interested in",
        "Call again with next_page for the next page of analyses",
    ),
    ("Gets a redirect URL to download", "Downloads"),
    (" when you follow the redirect", ""),
    (" in the URL must", " must"),
    // A code scanning tool, not the tool an agent calls.
    (
        "Only results by this tool",
        "Only results by that code scanning tool",
    ),
    ("from this endpoint response", "from this tool's answer"),
    ("For more information see ", "For more information, see "),
    // The same note in two wordings, both naming the API rather than what holds.
    (
        "The Checks API only looks for pushes in the repository where the check suite or check run were created",
        "Checks only look for pushes in the repository where the check suite or check run was created",
    ),
    (
        "The endpoints to manage checks only look for pushes in the repository where the check suite or check run were created",
        "Checks only look for pushes in the repository where the check suite or check run was created",
    ),
    (
        "The Checks API limits the number of annotations to a maximum of 50 per API request",
        "A call takes at most 50 annotations",
    ),
    // "The rules_count field in the (default) response give the number of rules".
    (" response give the number", " answer gives the number"),
];
/// "You can use the REST API to list the sub-issues on an issue.": what the tool does, after words that say only
/// that it is part of the API.
const REST_API_LEAD: &str = "You can use the REST API to ";
/// A URL placeholder written the Rails way ("If you omit :ref"), which is the argument `ref` in a tool.
static COLON_PARAM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(^|[\s(]):([a-z][a-z_]*)\b").unwrap());
/// The tool itself in GitHub's words ("Anyone with read access can use this endpoint", "This method returns up to
/// 100 results", "this API requires", "That endpoint reports"), and tools in general ("the endpoint will return",
/// ""Issues" endpoints"); an agent calls tools.
static THIS_ENDPOINT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b([Tt]h(?:is|at)) (?:API endpoint|endpoint|method|API)\b|\b([Tt]he) endpoint\b|\bendpoints\b")
        .unwrap()
});
/// "pass the query parameter all=false": an argument, not a URL.
static QUERY_PARAM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:and )?pass the query parameter ([a-z_]+)=([^\s.,;]+)").unwrap()
});
/// An example of a list in the URL query ("Example: cwes=79,284,22 or cwes[]=79&cwes[]=284&cwes[]=22"), either
/// way round.
static LIST_EXAMPLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^Example: (?:([a-z_]+)=([^\s&]+) or ([a-z_]+)\[\]=\S+|([a-z_]+)\[\]=\S+ or ([a-z_]+)=([^\s&]+))$",
    )
    .unwrap()
});

/// "A response with an HTTP 200 status means that you already added the reaction type to this issue." utcp prints
/// the answer, not its status, and the answer is the same reaction either way.
static REACTION_EXISTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^A response with (?:an HTTP 200 status|a Status: 200 OK) means that you already added the reaction type to (this [a-z ]+)\.$")
        .unwrap()
});

/// A list example in the URL query as the JSON list an argument takes (utcp sends it comma-separated):
/// `Example: ["79", "284", "22"]`.
fn list_example(sentence: &str) -> Option<String> {
    let c = LIST_EXAMPLE.captures(sentence)?;
    let (name, values, other) = match (c.get(1), c.get(4)) {
        (Some(name), _) => (name, &c[2], &c[3]),
        (None, Some(name)) => (name, &c[6], &c[5]),
        (None, None) => return None,
    };
    if name.as_str() != other {
        return None;
    }
    let list: Vec<Value> = values
        .split(',')
        .map(|v| Value::String(v.to_string()))
        .collect();
    Some(format!("Example: {}", Value::Array(list)).replace("\",\"", "\", \""))
}

/// A sentence with [`HTTP_REWRITES`] applied, or `None` when it is an [`HTTP_DETAIL`].
fn http_detail(sentence: &str) -> Option<String> {
    let mut text = match sentence.strip_prefix(REST_API_LEAD) {
        Some(rest) => capitalized(rest),
        None => sentence.to_string(),
    };
    for (from, to) in HTTP_REWRITES {
        text = text.replace(from, to);
    }
    let text = REACTION_EXISTS.replace(
        &text,
        "If you already added this reaction type to $1, the answer is that reaction and nothing is added.",
    );
    let text = COLON_PARAM.replace_all(&text, "$1$2");
    let text =
        THIS_ENDPOINT.replace_all(&text, |c: &regex::Captures| match c.get(1).or(c.get(2)) {
            Some(t) => format!("{} tool", t.as_str()),
            None => "tools".to_string(),
        });
    let text = QUERY_PARAM.replace_all(&text, "with $1 set to $2");
    let text = list_example(&text).unwrap_or(text.into_owned());
    (!HTTP_DETAIL.is_match(&text)).then_some(text)
}

/// Which GitHub plans have a feature ("Protected branches are available in public repositories with GitHub Free
/// and ..."): the same long sentence on many tools, and GitHub's answer says so when a repository's plan lacks it.
/// Also what holds only on GitHub Enterprise Server, which the manuals do not call.
static PLANS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"available in public repositories with GitHub Free\b|^For GitHub Enterprise Server,",
    )
    .unwrap()
});

/// "Note: " or "Notes: " in front of a sentence, which says nothing once the note's box is gone.
static NOTE_PREFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^Notes?:\s+").unwrap());

/// `sentence` without a [`NOTE_PREFIX`], starting with a capital letter.
fn without_note(sentence: String) -> String {
    match NOTE_PREFIX.find(&sentence) {
        Some(m) if m.end() < sentence.len() => capitalized(&sentence[m.end()..]),
        _ => sentence,
    }
}

/// `text` with its first letter uppercase.
fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// A clause at the end of a sentence that only points to documentation whose link was stripped: ", see X for more
/// information." and "; for more information, see X.".
static REFERENCE_CLAUSE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?:,\s*see\s[^,;]+\sfor more information|;\s*for more information,?\s*see\s[^;]+?)[."']*$"#,
    )
    .unwrap()
});

/// Small words GitHub sometimes doubles by mistake ("scope to to get"); a doubled one is written once.
const DOUBLED: [&str; 9] = ["a", "an", "the", "to", "of", "in", "is", "for", "and"];

/// Whitespace runs as one space, and a small word repeated right after itself written once.
fn words(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for w in text.split_whitespace() {
        let doubled = out.last().is_some_and(|prev| {
            prev.eq_ignore_ascii_case(w) && DOUBLED.contains(&w.to_lowercase().as_str())
        });
        if !doubled {
            out.push(w);
        }
    }
    out.join(" ")
}

static CLASSIC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)OAuth (?:app tokens|tokens|apps) and personal access tokens \(classic\)")
        .unwrap()
});
static USE_THIS_ENDPOINT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r" to use this (?:endpoint|tool)( with| on| if|\.|$)").unwrap());

/// GitHub's "OAuth app tokens and personal access tokens (classic) need the repo scope to use this endpoint." as
/// "Classic tokens need the repo scope."; `None` when the sentence is not about classic token scopes.
fn classic_scope(sentence: &str) -> Option<String> {
    if !CLASSIC.is_match(sentence)
        && !sentence.contains("public_repo scope instead")
        && !sentence.starts_with("Classic tokens ")
    {
        return None;
    }
    let text = CLASSIC.replace_all(sentence, "classic tokens");
    let text = USE_THIS_ENDPOINT.replace_all(&text, "$1");
    let text = text.strip_prefix("To call this tool, ").unwrap_or(&text);
    let mut chars = text.chars();
    Some(match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    })
}

/// Words that do not make a sentence say more than a summary ("Lists all workflow runs for a repository", "Creates a
/// new repository", "Lists all of your social accounts", "Lists repositories a user has starred").
const FILLER: [&str; 30] = [
    "a",
    "an",
    "the",
    "all",
    "specific",
    "specified",
    "single",
    "given",
    "new",
    "entire",
    "that",
    "which",
    "who",
    "are",
    "is",
    "has",
    "have",
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
    "you",
    "your",
];

/// Verbs a description uses for its summary's `get`.
const GET: [&str; 3] = ["return", "retrieve", "fetch"];

/// Lowercase words cut to a stem, so "Gets" and "Get", "comments" and "comment", "Searches" and "search",
/// "repositories" and "repository", "watching" and "watched", "configured" and "configure" compare equal. A word
/// ending in `ss` ("access") keeps it. "Returns" is "get".
fn stems(text: &str) -> Vec<String> {
    let cut = |w: &str, suffix: &str, with: &str| {
        w.strip_suffix(suffix)
            .filter(|s| s.len() > 2)
            .map(|s| format!("{s}{with}"))
    };
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty() && !FILLER.contains(w))
        .map(|w| {
            let es = ["shes", "ches", "xes", "zes", "sses"]
                .iter()
                .any(|e| w.ends_with(e));
            let w = if es {
                cut(w, "es", "")
            } else if w.ends_with("ss") {
                None
            } else {
                cut(w, "ies", "y").or_else(|| cut(w, "s", ""))
            }
            .unwrap_or_else(|| w.to_string());
            let w = cut(&w, "ing", "")
                .or_else(|| cut(&w, "ed", ""))
                .unwrap_or(w);
            let w = cut(&w, "e", "").unwrap_or(w);
            if GET.contains(&w.as_str()) || GET.iter().any(|g| g.strip_suffix('e') == Some(&w)) {
                "get".to_string()
            } else {
                w
            }
        })
        .collect()
}

/// Whether a sentence says nothing that `context` (a summary, and the words of its route) does not ("Gets a
/// repository." after "Get a repository").
pub fn restates(context: &str, sentence: &str) -> bool {
    let context = stems(context);
    let words = stems(sentence);
    !words.is_empty() && words.iter().all(|w| context.contains(w))
}

/// One sentence of a description. An essential one (which token scope a call needs) is kept when a description is
/// shortened.
#[derive(Debug, Clone, PartialEq)]
pub struct Sentence {
    pub text: String,
    pub essential: bool,
}

/// A blank line between paragraphs: a sentence always ends there.
static PARAGRAPH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n\s*\n").unwrap());

/// Plain text with each paragraph on one line, whitespace runs as one space and doubled small words written once,
/// as [`tidy_sentences`] reads it; config sentence fixes match this form.
pub fn normalize(text: &str) -> String {
    PARAGRAPH
        .split(text)
        .map(words)
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The sentences of plain text (a paragraph break always ends one) without pointers to stripped links or HTTP
/// details ([`http_detail`]), with classic-token scope sentences shortened (and essential), without a sentence equal
/// to the one before it, and without a first sentence that restates `summary` (pass `""` to keep it). A sentence
/// that ends with `:` ("For example:") takes the next one with it, so the two are kept or dropped together.
pub fn tidy_sentences(text: &str, summary: &str) -> Vec<Sentence> {
    let paragraphs: Vec<String> = PARAGRAPH
        .split(text)
        .map(words)
        .filter(|p| !p.is_empty())
        .collect();
    let count = paragraphs.len();
    // A paragraph followed by another always ends a sentence ("... base64 -w0. SARIF upload ..."); the last one gets
    // a period only after a plain word, not after an example, a URL or a path.
    let all = paragraphs.iter().enumerate().flat_map(|(k, p)| {
        let list = sentences(p);
        let last = list.len().saturating_sub(1);
        list.into_iter().enumerate().map(move |(j, s)| {
            let s = if j != last {
                s.to_string()
            } else if k + 1 < count && !ends_sentence(s) {
                format!("{s}.")
            } else {
                end_sentence(s)
            };
            (k, s)
        })
    });
    let mut out: Vec<Sentence> = Vec::new();
    // An example lead-in of a sentence dropped in the same paragraph ("For a list of media types, see X. For
    // example: application/zip") goes with it, and so does the example it leads to. One that starts a paragraph
    // leads to its own example ("For example, if you want to find ..., your query might look like this:").
    let mut dropped: Option<usize> = None;
    let mut skip_next = false;
    for (paragraph, sentence) in all {
        if std::mem::take(&mut skip_next) {
            continue;
        }
        let example = sentence.starts_with("For example") && sentence.ends_with(':');
        if example && dropped == Some(paragraph) {
            skip_next = true;
            continue;
        }
        dropped = Some(paragraph);
        let Some(sentence) = http_detail(&sentence) else {
            continue;
        };
        if is_reference(&sentence) || PLANS.is_match(&sentence) {
            continue;
        }
        dropped = None;
        // A first sentence that restates the summary goes, but an example after it is still about the tool.
        if out.is_empty() && !summary.is_empty() && restates(summary, &sentence) {
            continue;
        }
        let sentence = without_note(sentence);
        let sentence = match REFERENCE_CLAUSE.find(&sentence) {
            Some(m) if m.start() > 0 && !TOOL_NAME.is_match(m.as_str()) => {
                format!("{}.", &sentence[..m.start()])
            }
            _ => sentence,
        };
        let classic = classic_scope(&sentence);
        let essential = classic.is_some();
        let text = classic.unwrap_or(sentence);
        match out.last_mut() {
            Some(prev) if prev.text == text => {}
            Some(prev) if prev.text.ends_with(':') => {
                prev.text = without_note(format!("{} {text}", prev.text));
                prev.essential |= essential;
            }
            _ => out.push(Sentence { text, essential }),
        }
    }
    out
}

/// [`tidy_sentences`] as one line.
pub fn tidy(text: &str, summary: &str) -> String {
    join(&tidy_sentences(text, summary))
}

fn join(sentences: &[Sentence]) -> String {
    sentences
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whole sentences, in order, within `limit` characters: every essential sentence, and the others up to the first
/// that no longer fits. Only a first sentence that alone is too long is cut, at a word, with `...`. A lead-in left
/// last ("For example:") goes.
pub fn fit(sentences: &[Sentence], limit: usize) -> String {
    let len = |s: &Sentence| s.text.chars().count() + 1;
    let reserved: usize = sentences.iter().filter(|s| s.essential).map(len).sum();
    let mut budget = limit.saturating_sub(reserved) + 1;
    let mut out: Vec<Sentence> = Vec::new();
    let mut full = false;
    for s in sentences {
        if s.essential {
            out.push(s.clone());
        } else if !full && len(s) <= budget {
            budget -= len(s);
            out.push(s.clone());
        } else if !full && out.is_empty() && budget > 40 {
            out.push(Sentence {
                text: one_line(&s.text, budget - 1),
                essential: false,
            });
            full = true;
        } else {
            full = true;
        }
    }
    while out
        .last()
        .is_some_and(|s| !s.essential && s.text.ends_with(':'))
    {
        out.pop();
    }
    join(&out)
}

/// Whether text ends with sentence punctuation, perhaps followed by a closing quote or parenthesis.
fn ends_sentence(text: &str) -> bool {
    text.trim_end_matches(['"', '\'', ')'])
        .ends_with(['.', '!', '?', ':', ';'])
}

/// A period after text that ends in a plain word ("Default: false", "a branch or tag", "branch/PR", "off-topic",
/// "(an existing option)") or in JSON arguments (`{"q": "css"}`), but not after a URL, a path, a number, a quoted
/// value or a list of examples.
pub fn end_sentence(text: &str) -> String {
    let json = text.ends_with('}') || text.ends_with("\"]");
    let last = text
        .split_whitespace()
        .last()
        .unwrap_or("")
        .trim_matches(['`', '*'])
        .trim_end_matches(')')
        .trim_start_matches('(');
    let word = |w: &str| {
        w.starts_with(char::is_alphabetic)
            && w.ends_with(char::is_alphabetic)
            && w.chars()
                .all(|c| c.is_alphabetic() || c == '-' || c == '\'')
    };
    if json
        || last.split('/').all(word)
        || TOOL_NAME.is_match(last) && last.ends_with(char::is_alphanumeric)
    {
        format!("{text}.")
    } else {
        text.to_string()
    }
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
