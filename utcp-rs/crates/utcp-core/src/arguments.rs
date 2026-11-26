//! Checks a call's arguments against the tool's input schema before anything is sent, so a wrong
//! call fails here with the fix instead of reaching the API as a request the API misreads: a
//! missing argument would leave a literal `{repo}` in the URL, an unknown one would become a query
//! parameter the API ignores, and a value of the wrong type or outside the enum is answered with
//! an error that does not name the argument.

use anyhow::{anyhow, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};

/// `{name}` in an HTTP URL template; `${VAR}` placeholders are resolved before this runs.
static URL_PLACEHOLDER_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(^|[^$])\{([A-Za-z0-9_.$-]+)\}").expect("valid placeholder regex"));

/// Arguments utcp itself names in a page's `next_page`, which a tool may not declare: the Azure
/// DevOps continuation token (as a query argument or header field) and common cursor names from a
/// `Link` header.
const PAGING_NAMES: &[&str] = &[
    "continuationToken",
    "x-ms-continuationtoken",
    "page",
    "after",
    "before",
    "cursor",
];

/// The most allowed values an error lists before pointing to `utcp schema`.
const MAX_LISTED_VALUES: usize = 30;

/// Checks (and where it is unambiguous, normalizes) `args` for `tool` (`manual.tool`).
///
/// - Every `required` input without a `default`, and every `{placeholder}` left in an HTTP URL,
///   must be passed, except a placeholder the schema declares and does not require: left out, it
///   is passed as `""`, which [`fill_url`] turns into leaving its path segment out. A required
///   placeholder may not be empty, since that would call a different URL.
/// - Names must be declared inputs, URL placeholders, header or body fields, or paging names, unless
///   the schema has no `properties` (its inputs are unknown) or allows `additionalProperties`.
///   `"properties": {}` declares that the tool takes none.
/// - `integer`, `number` and `boolean` inputs take those values or strings that parse as them;
///   `string` inputs take any single value but not a list or object, except that a list of plain
///   values for a query input is joined with commas (see [`join_query_list`]).
/// - Enum values match ignoring case (a comma-separated string is checked value by value) and are
///   rewritten to the declared spelling; a number passes a string enum (Azure DevOps flag values).
/// - A string given for an `integer`, `number` or `boolean` input (not also declared `string`)
///   becomes that JSON value, so a body field gets `5`, not `"5"`.
/// - Objects whose schema declares `properties` (a JSON body, the objects inside it, and the items
///   of lists of objects) are checked the same way, key by key. An undeclared key that is close to
///   a declared key the object lacks (`feilds`, `Title`) is refused, since the API would ignore it;
///   other undeclared keys are sent, because bodies are often an answer read back with keys the
///   schema leaves out (`_links`), and are named in the returned note. `required` inside an object
///   is left to the API, whose error names the field.
///
/// All problems are reported together, followed by where to read the tool's inputs. On success,
/// the result is a note naming the undeclared keys that will be sent, if any.
pub fn check_arguments(
    tool: &str,
    inputs: &Value,
    provider: &Value,
    args: &mut HashMap<String, Value>,
) -> Result<Option<String>> {
    let props = inputs.get("properties").and_then(Value::as_object);
    let url_names = url_placeholders(provider);
    let mut problems = Vec::new();
    let mut undeclared: Vec<String> = Vec::new();

    let mut declared: BTreeSet<&str> = url_names.iter().map(String::as_str).collect();
    declared.extend(props.into_iter().flat_map(|p| p.keys().map(String::as_str)));
    if let Some(fields) = provider.get("header_fields").and_then(Value::as_array) {
        declared.extend(fields.iter().filter_map(Value::as_str));
    }
    if let Some(field) = provider.get("body_field").and_then(Value::as_str) {
        declared.insert(field);
    }
    let keys: Vec<String> = args.keys().cloned().collect();
    for (from, to) in case_fixes(keys.iter().map(String::as_str), &declared) {
        let value = args.remove(&from).expect("key taken from args");
        args.insert(to, value);
    }

    let mut missing: BTreeSet<&str> = BTreeSet::new();
    if let Some(required) = inputs.get("required").and_then(Value::as_array) {
        for name in required.iter().filter_map(Value::as_str) {
            let has_default = props
                .and_then(|p| p.get(name))
                .is_some_and(|s| s.get("default").is_some());
            if !args.contains_key(name) && !has_default {
                missing.insert(name);
            }
        }
    }
    let required: BTreeSet<&str> = inputs
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let optional_in_url: BTreeSet<&str> = url_names
        .iter()
        .map(String::as_str)
        .filter(|n| props.is_some_and(|p| p.contains_key(*n)) && !required.contains(n))
        .collect();
    let mut left_out: Vec<String> = Vec::new();
    for name in &url_names {
        let optional = optional_in_url.contains(name.as_str());
        match args.get(name.as_str()) {
            None if optional => left_out.push(name.clone()),
            None => {
                missing.insert(name);
            }
            Some(v) if !optional && is_empty(v) => problems.push(format!(
                "'{name}' is empty, but it is part of the URL and required"
            )),
            Some(_) => {}
        }
    }
    if !missing.is_empty() {
        problems.push(format!(
            "missing required argument{}: {}",
            plural(missing.len()),
            missing.iter().copied().collect::<Vec<_>>().join(", ")
        ));
    }

    if let Some(props) = props {
        let open = inputs
            .get("additionalProperties")
            .is_some_and(|v| !matches!(v, Value::Bool(false)));
        if !open {
            let mut known: BTreeSet<&str> = props.keys().map(String::as_str).collect();
            known.extend(url_names.iter().map(String::as_str));
            known.extend(PAGING_NAMES.iter().copied());
            if let Some(fields) = provider.get("header_fields").and_then(Value::as_array) {
                known.extend(fields.iter().filter_map(Value::as_str));
            }
            if let Some(field) = provider.get("body_field").and_then(Value::as_str) {
                known.insert(field);
            }
            let mut unknown: Vec<&String> = args
                .keys()
                .filter(|k| !known.contains(k.as_str()))
                .collect();
            unknown.sort();
            for name in unknown {
                let mut msg = format!("unknown argument '{name}'");
                if props.is_empty() && known.len() == PAGING_NAMES.len() {
                    msg.push_str(" (this tool takes no arguments)");
                } else if let Some(guess) = closest(name, props.keys().map(String::as_str)) {
                    msg.push_str(&format!(" (did you mean '{guess}'?)"));
                }
                problems.push(msg);
            }
        }

        let mut names: Vec<String> = args.keys().cloned().collect();
        names.sort();
        for name in names {
            let Some(schema) = props.get(&name) else {
                continue;
            };
            let value = args.get_mut(&name).expect("name taken from args");
            if optional_in_url.contains(name.as_str()) && is_empty(value) {
                continue;
            }
            if is_http(provider)
                && !url_names.contains(&name)
                && !declared_elsewhere(provider, &name)
            {
                join_query_list(schema, value);
            }
            check_value(&name, schema, value, &mut problems, &mut undeclared, 0);
        }
    }

    if problems.is_empty() {
        for name in left_out {
            args.insert(name, Value::String(String::new()));
        }
        return Ok(undeclared_note(&undeclared));
    }
    Err(anyhow!(
        "{tool}: {}. Nothing was sent. See its inputs with: utcp schema {tool}",
        problems.join("; ")
    ))
}

