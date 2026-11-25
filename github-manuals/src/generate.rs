//! Turn GitHub's OpenAPI 3 description into utcp HTTP manuals plus a report. Nothing here touches the network.
//!
//! Each operation is either left out (a rule of `Config`, with its reason) or becomes one tool in the manual of its
//! `x-github` category. Request bodies are flat: the fields of a JSON object body are the tool's arguments next to
//! the path parameters, as in GitHub's documentation (`create_issue` takes `owner`, `repo`, `title`, `body`). A body
//! that is not an object, or that comes with query parameters, is passed whole as the argument `body` instead.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::sync::LazyLock;

use indexmap::IndexMap;
use serde_json::{json, Map, Value};

use crate::config::{Config, PAT_VAR};
use crate::names;
use crate::text::{one_line, plain, tidy, truthy};

/// Starts the description of a write that has not yet worked in a live test against a throwaway repository.
pub const UNTESTED: &str = "(untested)";
/// Starts the description of every DELETE.
pub const DELETE_NOTE: &str = "Delete only what the user asked for or what you created.";
/// The output of a tool that answers with a file.
pub const FILE_ANSWER: &str = "A file. utcp saves it (to the Downloads folder, or the path given with --out) and \
                               prints {\"type\": \"file\", \"path\", \"mime_type\", \"bytes\", \"filename\"}.";
pub const ACCEPT: &str = "application/vnd.github+json";
const DEPRECATED: &str = "deprecated by GitHub";
const READ_ONLY: &str = "write (--read-only)";
/// utcp refuses a manual hint longer than this (`MAX_MANUAL_HINT_CHARS` in utcp-core).
pub const UTCP_MAX_HINT_CHARS: usize = 60;
const SCHEMA_DEPTH: usize = 3;
const MAX_DESCRIPTION: usize = 800;
const MAX_FIELD_DESCRIPTION: usize = 300;
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
    one_line(&tidy(&plain(text), ""), MAX_FIELD_DESCRIPTION)
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
                field_description(str_of(target, "title")),
                name.clone(),
            ]
            .into_iter()
            .find(|d| !d.is_empty())
            .unwrap_or_default();
            out.insert("description".into(), json!(d));
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
    if let Some(extra @ Value::Object(_)) = schema.get("additionalProperties") {
        out.insert(
            "additionalProperties".into(),
            Value::Object(convert(spec, extra, depth + 1, seen)?),
        );
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

fn outputs(spec: &Value, op: &Value) -> Result<Value> {
    if answers_file(spec, op)? {
        return Ok(json!({"type": "object", "description": FILE_ANSWER}));
    }
    for (_, r) in success_codes(op) {
        let r = deref(spec, r)?;
        let Some(content) = r.get("content").and_then(Value::as_object) else {
            continue;
        };
        // "Response" says nothing.
        let description = match field_description(str_of(r, "description")) {
            d if d == "Response" => String::new(),
            d => d,
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
        if !description.is_empty() {
            out.insert("description".into(), json!(description));
        }
        return Ok(Value::Object(out));
    }
    Ok(json!({"type": "null", "description": "No content: the call answers null when it worked."}))
}

fn upload_schema() -> Value {
    json!({
        "type": "object",
        "description": "The file to upload, as {\"file\": \"<path>\"}. utcp reads and sends it.",
        "properties": {"file": {"type": "string", "description": "Path of the file to upload."}},
        "required": ["file"],
    })
}

fn tool_description(o: &Operation, untested: bool) -> String {
    let summary = one_line(&plain(str_of(o.op, "summary")), MAX_DESCRIPTION);
    let mut detail = str_of(o.op, "description");
    for end in DESCRIPTION_ENDS {
        if let Some(i) = detail.find(end) {
            detail = &detail[..i];
        }
    }
    let detail = one_line(&tidy(&plain(detail), &summary), MAX_DESCRIPTION);
    let mut text = summary.trim_end_matches('.').to_string();
    if !text.is_empty() && !text.ends_with(['?', '!']) {
        text.push('.');
    }
    // After the summary, so the first sentence (all that `utcp search` shows) still says what is deleted.
    if o.method == "DELETE" {
        text = format!("{text} {DELETE_NOTE}");
    }
    if !detail.is_empty() {
        text = format!("{text} {detail}");
    }
    if untested {
        text = format!("{UNTESTED} {text}");
    }
    one_line(&text, MAX_DESCRIPTION + 80)
}

pub fn build_tool(spec: &Value, o: &Operation, api_version: &str, conf: &Config) -> Result<Entry> {
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
        if place == "path" && description == format!("{name} parameter") {
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
        let optional = place == "path"
            && conf
                .optional_path
                .iter()
                .any(|(id, param)| id == op_id && param == name);
        if optional {
            description = format!("{description} Leave it out for the repository root.");
        }
        if place == "query" && schema.get("type") == Some(&json!("array")) {
            // utcp sends an array as one JSON string; GitHub reads repeated parameters. One value always works.
            let items = schema
                .get("items")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            schema = items;
            description = format!("{description} (one value)").trim().to_string();
        }
        if !description.is_empty() {
            schema.insert("description".into(), json!(description));
        }
        match place {
            "path" if optional => {}
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
    let mut tool = Map::new();
    tool.insert("name".into(), Value::Null);
    tool.insert("description".into(), json!(tool_description(o, untested)));
    tool.insert("tags".into(), Value::Array(tags));
    tool.insert("inputs".into(), Value::Object(inputs));
    tool.insert("outputs".into(), outputs(spec, o.op)?);
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

/// Every operationId the configuration names must exist, so a typo cannot silently keep an old name.
fn check_config_names(spec: &Value, conf: &Config) -> Result<()> {
    let ids: HashSet<&str> = operations(spec).iter().map(|o| o.op_id).collect();
    let mut named: Vec<&String> = conf
        .names
        .keys()
        .chain(conf.tested_writes.iter())
        .chain(conf.optional_path.iter().map(|(id, _)| id))
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
    let mut per_manual: IndexMap<String, Vec<Entry>> = conf
        .manuals
        .iter()
        .map(|m| (m.name.clone(), Vec::new()))
        .collect();
    let mut left_out = Vec::new();
    let mut seen_ids = HashSet::new();
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
        list.push(build_tool(spec, &o, api_version, conf)?);
    }
    let mut manuals = IndexMap::new();
    let mut report_manuals = Map::new();
    let mut tools_index = Vec::new();
    for mconf in &conf.manuals {
        let mut entries = per_manual.shift_remove(&mconf.name).unwrap_or_default();
        if entries.is_empty() {
            continue;
        }
        entries.sort_by(|a, b| a.op_id.cmp(&b.op_id));
        let ids: Vec<&str> = entries.iter().map(|e| e.op_id.as_str()).collect();
        let assigned = names::assign(&mconf.name, &ids, conf).map_err(GenerateError)?;
        for (e, n) in entries.iter_mut().zip(assigned) {
            e.tool.insert("name".into(), json!(n));
            e.name = n;
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
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
            }));
        }
        manuals.insert(mconf.name.clone(), manual);
    }
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
