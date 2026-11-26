//! Turn GitHub's OpenAPI 3 description into utcp HTTP manuals plus a report. Nothing here touches the network.
//!
//! Each operation is either left out (a rule of `Config`, with its reason) or becomes one tool in the manual of its
//! `x-github` category. Request bodies are flat: the fields of a JSON object body are the tool's arguments next to
//! the path parameters, as in GitHub's documentation (`create_issue` takes `owner`, `repo`, `title`, `body`). A body
//! that is not an object, or that comes with query parameters, is passed whole as the argument `body` instead.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;
use serde_json::{json, Map, Value};

use crate::config::{Config, Variant, PAT_VAR};
use crate::links;
use crate::names;
use crate::text::{
    end_sentence, fit, named_tools, normalize, one_line, plain, restates, sentences,
    tidy_sentences, truthy, Sentence,
};

/// Starts the description of a write that has not yet worked in a live test against a throwaway repository.
pub const UNTESTED: &str = "(untested)";
/// Follows the summary of a DELETE that deletes something ("Delete a release"); see [`delete_note`].
pub const DELETE_NOTE: &str = "Delete only what the user asked for or what you created.";
/// Ends the description of every search: GitHub's limit, which next_page does not show.
pub const SEARCH_NOTE: &str =
    "A search finds at most 1,000 results in all, over all pages; narrow the query to \
                               reach others.";
/// The output of a tool that answers with a file.
pub const FILE_ANSWER: &str = "A file. utcp saves it (to the Downloads folder, or the path given with --out) and \
                               prints {\"type\": \"file\", \"path\", \"mime_type\", \"bytes\", \"filename\"}.";
/// Ends the output description of a tool that has a successful answer with no content next to one with content.
pub const NULL_TOO: &str = "Can also be null (an answer with no content).";
/// The output of a write whose successful answer has no content.
pub const NO_CONTENT: &str = "No content: the call answers null when it worked.";
/// Ends the description of a yes/no check: a GET whose successful answer has no content ("Check if a user is
/// blocked"). GitHub answers no with 404, which utcp reports as a failed call.
pub const YES_NO: &str = "The answer is null if so; if not, the call fails with 404 Not Found.";
/// The output of a yes/no check; see [`YES_NO`].
pub const YES_NO_ANSWER: &str = "Null means yes; no is a failed call with 404 Not Found.";
pub const ACCEPT: &str = "application/vnd.github+json";
const DEPRECATED: &str = "deprecated by GitHub";
const READ_ONLY: &str = "write (--read-only)";
/// utcp refuses a manual hint longer than this (`MAX_MANUAL_HINT_CHARS` in utcp-core).
pub const UTCP_MAX_HINT_CHARS: usize = 60;
const SCHEMA_DEPTH: usize = 5;
/// Longest tool and field descriptions; longer ones keep their first whole sentences (and every classic-token scope
/// sentence).
const MAX_DESCRIPTION: usize = 1200;
const MAX_FIELD_DESCRIPTION: usize = 600;
/// Where a description stops being about the operation: token-type boilerplate, and media types the fixed
/// `Accept` header does not ask for.
const DESCRIPTION_ENDS: [&str; 2] = [
    "**Fine-grained access tokens for",
    "This endpoint supports the following custom media types",
];
const HTTP_METHODS: [&str; 7] = ["get", "put", "post", "patch", "delete", "head", "options"];
const SCHEMA_FORMATS: [&str; 7] = [
    "int32",
    "int64",
    "date-time",
    "date",
    "uri",
    "email",
    "binary",
];

#[derive(Debug)]
pub struct GenerateError(pub String);

impl fmt::Display for GenerateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GenerateError {}

type Result<T> = std::result::Result<T, GenerateError>;

fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(GenerateError(msg.into()))
}

fn obj(v: &Value) -> &Map<String, Value> {
    static EMPTY: LazyLock<Map<String, Value>> = LazyLock::new(Map::new);
    v.as_object().unwrap_or(&EMPTY)
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn resolve_ref<'a>(spec: &'a Value, reference: &str) -> Result<(String, &'a Value)> {
    let Some(rest) = reference.strip_prefix("#/") else {
        return err(format!("external $ref not supported: {reference}"));
    };
    let mut node = spec;
    for part in rest.split('/') {
        node = match node.get(part.replace("~1", "/").replace("~0", "~")) {
            Some(n) => n,
            None => return err(format!("unresolvable $ref: {reference}")),
        };
    }
    Ok((reference.rsplit('/').next().unwrap_or("").to_string(), node))
}

/// Follow `$ref`s until a node without one.
fn deref<'a>(spec: &'a Value, mut node: &'a Value) -> Result<&'a Value> {
    for _ in 0..16 {
        match node.get("$ref").and_then(Value::as_str) {
            Some(r) => node = resolve_ref(spec, r)?.1,
            None => return Ok(node),
        }
    }
    err("a $ref chain longer than 16")
}

fn field_description(text: &str) -> String {
    end_sentence(&fit(
        &tidy_sentences(&plain(text), ""),
        MAX_FIELD_DESCRIPTION,
    ))
}