/// A query input declared `string` that is given a list of plain values (`"definitions": [7, 9]`)
/// becomes the comma-separated string the API reads (`"7,9"`), as Azure DevOps and GitHub declare
/// their list filters. The enum check then sees each value.
fn join_query_list(schema: &Value, value: &mut Value) {
    let types = declared_types(schema);
    if types.is_empty() || types.iter().any(|t| t != "string") {
        return;
    }
    let Value::Array(items) = value else {
        return;
    };
    if items
        .iter()
        .any(|v| v.is_array() || v.is_object() || v.is_null())
    {
        return;
    }
    let joined = items
        .iter()
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(",");
    *value = Value::String(joined);
}

/// Whether `name` goes to a header or is the JSON body rather than the query string.
fn declared_elsewhere(provider: &Value, name: &str) -> bool {
    let header = provider
        .get("header_fields")
        .and_then(Value::as_array)
        .is_some_and(|f| f.iter().any(|h| h.as_str() == Some(name)));
    header || provider.get("body_field").and_then(Value::as_str) == Some(name)
}

fn is_http(provider: &Value) -> bool {
    provider
        .get("provider_type")
        .or_else(|| provider.get("call_template_type"))
        .or_else(|| provider.get("type"))
        .and_then(Value::as_str)
        .is_none_or(|t| t.eq_ignore_ascii_case("http"))
}

fn is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        _ => false,
    }
}

/// Fills the `{name}` placeholders of an HTTP `url` from `args`, URL-encoding each value, and
/// returns the URL and the names it used. An empty (or null) value leaves its placeholder's whole
/// path segment out, as Azure DevOps routes with optional segments expect:
/// `…/timeline/{timelineId}?v=1` becomes `…/timeline?v=1`, `{project}/{team}/_apis` becomes
/// `{project}/_apis`. A placeholder inside a segment (`v{n}.json`) is just removed.
pub fn fill_url(url: &str, args: &HashMap<String, Value>) -> (String, Vec<String>) {
    let mut url = url.to_string();
    let mut used = Vec::new();
    let mut keys: Vec<&String> = args.keys().collect();
    keys.sort();
    for key in keys {
        let placeholder = format!("{{{key}}}");
        if !url.contains(&placeholder) {
            continue;
        }
        let raw = match &args[key] {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
        url = if raw.trim().is_empty() {
            without_placeholder(&url, &placeholder)
        } else {
            url.replace(&placeholder, &urlencoding::encode(&raw))
        };
        used.push(key.clone());
    }
    (url, used)
}

fn without_placeholder(url: &str, placeholder: &str) -> String {
    let mut out = url.to_string();
    while let Some(start) = out.find(placeholder) {
        let end = start + placeholder.len();
        let whole_segment = out[..start].ends_with('/')
            && matches!(out[end..].chars().next(), None | Some('/' | '?' | '#'));
        let from = if whole_segment { start - 1 } else { start };
        out.replace_range(from..end, "");
    }
    out
}

/// The `{name}` placeholders in an HTTP provider's URL.
fn url_placeholders(provider: &Value) -> BTreeSet<String> {
    let url = provider.get("url").and_then(Value::as_str).unwrap_or("");
    if !is_http(provider) {
        return BTreeSet::new();
    }
    URL_PLACEHOLDER_RE
        .captures_iter(url)
        .map(|c| c[2].to_string())
        .collect()
}

/// How deep `check_value` follows objects and lists inside a value.
const MAX_DEPTH: usize = 16;

/// The most undeclared keys the note names before counting the rest.
const MAX_NOTED_KEYS: usize = 8;

/// One value (`name` is its path, as `body.fields` or `body[0].op`) against its schema. Problems go
/// to `problems`, undeclared keys that will be sent to `undeclared`. May rewrite an enum value to its
/// declared spelling and a numeric or boolean string to that JSON value.
fn check_value(
    name: &str,
    schema: &Value,
    value: &mut Value,
    problems: &mut Vec<String>,
    undeclared: &mut Vec<String>,
    depth: usize,
) {
    if value.is_null() || depth > MAX_DEPTH {
        return;
    }
    let types = declared_types(schema);
    if !types.is_empty() && !types.iter().any(|t| type_accepts(t, value)) {
        problems.push(format!(
            "'{name}' must be {}, not {}",
            types
                .iter()
                .map(|t| article(t))
                .collect::<Vec<_>>()
                .join(" or "),
            describe(value)
        ));
        return;
    }
    if let Some(allowed) = string_enum(schema) {
        if let Some(problem) = check_enum(name, &allowed, value) {
            problems.push(problem);
        }
        return;
    }
    convert_text(&types, value);
    match value {
        Value::Object(map) => {
            if let Some(props) = schema.get("properties").and_then(Value::as_object) {
                if !props.is_empty() {
                    let open = schema
                        .get("additionalProperties")
                        .is_some_and(|v| !matches!(v, Value::Bool(false)));
                    check_object(name, props, open, map, problems, undeclared, depth);
                }
            }
        }
        Value::Array(values) => {
            let Some(items) = schema.get("items") else {
                return;
            };
            for (i, v) in values.iter_mut().enumerate() {
                let before = problems.len();
                check_value(
                    &format!("{name}[{i}]"),
                    items,
                    v,
                    problems,
                    undeclared,
                    depth + 1,
                );
                if problems.len() > before {
                    // One bad item is enough to show what is wrong; the rest are likely the same.
                    return;
                }
            }
        }
        _ => {}
    }
}

/// The keys of an object whose schema declares `props`: a key close to a declared key the object
/// lacks is a problem, another undeclared key is noted (unless the schema is `open`), and declared
/// keys are checked against their schemas.
fn check_object(
    name: &str,
    props: &serde_json::Map<String, Value>,
    open: bool,
    map: &mut serde_json::Map<String, Value>,
    problems: &mut Vec<String>,
    undeclared: &mut Vec<String>,
    depth: usize,
) {
    let declared: BTreeSet<&str> = props.keys().map(String::as_str).collect();
    let fixes = case_fixes(map.keys().map(String::as_str), &declared);
    for (from, to) in fixes {
        let value = map.remove(&from).expect("key taken from map");
        map.insert(to, value);
    }
    let mut keys: Vec<String> = map.keys().cloned().collect();
    keys.sort();
    for key in &keys {
        let path = format!("{name}.{key}");
        match props.get(key) {
            Some(schema) => {
                let value = map.get_mut(key).expect("key taken from map");
                check_value(&path, schema, value, problems, undeclared, depth + 1);
            }
            None => {
                let absent = props
                    .keys()
                    .map(String::as_str)
                    .filter(|k| !map.contains_key(*k));
                match closest(key, absent) {
                    Some(guess) => problems.push(format!(
                        "'{path}' is not a field here (did you mean '{name}.{guess}'?)"
                    )),
                    None if !open => undeclared.push(path),
                    None => {}
                }
            }
        }
    }
}

/// Keys to rename to their declared spelling: a key that is not declared but matches exactly one
/// declared name ignoring case, when that name is not also given.
fn case_fixes<'k>(
    keys: impl Iterator<Item = &'k str> + Clone,
    declared: &BTreeSet<&str>,
) -> Vec<(String, String)> {
    let given: BTreeSet<&str> = keys.clone().collect();
    keys.filter(|k| !declared.contains(k))
        .filter_map(|k| {
            let mut matches = declared.iter().filter(|d| d.eq_ignore_ascii_case(k));
            match (matches.next(), matches.next()) {
                (Some(d), None) if !given.contains(d) => Some((k.to_string(), d.to_string())),
                _ => None,
            }
        })
        .collect()
}

/// A string given for an `integer`, `number` or `boolean` that is not also a `string`: that JSON
/// value. `type_accepts` has already checked that it parses.
fn convert_text(types: &[String], value: &mut Value) {
    let Value::String(text) = value else {
        return;
    };
    if types.is_empty() || types.iter().any(|t| t == "string") {
        return;
    }
    let text = text.trim();
    let converted = types.iter().find_map(|t| match t.as_str() {
        "integer" => text
            .parse::<i64>()
            .map(Value::from)
            .or_else(|_| text.parse::<u64>().map(Value::from))
            .ok(),
        "number" => text.parse::<serde_json::Number>().ok().map(Value::Number),
        "boolean" => match text.to_ascii_lowercase().as_str() {
            "true" => Some(Value::Bool(true)),
            "false" => Some(Value::Bool(false)),
            _ => None,
        },
        _ => None,
    });
    if let Some(converted) = converted {
        *value = converted;
    }
}