/// When a `oneOf`/`anyOf` has no description of its own, the sentences its described branches start with move up to
/// it: all of the only described branch's (`milestone`: a number, or a title), or those all of them share.
fn hoist_description(out: &mut Map<String, Value>, key: &str) {
    if out.contains_key("description") {
        return;
    }
    let Some(Value::Array(branches)) = out.get_mut(key) else {
        return;
    };
    let described: Vec<Vec<String>> = branches
        .iter()
        .filter_map(|b| b.get("description").and_then(Value::as_str))
        .map(|d| sentences(d).into_iter().map(String::from).collect())
        .collect();
    let Some(first) = described.first() else {
        return;
    };
    let shared = (0..first.len())
        .take_while(|&i| described.iter().all(|d| d.get(i) == first.get(i)))
        .count();
    if shared == 0 {
        return;
    }
    let hoisted = first[..shared].join(" ");
    for b in branches.iter_mut() {
        let Some(branch) = b.as_object_mut() else {
            continue;
        };
        let Some(d) = branch.get("description").and_then(Value::as_str) else {
            continue;
        };
        let rest = sentences(d)[shared..].join(" ");
        if rest.is_empty() {
            branch.remove("description");
        } else {
            branch.insert("description".into(), json!(rest));
        }
    }
    out.insert("description".into(), json!(hoisted));
}

/// A JSON Schema for a tool's input from an OpenAPI schema: `$ref` and `allOf` inlined down to SCHEMA_DEPTH levels,
/// without cycles; `oneOf`/`anyOf` kept; read-only fields, examples and titles dropped.
pub fn convert(
    spec: &Value,
    schema: &Value,
    depth: usize,
    seen: &[String],
) -> Result<Map<String, Value>> {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let (name, target) = resolve_ref(spec, reference)?;
        if seen.contains(&name) || depth >= SCHEMA_DEPTH {
            let mut out = Map::new();
            let t = target
                .get("type")
                .cloned()
                .unwrap_or_else(|| json!("object"));
            out.insert("type".into(), t);
            let d = [
                field_description(str_of(target, "description")),
                one_line(&plain(str_of(target, "title")), MAX_FIELD_DESCRIPTION),
                name.clone(),
            ]
            .into_iter()
            .find(|d| !d.is_empty())
            .unwrap_or_default();
            out.insert("description".into(), json!(d));
            // A map cut off here still takes keys of its own.
            if truthy(target.get("additionalProperties")) {
                out.insert("additionalProperties".into(), json!(true));
            }
            return Ok(out);
        }
        let mut seen = seen.to_vec();
        seen.push(name);
        return convert(spec, target, depth, &seen);
    }
    let mut out = Map::new();
    let mut props = Map::new();
    let mut required: Vec<String> = Vec::new();
    let mut merged_type = None;
    for part in schema
        .get("allOf")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let m = convert(spec, part, depth, seen)?;
        if let Some(Value::Object(p)) = m.get("properties") {
            props.extend(p.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        required.extend(strings(m.get("required")));
        if merged_type.is_none() {
            merged_type = m.get("type").cloned();
        }
        if !out.contains_key("description") {
            if let Some(d) = m.get("description") {
                out.insert("description".into(), d.clone());
            }
        }
    }
    let stype = match schema.get("type") {
        Some(t) if truthy(Some(t)) => Some(t.clone()),
        _ if !props.is_empty() || schema.get("properties").is_some() => Some(json!("object")),
        _ => merged_type,
    };
    if let Some(t) = &stype {
        out.insert("type".into(), t.clone());
    }
    let d = field_description(str_of(schema, "description"));
    if !d.is_empty() {
        out.insert("description".into(), json!(d));
    }
    if let Some(f) = schema.get("format").and_then(Value::as_str) {
        if SCHEMA_FORMATS.contains(&f) {
            out.insert("format".into(), json!(f));
        }
    }
    for key in ["enum", "default"] {
        if let Some(v) = schema.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(variants) = schema.get(key).and_then(Value::as_array) {
            let mut converted = Vec::new();
            for v in variants {
                converted.push(Value::Object(convert(spec, v, depth + 1, seen)?));
            }
            out.insert(key.into(), Value::Array(converted));
            hoist_description(&mut out, key);
        }
    }
    for (name, prop) in obj(schema.get("properties").unwrap_or(&Value::Null)) {
        let resolved = deref(spec, prop)?;
        if truthy(prop.get("readOnly")) || truthy(resolved.get("readOnly")) {
            continue;
        }
        props.insert(
            name.clone(),
            Value::Object(convert(spec, prop, depth + 1, seen)?),
        );
    }
    if !props.is_empty() {
        out.insert("properties".into(), Value::Object(props.clone()));
    }
    required.extend(strings(schema.get("required")));
    let required: Vec<String> = dedup(required)
        .into_iter()
        .filter(|r| props.is_empty() || props.contains_key(r))
        .collect();
    if !required.is_empty() {
        out.insert("required".into(), json!(required));
    }
    if stype.as_ref().and_then(Value::as_str) == Some("array") {
        if let Some(items) = schema.get("items") {
            out.insert(
                "items".into(),
                Value::Object(convert(spec, items, depth + 1, seen)?),
            );
        }
    }
    match schema.get("additionalProperties") {
        Some(extra @ Value::Object(_)) => {
            out.insert(
                "additionalProperties".into(),
                Value::Object(convert(spec, extra, depth + 1, seen)?),
            );
        }
        // `true` marks a free-form object (client_payload, workflow inputs), so it does not look empty.
        Some(open @ Value::Bool(_)) => {
            out.insert("additionalProperties".into(), open.clone());
        }
        _ => {}
    }
    Ok(out)
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn dedup(items: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|i| seen.insert(i.clone()))
        .collect()
}