/// The note naming the undeclared keys a call sends.
fn undeclared_note(undeclared: &[String]) -> Option<String> {
    if undeclared.is_empty() {
        return None;
    }
    let mut listed: Vec<&str> = undeclared
        .iter()
        .take(MAX_NOTED_KEYS)
        .map(String::as_str)
        .collect();
    let more = undeclared.len().saturating_sub(MAX_NOTED_KEYS);
    let extra = if more > 0 {
        format!(" and {more} more")
    } else {
        String::new()
    };
    listed.dedup();
    Some(format!(
        "sent as given, but not in the tool's schema (the API may ignore {}): {}{extra}",
        if undeclared.len() == 1 { "it" } else { "them" },
        listed.join(", ")
    ))
}

fn declared_types(schema: &Value) -> Vec<String> {
    match schema.get("type") {
        Some(Value::String(t)) => vec![t.clone()],
        Some(Value::Array(ts)) => ts
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn type_accepts(ty: &str, value: &Value) -> bool {
    let text = value.as_str().map(str::trim);
    match ty {
        "integer" => {
            value.as_i64().is_some()
                || value.as_u64().is_some()
                || value.as_f64().is_some_and(|f| f.fract() == 0.0)
                || text.is_some_and(|s| s.parse::<i128>().is_ok())
        }
        "number" => value.is_number() || text.is_some_and(|s| s.parse::<f64>().is_ok()),
        "boolean" => {
            value.is_boolean()
                || text.is_some_and(|s| {
                    s.eq_ignore_ascii_case("true") || s.eq_ignore_ascii_case("false")
                })
        }
        "string" => !value.is_array() && !value.is_object(),
        // Lists and objects: a single value or JSON text is left for the API to judge.
        _ => true,
    }
}

fn article(ty: &str) -> String {
    match ty {
        "integer" | "array" | "object" => format!("an {ty}"),
        "null" => "null".to_string(),
        other => format!("a {other}"),
    }
}

fn describe(value: &Value) -> String {
    match value {
        Value::Array(_) => "a list".to_string(),
        Value::Object(_) => "an object".to_string(),
        other => other.to_string(),
    }
}

fn string_enum(schema: &Value) -> Option<Vec<String>> {
    let values = schema.get("enum")?.as_array()?;
    let strings: Vec<String> = values
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    (!strings.is_empty() && strings.len() == values.len()).then_some(strings)
}

/// A value of a string enum: matched ignoring case, a comma-separated string value by value.
fn check_enum(name: &str, allowed: &[String], value: &mut Value) -> Option<String> {
    let find = |s: &str| allowed.iter().find(|a| a.eq_ignore_ascii_case(s.trim()));
    let all_words = allowed.iter().all(|a| a.parse::<f64>().is_err());
    let text = match value {
        Value::String(s) => s.clone(),
        Value::Number(_) if all_words => return None,
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => return None,
    };
    if all_words && text.trim().parse::<f64>().is_ok() {
        return None;
    }
    if let Some(exact) = find(&text) {
        *value = Value::String(exact.clone());
        return None;
    }
    let parts: Vec<&str> = text.split(',').collect();
    if parts.len() > 1 {
        let matched: Vec<Option<&String>> = parts.iter().map(|p| find(p)).collect();
        if matched.iter().all(Option::is_some) {
            let joined: Vec<&str> = matched.into_iter().flatten().map(String::as_str).collect();
            *value = Value::String(joined.join(","));
            return None;
        }
        let bad: Vec<&str> = parts
            .iter()
            .filter(|p| find(p).is_none())
            .map(|p| p.trim())
            .collect();
        return Some(enum_problem(name, allowed, &bad.join(", ")));
    }
    Some(enum_problem(name, allowed, &text))
}

fn enum_problem(name: &str, allowed: &[String], got: &str) -> String {
    let mut listed: Vec<&str> = allowed
        .iter()
        .take(MAX_LISTED_VALUES)
        .map(String::as_str)
        .collect();
    if allowed.len() > MAX_LISTED_VALUES {
        listed.push("…");
    }
    let mut msg = format!(
        "'{name}' can't be \"{got}\"; it takes: {}",
        listed.join(", ")
    );
    if let Some(guess) = closest(got, allowed.iter().map(String::as_str)) {
        msg.push_str(&format!(" (did you mean '{guess}'?)"));
    }
    msg
}

/// The declared name closest to `name`: same letters ignoring case, or a small edit distance.
fn closest<'a>(name: &str, candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let lower = name.to_ascii_lowercase();
    let limit = (name.chars().count() / 3).clamp(1, 3);
    candidates
        .map(|c| (distance(&lower, &c.to_ascii_lowercase()), c))
        .filter(|(d, _)| *d <= limit)
        .min_by_key(|(d, c)| (*d, c.len()))
        .map(|(_, c)| c)
}

/// Up to `max` of `candidates` that look like `name`, best first, ignoring case: those containing
/// it (`get_us` -> `get_user`), then those having each of its `_`/`-` words, also as a word's start
/// (`list_comments` -> `list_issue_comments`), then near-typos (`trigers` -> `triggers`).
pub fn similar_names<'a>(name: &str, candidates: &[&'a str], max: usize) -> Vec<&'a str> {
    let lower = name.to_ascii_lowercase();
    let is_sep = |c: char| c == '_' || c == '-';
    let words: Vec<&str> = lower.split(is_sep).filter(|w| !w.is_empty()).collect();
    let limit = (name.chars().count() / 3).clamp(1, 3);
    let mut ranked: Vec<(usize, usize, usize, &str)> = candidates
        .iter()
        .filter_map(|&c| {
            let cl = c.to_ascii_lowercase();
            if cl == lower {
                return None;
            }
            if !lower.is_empty() && cl.contains(&lower) {
                return Some((0, 0, c.len(), c));
            }
            if words.len() > 1
                && words
                    .iter()
                    .all(|w| cl.split(is_sep).any(|cw| cw.starts_with(w)))
            {
                return Some((1, 0, c.len(), c));
            }
            let d = distance(&lower, &cl);
            (d <= limit).then_some((2, d, c.len(), c))
        })
        .collect();
    ranked.sort();
    ranked.into_iter().take(max).map(|r| r.3).collect()
}