/// One operation of the description.
pub struct Operation<'a> {
    pub path: &'a str,
    /// Upper case.
    pub method: String,
    pub op: &'a Value,
    pub op_id: &'a str,
    pub category: &'a str,
    pub subcategory: &'a str,
}

pub fn operations(spec: &Value) -> Vec<Operation<'_>> {
    let mut out = Vec::new();
    for (path, item) in obj(spec.get("paths").unwrap_or(&Value::Null)) {
        for (method, op) in obj(item) {
            if !HTTP_METHODS.contains(&method.as_str()) {
                continue;
            }
            let x = op.get("x-github").unwrap_or(&Value::Null);
            out.push(Operation {
                path,
                method: method.to_uppercase(),
                op,
                op_id: str_of(op, "operationId"),
                category: str_of(x, "category"),
                subcategory: str_of(x, "subcategory"),
            });
        }
    }
    out
}

fn is_deprecated(op: &Value) -> bool {
    truthy(op.get("deprecated"))
        || truthy(op.get("x-github").and_then(|x| x.get("deprecationDate")))
}

fn request_body<'a>(spec: &'a Value, op: &'a Value) -> Result<Option<&'a Value>> {
    match op.get("requestBody") {
        Some(b) => Ok(Some(deref(spec, b)?)),
        None => Ok(None),
    }
}

/// `None` to include the operation, or the reason it is left out.
pub fn select(o: &Operation, conf: &Config) -> Option<String> {
    if ["HEAD", "OPTIONS"].contains(&o.method.as_str()) {
        return Some(format!("{} never included", o.method));
    }
    if is_deprecated(o.op) {
        return Some(DEPRECATED.into());
    }
    if let Some(r) = conf
        .rules
        .iter()
        .find(|r| r.matches(&o.method, o.path, o.category))
    {
        return Some(r.reason.clone());
    }
    if conf.read_only && o.method != "GET" {
        return Some(READ_ONLY.into());
    }
    None
}

/// One generated tool before naming.
#[derive(Clone, Debug)]
pub struct Entry {
    pub op_id: String,
    pub method: String,
    pub path: String,
    pub read: bool,
    pub name: String,
    pub tool: Map<String, Value>,
}

pub fn auth() -> Value {
    json!({
        "auth_type": "api_key",
        "var_name": "Authorization",
        "api_key": format!("Bearer ${{{PAT_VAR}}}"),
        "location": "header",
    })
}

fn success_codes(op: &Value) -> Vec<(&String, &Value)> {
    obj(op.get("responses").unwrap_or(&Value::Null))
        .iter()
        .filter(|(c, _)| c.starts_with('2') || c.starts_with('3'))
        .collect()
}

/// An answer that is only a redirect to a file (logs, archives, artifacts): utcp follows it and saves the file.
fn answers_file(spec: &Value, op: &Value) -> Result<bool> {
    let codes = success_codes(op);
    let mut redirect = false;
    for (code, r) in &codes {
        if truthy(deref(spec, r)?.get("content")) {
            return Ok(false);
        }
        redirect |= code.as_str() == "302";
    }
    Ok(redirect)
}

/// What an answer's description says about the outcome or the status code rather than the content ("Task created
/// successfully", "OK", "Response if merge was successful", "Response when getting a billing usage report"). utcp
/// prints the answer, not its status code, so it says nothing an agent can use.
static OUTCOME_ONLY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(?:(?:successful |status |empty )?response|ok|created|accepted)\.?$|\bsuccessfully\b|^(?:response )?(?:if|when|for)\b|\b(?:submitted|accepted) for (?:asynchronous )?processing\b",
    )
    .unwrap()
});

/// GitHub's description of a successful answer, for the tool's `outputs`: only what it says about the content, and
/// nothing when the description is an [`OUTCOME_ONLY`] or restates `context` (the tool's summary and route).
pub(crate) fn answer_description(text: &str, context: &str) -> String {
    let text = field_description(text);
    let text = text
        .strip_prefix("Successful Response (")
        .and_then(|t| t.strip_suffix(")."))
        .map(|t| format!("{t}."))
        .unwrap_or(text);
    let text = match text.strip_prefix("Response including ") {
        Some(rest) => format!("Includes {rest}"),
        None => text,
    };
    if OUTCOME_ONLY.is_match(&text) || restates(context, &text) {
        return String::new();
    }
    let mut chars = text.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => text,
    }
}

/// When a tool that answers with content answers null instead, after GitHub's description of that answer
/// ("Response if repository is empty" is "Null if repository is empty."), or [`NULL_TOO`] when it does not say.
pub(crate) fn null_when(description: &str) -> String {
    let text = field_description(description);
    let text = text.trim_end_matches('.');
    let text = text.strip_prefix("Response ").unwrap_or(text);
    match text.split_once(' ') {
        Some((word @ ("if" | "when"), rest)) if !rest.is_empty() => {
            // "Response if repository is empty"
            let rest = match rest.strip_prefix("repository ") {
                Some(r) => format!("the repository {r}"),
                None => rest.to_string(),
            };
            format!("Null {word} {rest}.")
        }
        _ => NULL_TOO.to_string(),
    }
}

/// The type of a tool's answer and, when GitHub says something about its content, a description. Several successful
/// answers that GitHub describes differently ("Reaction exists", "Reaction created") get none, because utcp does not
/// show which one came. When one successful answer has content and another has none (a 204 next to a 200), the
/// description says when the answer is null ([`null_when`]): utcp's `type` is one name, so it cannot say "object or
/// null".
pub(crate) fn outputs(spec: &Value, o: &Operation) -> Result<Value> {
    let op = o.op;
    if answers_file(spec, op)? {
        return Ok(json!({"type": "object", "description": FILE_ANSWER}));
    }
    let mut answers = Vec::new();
    let mut empty = None;
    // A redirect is followed by utcp, so it is never the answer.
    for (_, r) in success_codes(op)
        .into_iter()
        .filter(|(c, _)| c.starts_with('2'))
    {
        let r = deref(spec, r)?;
        match r.get("content").and_then(Value::as_object) {
            Some(content) => answers.push((content, str_of(r, "description"))),
            None => empty = Some(null_when(str_of(r, "description"))),
        }
    }
    let Some((content, description)) = answers.first() else {
        let description = if o.method == "GET" {
            YES_NO_ANSWER
        } else {
            NO_CONTENT
        };
        return Ok(json!({"type": "null", "description": description}));
    };
    let mut out = Map::new();
    match content.get("application/json") {
        Some(media) => {
            let schema = deref(spec, media.get("schema").unwrap_or(&Value::Null))?;
            let t = match schema.get("type").and_then(Value::as_str) {
                Some(t) => t,
                None if schema.get("items").is_some() => "array",
                None => "object",
            };
            out.insert("type".into(), json!(t));
        }
        None => {
            out.insert("type".into(), json!("string"));
        }
    }
    let alike = answers.iter().all(|(_, d)| d.trim() == description.trim());
    let mut description = if alike {
        answer_description(description, &format!("{} {}", summary(o), o.path))
    } else {
        String::new()
    };
    if let Some(null) = empty {
        description = format!("{description} {null}").trim().to_string();
    }
    if !description.is_empty() {
        out.insert("description".into(), json!(description));
    }
    Ok(Value::Object(out))
}

fn upload_schema() -> Value {
    json!({
        "type": "object",
        "description": "The file to upload, as {\"file\": \"<path>\"}. utcp reads and sends it.",
        "properties": {"file": {"type": "string", "description": "Path of the file to upload."}},
        "required": ["file"],
    })
}

/// GitHub's summary of an operation as one line of plain text, without a final period.
fn summary(o: &Operation) -> String {
    one_line(&plain(str_of(o.op, "summary")), MAX_DESCRIPTION)
        .trim_end_matches('.')
        .to_string()
}

/// The caution for a DELETE whose summary deletes, removes or disables something. One that undoes the user's own
/// action ("Unstar a repository", "Unlock an issue", "Mark a thread as done") gets none.
pub(crate) fn delete_note(summary: &str) -> Option<String> {
    match summary.split_whitespace().next()? {
        "Delete" => Some(DELETE_NOTE.to_string()),
        "Remove" => Some("Remove only what the user asked for or what you added.".to_string()),
        "Disable" => Some("Disable only what the user asked for or what you enabled.".to_string()),
        _ => None,
    }
}

/// The summary (with `qualifier` when other tools have the same summary), then GitHub's description in whole
/// sentences with config `sentence_fixes` applied, then config `notes`, all within MAX_DESCRIPTION characters. A
/// `yes_no` check (a GET that answers with no content) ends with [`YES_NO`] unless its description already says when
/// it fails with 404.
fn tool_description(
    o: &Operation,
    untested: bool,
    yes_no: bool,
    qualifier: Option<&str>,
    conf: &Config,
) -> Result<String> {
    let summary = summary(o);
    let mut detail = str_of(o.op, "description");
    for end in DESCRIPTION_ENDS {
        if let Some(i) = detail.find(end) {
            detail = &detail[..i];
        }
    }
    let mut detail = normalize(&plain(detail));
    for (id, from, to) in &conf.sentence_fixes {
        if id != o.op_id {
            continue;
        }
        if !detail.contains(from.as_str()) {
            return err(format!(
                "{id}: config sentence_fixes text \"{from}\" is not in GitHub's description, which now reads:\n{detail}"
            ));
        }
        detail = detail.replace(from.as_str(), to);
    }
    let mut text = match qualifier {
        Some(q) => format!("{summary} {q}"),
        None => summary.clone(),
    };
    if !text.is_empty() && !text.ends_with(['?', '!']) {
        text.push('.');
    }
    // After the summary, so the first sentence (all that `utcp search` shows) still says what is deleted.
    if o.method == "DELETE" {
        if let Some(note) = delete_note(&summary) {
            text = format!("{text} {note}");
        }
    }
    if untested {
        text = format!("{UNTESTED} {text}");
    }
    let budget = MAX_DESCRIPTION.saturating_sub(text.chars().count() + 1);
    // "Lists webhooks for a repository." after "List repository webhooks" says only what the route does.
    let route = o
        .path
        .replace("/repos/", "/repository/")
        .replace("/orgs/", "/organization/");
    let mut sentences = tidy_sentences(&detail, &format!("{summary} {route}"));
    if let Some(note) = conf.notes.get(o.op_id) {
        sentences.push(Sentence {
            text: note.clone(),
            essential: true,
        });
    }
    if o.path.starts_with("/search/") {
        sentences.push(Sentence {
            text: SEARCH_NOTE.to_string(),
            essential: true,
        });
    }
    if yes_no && !sentences.iter().any(|s| s.text.contains("404")) {
        sentences.push(Sentence {
            text: YES_NO.to_string(),
            essential: true,
        });
    }
    let detail = end_sentence(&fit(&sentences, budget));
    Ok(if detail.is_empty() {
        text
    } else {
        format!("{text} {detail}")
    })
}