/// Edit distance between two short strings, counting two swapped neighbors (`vaule`) as one edit
/// (optimal string alignment).
fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = best;
        }
    }
    d[a.len()][b.len()]
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn github_repo_tool() -> (Value, Value) {
        let inputs = json!({
            "type": "object",
            "properties": {
                "owner": {"type": "string"},
                "repo": {"type": "string"},
                "state": {"type": "string", "enum": ["open", "closed", "all"]},
                "per_page": {"type": "integer"},
                "draft": {"type": "boolean"},
                "labels": {"type": "array", "items": {"type": "string", "enum": ["bug", "docs"]}},
            },
            "required": ["owner", "repo"],
        });
        let provider = json!({
            "call_template_type": "http",
            "url": "https://api.github.com/repos/{owner}/{repo}/issues",
            "http_method": "GET",
        });
        (inputs, provider)
    }

    fn args(v: Value) -> HashMap<String, Value> {
        serde_json::from_value(v).unwrap()
    }

    fn check(v: Value) -> (Result<Option<String>>, HashMap<String, Value>) {
        let (inputs, provider) = github_repo_tool();
        let mut a = args(v);
        let r = check_arguments("gh.list_issues", &inputs, &provider, &mut a);
        (r, a)
    }

    fn err(v: Value) -> String {
        format!("{:#}", check(v).0.unwrap_err())
    }

    #[test]
    fn a_correct_call_passes() {
        let (r, a) = check(json!({"owner": "o", "repo": "r", "per_page": "30", "draft": "true"}));
        r.unwrap();
        // A string for an integer becomes the number; query and URL values print the same.
        assert_eq!(a["per_page"], json!(30));
    }

    #[test]
    fn a_list_for_a_string_query_input_is_joined_with_commas() {
        let (r, a) = check(json!({"owner": "o", "repo": "r", "state": ["open", "Closed"]}));
        r.unwrap();
        assert_eq!(a["state"], json!("open,closed"));
        let e = err(json!({"owner": "o", "repo": "r", "state": ["open", "shut"]}));
        assert!(e.contains("shut"), "{e}");

        let inputs = json!({"type": "object", "properties": {
            "definitions": {"type": "string"},
            "token": {"type": "string"},
            "body": {"type": "string"},
            "pair": {"type": "string"},
        }});
        let provider = json!({"call_template_type": "http", "url": "https://h/builds",
            "header_fields": ["token"], "body_field": "body"});
        let mut a = args(json!({"definitions": [7, 9]}));
        check_arguments("t.get_builds", &inputs, &provider, &mut a).unwrap();
        assert_eq!(a["definitions"], json!("7,9"));
        for bad in [
            json!({"token": ["a", "b"]}),
            json!({"body": ["a"]}),
            json!({"pair": [[1, 2]]}),
        ] {
            let mut a = args(bad.clone());
            let e = check_arguments("t.get_builds", &inputs, &provider, &mut a).unwrap_err();
            assert!(
                format!("{e:#}").contains("must be a string, not a list"),
                "{bad}"
            );
        }
    }

    #[test]
    fn missing_arguments_are_named_before_sending() {
        let e = err(json!({"owner": "Faraclas"}));
        assert!(e.contains("missing required argument: repo"), "{e}");
        assert!(e.contains("Nothing was sent"), "{e}");
        assert!(e.contains("utcp schema gh.list_issues"), "{e}");
    }

    #[test]
    fn a_url_placeholder_must_be_passed_even_when_not_required() {
        let inputs = json!({"type": "object", "properties": {"definitionId": {"type": "integer"}}});
        let provider =
            json!({"call_template_type": "http", "url": "https://h/{DefinitionId}/tags"});
        let mut a = args(json!({"definitionId": 7}));
        let e = format!(
            "{:#}",
            check_arguments("p.t", &inputs, &provider, &mut a).unwrap_err()
        );
        assert!(e.contains("missing required argument: DefinitionId"), "{e}");
        // The placeholder's own name is accepted.
        let mut a = args(json!({"DefinitionId": 7}));
        check_arguments("p.t", &inputs, &provider, &mut a).unwrap();
    }

    fn timeline_tool() -> (Value, Value) {
        let inputs = json!({
            "type": "object",
            "properties": {
                "buildId": {"type": "integer"},
                "timelineId": {"type": "string"},
                "changeId": {"type": "integer"},
            },
            "required": ["buildId"],
        });
        let provider = json!({
            "call_template_type": "http",
            "url": "https://h/_apis/build/builds/{buildId}/timeline/{timelineId}?api-version=7.1",
        });
        (inputs, provider)
    }

    #[test]
    fn an_optional_url_placeholder_left_out_leaves_its_segment_out() {
        let (inputs, provider) = timeline_tool();
        let url = provider["url"].as_str().unwrap();
        let mut a = args(json!({"buildId": 7}));
        check_arguments("p.t", &inputs, &provider, &mut a).unwrap();
        assert_eq!(a["timelineId"], json!(""));
        let (filled, used) = fill_url(url, &a);
        assert_eq!(
            filled,
            "https://h/_apis/build/builds/7/timeline?api-version=7.1"
        );
        assert_eq!(used, vec!["buildId", "timelineId"]);
        // Passed as "" or null, it is the same call; passed, it is filled.
        for empty in [json!(""), json!(null)] {
            let mut a = args(json!({"buildId": 7, "timelineId": empty}));
            check_arguments("p.t", &inputs, &provider, &mut a).unwrap();
            assert_eq!(fill_url(url, &a).0, filled);
        }
        let mut a = args(json!({"buildId": 7, "timelineId": "a b"}));
        check_arguments("p.t", &inputs, &provider, &mut a).unwrap();
        assert_eq!(
            fill_url(url, &a).0,
            "https://h/_apis/build/builds/7/timeline/a%20b?api-version=7.1"
        );
    }

    #[test]
    fn a_required_url_placeholder_may_not_be_empty() {
        let (inputs, provider) = timeline_tool();
        let mut a = args(json!({"buildId": " "}));
        let e = format!(
            "{:#}",
            check_arguments("p.t", &inputs, &provider, &mut a).unwrap_err()
        );
        assert!(
            e.contains("'buildId' is empty, but it is part of the URL"),
            "{e}"
        );
        let e = err(json!({"owner": "Faraclas", "repo": ""}));
        assert!(e.contains("'repo' is empty"), "{e}");
    }

    #[test]
    fn fill_url_leaves_out_middle_and_inner_segments() {
        let a = args(json!({"project": "P", "team": ""}));
        assert_eq!(
            fill_url("https://h/{project}/{team}/_apis/work", &a).0,
            "https://h/P/_apis/work"
        );
        let a = args(json!({"n": "", "x": 1}));
        assert_eq!(
            fill_url("https://h/v{n}.json/{x}", &a).0,
            "https://h/v.json/1"
        );
    }

    #[test]
    fn unknown_names_are_refused_with_a_suggestion() {
        let e = err(json!({"owner": "o", "repoo": "r"}));
        assert!(e.contains("missing required argument: repo"), "{e}");
        assert!(
            e.contains("unknown argument 'repoo' (did you mean 'repo'?)"),
            "{e}"
        );
        let e = err(json!({"owner": "o", "repo": "r", "stat": "open"}));
        assert!(
            e.contains("unknown argument 'stat' (did you mean 'state'?)"),
            "{e}"
        );
        let e = err(json!({"owner": "o", "repo": "r", "zzzzzz": 1}));
        assert!(
            e.contains("unknown argument 'zzzzzz'") && !e.contains("did you mean"),
            "{e}"
        );
    }

    #[test]
    fn paging_names_and_open_schemas_are_accepted() {
        check(json!({"owner": "o", "repo": "r", "page": 2}))
            .0
            .unwrap();
        check(json!({"owner": "o", "repo": "r", "continuationToken": "x"}))
            .0
            .unwrap();
        let open = json!({"type": "object", "properties": {"a": {}}, "additionalProperties": true});
        let mut a = args(json!({"anything": 1}));
        check_arguments("m.t", &open, &json!({"type": "cli"}), &mut a).unwrap();
        let unknown = json!({"type": "object"});
        check_arguments("m.t", &unknown, &json!({"type": "cli"}), &mut a).unwrap();
    }

    #[test]
    fn a_tool_that_declares_no_inputs_takes_none() {
        let none = json!({"type": "object", "properties": {}});
        let provider = json!({"call_template_type": "http", "url": "https://api.github.com/user"});
        let mut a = args(json!({"username": "octocat"}));
        let e = format!(
            "{:#}",
            check_arguments("gh.me", &none, &provider, &mut a).unwrap_err()
        );
        assert!(
            e.contains("unknown argument 'username' (this tool takes no arguments)"),
            "{e}"
        );
        // Paging names still pass, and an input-less tool with a URL placeholder takes it.
        let mut a = args(json!({"page": 2}));
        check_arguments("gh.me", &none, &provider, &mut a).unwrap();
        let provider = json!({"call_template_type": "http", "url": "https://h/{org}/x"});
        let mut a = args(json!({"org": "o"}));
        check_arguments("m.t", &none, &provider, &mut a).unwrap();
    }

    #[test]
    fn types_are_checked_with_numeric_strings_allowed() {
        let e = err(json!({"owner": "o", "repo": "r", "per_page": "lots"}));
        assert!(
            e.contains("'per_page' must be an integer, not \"lots\""),
            "{e}"
        );
        let e = err(json!({"owner": "o", "repo": "r", "draft": "maybe"}));
        assert!(e.contains("'draft' must be a boolean"), "{e}");
        let e = err(json!({"owner": ["o"], "repo": "r"}));
        assert!(e.contains("'owner' must be a string, not a list"), "{e}");
        check(json!({"owner": 5, "repo": "r", "per_page": 30, "draft": false, "state": null}))
            .0
            .unwrap();
    }

    #[test]
    fn enums_ignore_case_and_take_the_declared_spelling() {
        let e = err(json!({"owner": "o", "repo": "r", "state": "opened"}));
        assert!(
            e.contains("'state' can't be \"opened\"; it takes: open, closed, all"),
            "{e}"
        );
        assert!(e.contains("did you mean 'open'"), "{e}");
        let (r, a) = check(json!({"owner": "o", "repo": "r", "state": "Closed"}));
        r.unwrap();
        assert_eq!(a["state"], json!("closed"));
        let (r, a) = check(json!({"owner": "o", "repo": "r", "labels": ["BUG", "docs"]}));
        r.unwrap();
        assert_eq!(a["labels"], json!(["bug", "docs"]));
        let e = err(json!({"owner": "o", "repo": "r", "labels": ["feature"]}));
        assert!(e.contains("'labels[0]' can't be \"feature\""), "{e}");
    }

    #[test]
    fn flag_enums_take_comma_separated_values_and_numbers() {
        let inputs = json!({"type": "object", "properties": {"queryFlags": {
            "type": "string", "enum": ["none", "includeDeleted", "includeAll"]}}});
        let provider = json!({"call_template_type": "http", "url": "https://h/x"});
        let mut a = args(json!({"queryFlags": "includedeleted, INCLUDEALL"}));
        check_arguments("m.t", &inputs, &provider, &mut a).unwrap();
        assert_eq!(a["queryFlags"], json!("includeDeleted,includeAll"));
        let mut a = args(json!({"queryFlags": "3"}));
        check_arguments("m.t", &inputs, &provider, &mut a).unwrap();
        let mut a = args(json!({"queryFlags": 3}));
        check_arguments("m.t", &inputs, &provider, &mut a).unwrap();
        let mut a = args(json!({"queryFlags": "none,bogus"}));
        let e = format!(
            "{:#}",
            check_arguments("m.t", &inputs, &provider, &mut a).unwrap_err()
        );
        assert!(e.contains("'queryFlags' can't be \"bogus\""), "{e}");
    }

    #[test]
    fn non_http_tools_have_no_url_placeholders() {
        let inputs = json!({"type": "object", "properties": {"q": {"type": "string"}}});
        let provider = json!({"call_template_type": "cli", "url": "{x}"});
        let mut a = args(json!({"q": "x"}));
        check_arguments("m.t", &inputs, &provider, &mut a).unwrap();
    }

    /// A tool with a JSON body like Azure DevOps' `get_work_items_batch` and a JSON Patch list like
    /// `update_work_item`.
    fn body_tool(body: Value) -> (Value, Value) {
        (
            json!({
                "type": "object",
                "properties": {"project": {"type": "string"}, "body": body},
                "required": ["project", "body"],
            }),
            json!({
                "call_template_type": "http",
                "http_method": "POST",
                "url": "https://dev.azure.com/o/{project}/_apis/wit/workitemsbatch",
                "body_field": "body",
            }),
        )
    }

    fn batch_body() -> Value {
        json!({
            "type": "object",
            "properties": {
                "$expand": {"type": "string", "enum": ["none", "relations", "fields", "links", "all"]},
                "asOf": {"type": "string", "format": "date-time"},
                "fields": {"type": "array", "items": {"type": "string"}},
                "ids": {"type": "array", "items": {"type": "integer"}},
            },
        })
    }

    fn check_body(
        body_schema: Value,
        v: Value,
    ) -> (Result<Option<String>>, HashMap<String, Value>) {
        let (inputs, provider) = body_tool(body_schema);
        let mut a = args(v);
        let r = check_arguments("azdo_work.get_work_items_batch", &inputs, &provider, &mut a);
        (r, a)
    }

    #[test]
    fn a_misspelled_body_key_is_refused_with_the_fix() {
        let (r, _) = check_body(
            batch_body(),
            json!({"project": "P", "body": {"ids": [1], "feilds": ["System.Title"]}}),
        );
        let e = format!("{:#}", r.unwrap_err());
        assert!(
            e.contains("'body.feilds' is not a field here (did you mean 'body.fields'?)"),
            "{e}"
        );
        assert!(e.contains("Nothing was sent"), "{e}");
        // A key that differs only in case takes the declared spelling, at the top level too.
        let (r, a) = check_body(batch_body(), json!({"Project": "P", "body": {"IDs": [1]}}));
        assert_eq!(r.unwrap(), None);
        assert_eq!(a["project"], "P");
        assert_eq!(a["body"], json!({"ids": [1]}));
        // Not when the declared spelling is given as well.
        let (r, _) = check_body(
            batch_body(),
            json!({"project": "P", "body": {"ids": [1], "IDs": [2]}}),
        );
        assert!(r.unwrap().unwrap().contains("body.IDs"));
    }

    #[test]
    fn other_undeclared_body_keys_are_sent_and_noted() {
        // A definition read back carries keys the schema leaves out; they are sent as given.
        let (r, a) = check_body(
            batch_body(),
            json!({"project": "P", "body": {"ids": [1], "_links": {}, "url": "x"}}),
        );
        let note = r.unwrap().unwrap();
        assert!(note.contains("body._links, body.url"), "{note}");
        assert_eq!(a["body"]["url"], "x");
        // A close key that the object already has under its declared name is not a misspelling.
        let (r, _) = check_body(
            batch_body(),
            json!({"project": "P", "body": {"ids": [1], "id": 3}}),
        );
        assert!(r.unwrap().unwrap().contains("body.id"));
        // An open schema takes any key without a note; a body with no declared keys is not checked.
        let mut open = batch_body();
        open["additionalProperties"] = json!(true);
        let (r, _) = check_body(open, json!({"project": "P", "body": {"x": 1}}));
        assert_eq!(r.unwrap(), None);
        let (r, _) = check_body(
            json!({"type": "object"}),
            json!({"project": "P", "body": {"x": 1}}),
        );
        assert_eq!(r.unwrap(), None);
    }

    #[test]
    fn body_values_are_checked_and_converted() {
        let (r, a) = check_body(
            batch_body(),
            json!({"project": "P", "body": {"ids": ["7", 8], "$expand": "Relations"}}),
        );
        r.unwrap();
        assert_eq!(a["body"]["ids"], json!([7, 8]));
        assert_eq!(a["body"]["$expand"], "relations");
        let (r, _) = check_body(
            batch_body(),
            json!({"project": "P", "body": {"ids": ["seven"]}}),
        );
        let e = format!("{:#}", r.unwrap_err());
        assert!(
            e.contains("'body.ids[0]' must be an integer, not \"seven\""),
            "{e}"
        );
        let (r, _) = check_body(
            batch_body(),
            json!({"project": "P", "body": {"$expand": "every"}}),
        );
        assert!(format!("{:#}", r.unwrap_err()).contains("'body.$expand' can't be \"every\""));
    }

    #[test]
    fn items_of_a_list_body_are_checked() {
        let patch = json!({
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "op": {"type": "string", "enum": ["add", "remove", "replace", "move", "copy", "test"]},
                    "path": {"type": "string"},
                    "value": {},
                },
            },
        });
        let (r, a) = check_body(
            patch.clone(),
            json!({"project": "P", "body": [{"op": "Add", "path": "/fields/System.Title", "value": "t"}]}),
        );
        assert_eq!(r.unwrap(), None);
        assert_eq!(a["body"][0]["op"], "add");
        let (r, _) = check_body(
            patch.clone(),
            json!({"project": "P", "body": [{"op": "add", "path": "/a", "vaule": 1}]}),
        );
        let e = format!("{:#}", r.unwrap_err());
        assert!(
            e.contains("'body[0].vaule' is not a field here (did you mean 'body[0].value'?)"),
            "{e}"
        );
        // `from` (JSON Patch move/copy) is left out of some schemas: sent, with a note.
        let (r, _) = check_body(
            patch,
            json!({"project": "P", "body": [{"op": "move", "from": "/a", "path": "/b"}]}),
        );
        assert!(r.unwrap().unwrap().contains("body[0].from"));
    }

    #[test]
    fn strings_become_numbers_and_booleans_only_where_the_schema_says_so() {
        let schema = json!({
            "type": "object",
            "properties": {
                "n": {"type": "integer"},
                "b": {"type": "boolean"},
                "either": {"type": ["string", "integer"]},
                "f": {"type": "number"},
            },
        });
        let (r, a) = check_body(
            schema,
            json!({"project": "P", "body": {"n": " 5 ", "b": "TRUE", "either": "5", "f": "0.5"}}),
        );
        r.unwrap();
        assert_eq!(
            a["body"],
            json!({"n": 5, "b": true, "either": "5", "f": 0.5})
        );
    }

    #[test]
    fn similar_names_rank_contains_then_words_then_typos() {
        let tools = [
            "list_issue_comments",
            "list_comments_for_repo",
            "get_user",
            "get_user_by_id",
            "create_issue",
            "delete_issue",
        ];
        // Contains the name first (shortest first), then names with all of its words.
        assert_eq!(
            similar_names("list_comments", &tools, 5),
            vec!["list_comments_for_repo", "list_issue_comments"]
        );
        assert_eq!(
            similar_names("get_us", &tools, 5),
            vec!["get_user", "get_user_by_id"]
        );
        // A typo within the edit-distance limit; case is ignored; the exact name is not a guess.
        assert_eq!(similar_names("creat_isue", &tools, 5), vec!["create_issue"]);
        assert_eq!(similar_names("GET_USER", &tools, 5), vec!["get_user_by_id"]);
        assert!(similar_names("zzz", &tools, 5).is_empty());
        assert_eq!(similar_names("issue", &tools, 2).len(), 2);
    }
}