/// One tool. `qualifier` ("for Dependabot") follows the summary when other tools have the same one.
pub fn build_tool(
    spec: &Value,
    o: &Operation,
    api_version: &str,
    conf: &Config,
    qualifier: Option<&str>,
) -> Result<Entry> {
    let op_id = o.op_id;
    if op_id.is_empty() {
        return err(format!("{} {}: no operationId", o.method, o.path));
    }
    let mut props = Map::new();
    let mut required: Vec<String> = Vec::new();
    let mut query: Vec<String> = Vec::new();
    for p in
        o.op.get("parameters")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
    {
        let p = deref(spec, p)?;
        let name = str_of(p, "name");
        let place = str_of(p, "in");
        let mut schema = convert(spec, p.get("schema").unwrap_or(&json!({})), 0, &[])?;
        let mut description = field_description(str_of(p, "description"));
        if place == "path" && description.trim_end_matches('.') == format!("{name} parameter") {
            description = match conf.param_descriptions.get(name) {
                Some(d) => d.clone(),
                None => {
                    return err(format!(
                        "{op_id}: path parameter '{name}' has only the placeholder description \"{description}\"; \
                         add one to config param_descriptions"
                    ))
                }
            };
        }
        let optional = conf
            .optional_path
            .iter()
            .find(|(id, param, _)| place == "path" && id == op_id && param == name);
        if place == "query" && schema.get("type") == Some(&json!("array")) {
            // GitHub reads a list as repeated parameters, utcp sends it comma-separated; one value works either way,
            // so GitHub's "Accepts one or more ..." goes.
            let items = schema
                .get("items")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            schema = items;
            let kept: Vec<&str> = sentences(&description)
                .into_iter()
                .filter(|s| !s.contains("one or more"))
                .collect();
            description = format!("{} Takes one value.", kept.join(" "))
                .trim()
                .to_string();
        }
        if !description.is_empty() {
            schema.insert("description".into(), json!(description));
        }
        match place {
            "path" if optional.is_some() => {}
            "path" => required.push(name.to_string()),
            "query" => {
                query.push(name.to_string());
                if truthy(p.get("required")) {
                    required.push(name.to_string());
                }
            }
            other => return err(format!("{op_id}: unsupported parameter location '{other}'")),
        }
        if props
            .insert(name.to_string(), Value::Object(schema))
            .is_some()
        {
            return err(format!("{op_id}: parameter '{name}' appears twice"));
        }
    }
    let mut template = Map::new();
    template.insert("call_template_type".into(), json!("http"));
    template.insert("http_method".into(), json!(o.method));
    let server =
        o.op.get("servers")
            .or_else(|| spec.get("servers"))
            .and_then(|s| s.get(0))
            .map(|s| str_of(s, "url"))
            .unwrap_or("");
    if !server.starts_with("https://") {
        return err(format!("{op_id}: no https server"));
    }
    if o.path.contains("${") {
        return err(format!(
            "{op_id}: '${{' in the path would be read as a utcp variable"
        ));
    }
    template.insert(
        "url".into(),
        json!(format!("{}{}", server.trim_end_matches('/'), o.path)),
    );
    template.insert(
        "headers".into(),
        json!({"Accept": ACCEPT, "X-GitHub-Api-Version": api_version}),
    );
    if let Some(body) = request_body(spec, o.op)? {
        let content = obj(body.get("content").unwrap_or(&Value::Null));
        let body_required = truthy(body.get("required"));
        let (media, media_obj) = match content.get("application/json") {
            Some(m) => ("application/json", m),
            None => match content.iter().next() {
                Some((t, m)) => (t.as_str(), m),
                None => return err(format!("{op_id}: request body without content")),
            },
        };
        let schema = convert(spec, media_obj.get("schema").unwrap_or(&json!({})), 0, &[])?;
        let mut as_field =
            |schema: Map<String, Value>, props: &mut Map<String, Value>| -> Result<()> {
                if props.contains_key("body") {
                    return err(format!(
                        "{op_id}: a parameter named 'body' clashes with the body argument"
                    ));
                }
                let mut schema = schema;
                if !schema.contains_key("description") {
                    let d = field_description(str_of(body, "description"));
                    let d = if d.is_empty() {
                        "The request body.".into()
                    } else {
                        d
                    };
                    schema.insert("description".into(), json!(d));
                }
                props.insert("body".into(), Value::Object(schema));
                if body_required {
                    required.push("body".into());
                }
                template.insert("body_field".into(), json!("body"));
                Ok(())
            };
        match (o.method.as_str(), media) {
            ("GET", _) => {
                // repos/get-content declares an empty optional body; a GET sends none.
                if schema.contains_key("properties") || body_required {
                    return err(format!("{op_id}: a GET with a request body"));
                }
            }
            (_, "application/octet-stream") => {
                let Value::Object(s) = upload_schema() else {
                    unreachable!()
                };
                as_field(s, &mut props)?;
                template.insert("body_type".into(), json!("file"));
            }
            (_, "text/plain") => {
                if schema.get("type") != Some(&json!("string")) {
                    return err(format!("{op_id}: a text body that is not a string"));
                }
                as_field(schema, &mut props)?;
                template.insert("body_type".into(), json!("text"));
                template["headers"]["Content-Type"] = json!("text/plain");
            }
            ("DELETE", "application/json") => {
                // utcp sends a DELETE's other arguments as query parameters, so the body is always one argument.
                as_field(schema, &mut props)?;
            }
            (_, "application/json") => {
                let fields = schema.get("properties").and_then(Value::as_object);
                let clash = fields.is_some_and(|f| f.keys().any(|k| props.contains_key(k)));
                let is_object = schema.get("type") == Some(&json!("object"));
                match fields {
                    Some(fields) if is_object && query.is_empty() && !clash => {
                        for (k, v) in fields {
                            props.insert(k.clone(), v.clone());
                        }
                        if body_required {
                            required.extend(strings(schema.get("required")));
                        }
                    }
                    _ => as_field(schema, &mut props)?,
                }
            }
            (_, other) => return err(format!("{op_id}: unsupported request body type {other}")),
        }
    }
    if answers_file(spec, o.op)? {
        template.insert("response_type".into(), json!("file"));
    }
    template.insert("auth".into(), auth());
    describe_inputs(op_id, &mut props, conf)?;
    for (_, name, note) in conf.optional_path.iter().filter(|(id, ..)| id == op_id) {
        let Some(schema) = props.get_mut(name).and_then(Value::as_object_mut) else {
            return err(format!(
                "config optional_path names {op_id} argument '{name}', which the tool does not have"
            ));
        };
        let d = schema
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let d = format!("{d} {note}");
        schema.insert("description".into(), json!(d.trim()));
    }
    let mut inputs = Map::new();
    inputs.insert("type".into(), json!("object"));
    inputs.insert("properties".into(), Value::Object(props));
    let required = dedup(required);
    if !required.is_empty() {
        inputs.insert("required".into(), json!(required));
    }
    let read = o.method == "GET";
    let untested = !read && !conf.tested_writes.iter().any(|w| w == op_id);
    let mut tags = vec![json!("github")];
    for t in [o.category, o.subcategory] {
        if !t.is_empty() && !tags.contains(&json!(t)) {
            tags.push(json!(t));
        }
    }
    if untested {
        tags.push(json!("untested"));
    }
    let outputs = outputs(spec, o)?;
    let yes_no = read && outputs.get("type") == Some(&json!("null"));
    let mut tool = Map::new();
    tool.insert("name".into(), Value::Null);
    tool.insert(
        "description".into(),
        json!(tool_description(o, untested, yes_no, qualifier, conf)?),
    );
    tool.insert("tags".into(), Value::Array(tags));
    tool.insert("inputs".into(), Value::Object(inputs));
    tool.insert("outputs".into(), outputs);
    tool.insert("tool_call_template".into(), Value::Object(template));
    Ok(Entry {
        op_id: op_id.to_string(),
        method: o.method.clone(),
        path: o.path.to_string(),
        read,
        name: String::new(),
        tool,
    })
}

/// Every argument has a description: GitHub's, or for the ones GitHub leaves undescribed, the one in config
/// `input_descriptions`. A config entry for an argument GitHub now describes, or that the tool does not have, is an
/// error too, so the config cannot go stale.
fn describe_inputs(op_id: &str, props: &mut Map<String, Value>, conf: &Config) -> Result<()> {
    for (name, schema) in props.iter_mut() {
        let configured = conf
            .input_descriptions
            .get(&(op_id.to_string(), name.clone()));
        let Some(schema) = schema.as_object_mut() else {
            continue;
        };
        match (schema.contains_key("description"), configured) {
            (false, Some(d)) => {
                schema.insert("description".into(), json!(d));
            }
            (false, None) => {
                return err(format!(
                    "{op_id}: argument '{name}' has no description; add one to config input_descriptions"
                ))
            }
            (true, Some(_)) => {
                return err(format!(
                    "{op_id}: GitHub now describes argument '{name}'; remove it from config input_descriptions"
                ))
            }
            (true, None) => {}
        }
    }
    for (id, name) in conf.input_descriptions.keys() {
        if id == op_id && !props.contains_key(name) {
            return err(format!(
                "config input_descriptions names {op_id} argument '{name}', which the tool does not have"
            ));
        }
    }
    Ok(())
}

/// Every operationId the configuration names must exist, so a typo cannot silently keep an old name.
fn check_config_names(spec: &Value, conf: &Config) -> Result<()> {
    let ids: HashSet<&str> = operations(spec).iter().map(|o| o.op_id).collect();
    let mut named: Vec<&String> = conf
        .names
        .keys()
        .chain(conf.tested_writes.iter())
        .chain(conf.optional_path.iter().map(|(id, ..)| id))
        .chain(conf.input_descriptions.keys().map(|(id, _)| id))
        .chain(conf.sentence_fixes.iter().map(|(id, ..)| id))
        .chain(conf.notes.keys())
        .chain(conf.variants.iter().map(|v| &v.op_id))
        .collect();
    named.sort();
    for op_id in named {
        if !ids.contains(op_id.as_str()) {
            return err(format!(
                "config names {op_id}, which the description does not have"
            ));
        }
    }
    Ok(())
}

/// For each operation, the qualifier from config `summary_qualifiers` for its category when another kept operation
/// has the same summary ("Get a repository secret" for Actions, Dependabot and the Copilot coding agent), so every
/// tool's first sentence, which is all `utcp search` shows, is different.
fn qualifiers<'a>(ops: &[Operation], conf: &'a Config) -> Result<Vec<Option<&'a str>>> {
    let key = |o: &Operation| summary(o).to_lowercase();
    let mut count: BTreeMap<String, usize> = BTreeMap::new();
    for o in ops {
        *count.entry(key(o)).or_default() += 1;
    }
    let mut out = Vec::new();
    let mut qualified: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for o in ops {
        let k = key(o);
        if count[&k] < 2 {
            qualified.entry(k).or_default().push(o.op_id);
            out.push(None);
            continue;
        }
        let Some(q) = conf.summary_qualifiers.get(o.category) else {
            return err(format!(
                "{}: other tools have the same summary \"{}\"; add category '{}' to config summary_qualifiers",
                o.op_id,
                summary(o),
                o.category
            ));
        };
        qualified
            .entry(format!("{k} {q}"))
            .or_default()
            .push(o.op_id);
        out.push(Some(q.as_str()));
    }
    for (summary, ids) in qualified {
        if ids.len() > 1 {
            return err(format!(
                "{} still have the same summary \"{summary}\"; change config summary_qualifiers",
                ids.join(", ")
            ));
        }
    }
    Ok(out)
}

/// The `manual.tool` name of every operation `select` keeps.
fn tool_names(ops: &[Operation], conf: &Config) -> Result<HashMap<String, String>> {
    let mut per_manual: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for o in ops {
        if o.op_id.is_empty() || select(o, conf).is_some() {
            continue;
        }
        // An operation without a manual fails `generate` with its own message.
        if let Some(manual) = conf.manual_of(o.category, o.subcategory) {
            per_manual.entry(manual).or_default().push(o.op_id);
        }
    }
    let mut out = HashMap::new();
    for (manual, mut ids) in per_manual {
        ids.sort();
        let names = names::assign(manual, &ids, conf).map_err(GenerateError)?;
        for (id, name) in ids.iter().zip(names) {
            out.insert(id.to_string(), format!("{manual}.{name}"));
        }
    }
    Ok(out)
}

/// A tool that calls the same operation as `base` with config variant `v`'s media type, name and description.
fn variant(base: &Entry, v: &Variant) -> Entry {
    let mut tool = base.tool.clone();
    tool.insert("name".into(), json!(v.name));
    tool.insert("description".into(), json!(v.description));
    if let Some(t) = tool.get_mut("tool_call_template") {
        t["headers"]["Accept"] = json!(v.accept);
        if v.file {
            t["response_type"] = json!("file");
        }
    }
    let output = if v.file {
        json!({"type": "object", "description": FILE_ANSWER})
    } else {
        json!({"description": v.output})
    };
    tool.insert("outputs".into(), output);
    Entry {
        name: v.name.clone(),
        tool,
        ..base.clone()
    }
}

/// Every `description` string in `map` and below it with `pattern` replaced by `with`.
fn replace_in_descriptions(map: &mut Map<String, Value>, pattern: &Regex, with: &str) {
    for (k, v) in map.iter_mut() {
        match v {
            Value::String(s) if k == "description" => {
                *s = pattern.replace_all(s, with).into_owned();
            }
            Value::Object(m) => replace_in_descriptions(m, pattern, with),
            Value::Array(a) => {
                for x in a.iter_mut() {
                    if let Value::Object(m) = x {
                        replace_in_descriptions(m, pattern, with);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Every tool a description names (`github_repos.get_commit`) must exist, so a renamed tool cannot leave a pointer
/// to nothing.
fn check_named_tools(manuals: &IndexMap<String, Value>) -> Result<()> {
    let mut all: HashSet<String> = HashSet::new();
    for (manual, m) in manuals {
        for t in m["tools"].as_array().into_iter().flatten() {
            all.insert(format!("{manual}.{}", str_of(t, "name")));
        }
    }
    fn walk<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
        match v {
            Value::Object(m) => {
                for (k, x) in m {
                    match x {
                        Value::String(s) if k == "description" => out.push(s),
                        _ => walk(x, out),
                    }
                }
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    for (manual, m) in manuals {
        for t in m["tools"].as_array().into_iter().flatten() {
            let mut texts = Vec::new();
            walk(t, &mut texts);
            for name in texts.into_iter().flat_map(named_tools) {
                if !all.contains(name) {
                    return err(format!(
                        "{manual}.{}: its description names {name}, which is not a tool",
                        str_of(t, "name")
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Returns ({manual name: manual}, report).
pub fn generate(
    spec: &Value,
    api_version: &str,
    source: &Value,
    conf: &Config,
) -> Result<(IndexMap<String, Value>, Value)> {
    check_config_names(spec, conf)?;
    for m in &conf.manuals {
        if m.hint.chars().count() > UTCP_MAX_HINT_CHARS {
            return err(format!(
                "{}: hint longer than {UTCP_MAX_HINT_CHARS} characters",
                m.name
            ));
        }
    }
    // Tool names first, so descriptions can point to tools instead of documentation pages.
    let ops = operations(spec);
    let tools = tool_names(&ops, conf)?;
    let linked = links::rewrite_spec(spec, &links::Targets::new(&ops, &tools));
    let spec = &linked;
    let mut per_manual: IndexMap<String, Vec<Entry>> = conf
        .manuals
        .iter()
        .map(|m| (m.name.clone(), Vec::new()))
        .collect();
    let mut left_out = Vec::new();
    let mut seen_ids = HashSet::new();
    let mut kept = Vec::new();
    for o in operations(spec) {
        if !o.op_id.is_empty() && !seen_ids.insert(o.op_id) {
            return err(format!("operationId {} is used twice", o.op_id));
        }
        if let Some(reason) = select(&o, conf) {
            left_out.push(json!({
                "operation_id": o.op_id, "method": o.method, "path": o.path,
                "category": o.category, "subcategory": o.subcategory, "reason": reason,
            }));
            continue;
        }
        kept.push(o);
    }
    let qualifiers = qualifiers(&kept, conf)?;
    for (o, qualifier) in kept.iter().zip(qualifiers) {
        let Some(manual) = conf.manual_of(o.category, o.subcategory) else {
            return err(format!(
                "{} ({} {}): category '{}' has no manual; add it to config categories, or leave it out with a rule",
                o.op_id, o.method, o.path, o.category
            ));
        };
        let Some(list) = per_manual.get_mut(manual) else {
            return err(format!(
                "config maps '{}' to unknown manual {manual}",
                o.category
            ));
        };
        list.push(build_tool(spec, o, api_version, conf, qualifier)?);
        for v in conf.variants.iter().filter(|v| v.op_id == o.op_id) {
            let base = list.last().expect("the base tool was just added");
            let entry = variant(base, v);
            list.push(entry);
        }
    }
    for v in &conf.variants {
        if !kept.iter().any(|o| o.op_id == v.op_id) {
            return err(format!(
                "config variant {} is of {}, which is left out",
                v.name, v.op_id
            ));
        }
    }
    let mut manuals = IndexMap::new();
    let mut report_manuals = Map::new();
    let mut tools_index = Vec::new();
    for mconf in &conf.manuals {
        let mut entries = per_manual.shift_remove(&mconf.name).unwrap_or_default();
        if entries.is_empty() {
            continue;
        }
        for e in entries.iter_mut().filter(|e| e.name.is_empty()) {
            let full = &tools[&e.op_id];
            let name = full
                .strip_prefix(&format!("{}.", mconf.name))
                .unwrap_or(full);
            e.tool.insert("name".into(), json!(name));
            e.name = name.to_string();
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        // "make multiple requests to github_actions.update_check_run" on that tool itself.
        for e in entries.iter_mut() {
            let own = Regex::new(&format!(
                r"\b{}\b",
                regex::escape(&format!("{}.{}", mconf.name, e.name))
            ))
            .expect("an escaped name is a valid regex");
            replace_in_descriptions(&mut e.tool, &own, "this tool");
        }
        if let Some(w) = entries.windows(2).find(|w| w[0].name == w[1].name) {
            return err(format!(
                "{}: two tools are named {} ({} and {}); change config variants or names",
                mconf.name, w[0].name, w[0].op_id, w[1].op_id
            ));
        }
        let commit: String = str_of(source, "commit").chars().take(12).collect();
        let manual = json!({
            "manual_version": "1.0.0",
            "utcp_version": "1.0.0",
            "info": {
                "title": mconf.title,
                "version": api_version,
                "description": format!(
                    "{} (GitHub REST API {api_version}). Generated by github-manuals from GitHub's OpenAPI \
                     description (commit {commit}). Authenticates with the token in {PAT_VAR}.",
                    mconf.title
                ),
            },
            "tools": entries.iter().map(|e| Value::Object(e.tool.clone())).collect::<Vec<_>>(),
        });
        let bytes = serde_json::to_string(&manual)
            .expect("JSON values always serialize")
            .len();
        let reads = entries.iter().filter(|e| e.read).count();
        let untested = entries
            .iter()
            .filter(|e| {
                e.tool["tags"]
                    .as_array()
                    .is_some_and(|t| t.contains(&json!("untested")))
            })
            .count();
        report_manuals.insert(
            mconf.name.clone(),
            json!({
                "title": mconf.title,
                "hint": mconf.hint,
                "tools": entries.len(),
                "reads": reads,
                "writes": entries.len() - reads,
                "untested_writes": untested,
                "bytes": bytes,
                "approx_tokens": bytes / 4,
            }),
        );
        for e in &entries {
            tools_index.push(json!({
                "manual": mconf.name, "name": e.name, "operation_id": e.op_id, "method": e.method, "path": e.path,
                "accept": e.tool["tool_call_template"]["headers"]["Accept"],
            }));
        }
        manuals.insert(mconf.name.clone(), manual);
    }
    check_named_tools(&manuals)?;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for x in &left_out {
        *counts.entry(str_of(x, "reason").to_string()).or_default() += 1;
    }
    let mut summary: Vec<(String, usize)> = counts.into_iter().collect();
    summary.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let report = json!({
        "api_version": api_version,
        "source": source,
        "manuals": report_manuals,
        "left_out_summary": summary.iter().map(|(r, n)| json!({"reason": r, "operations": n})).collect::<Vec<_>>(),
        "left_out": left_out,
        "tools": tools_index,
    });
    Ok((manuals, report))
}
