//! Turn Azure DevOps Swagger 2.0 specs into utcp HTTP manuals.
//!
//! Input: the parsed spec files of each area (see `Config::manuals`). Output: one utcp manual per group, plus a
//! report (tools per manual, size, required PAT scopes, operations left out and why). Nothing here touches the
//! network.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;
use serde_json::{json, Map, Value};

use crate::config::Config;
use crate::text::{
    compact_ascii, first_sentence_len, name_sentence, one_line, one_line_default, shown, snake,
    sorted_map, truthy, MAX_DESCRIPTION,
};

pub const PAT_VAR: &str = "AZURE_DEVOPS_EXT_PAT";
/// Starts the description of a write that `write-test` does not cover.
pub const UNTESTED: &str = "(untested)";
const BINARY_TYPES: [&str; 3] = ["application/zip", "application/octet-stream", "image/"];
/// The output of a tool that answers with a file.
const FILE_ANSWER: &str =
    "A file. utcp saves it (to the Downloads folder, or the path given with --out) and prints \
                           {\"type\": \"file\", \"path\", \"mime_type\", \"bytes\", \"filename\"}.";
const BODY_DEPTH: usize = 3;
const DROP_BODY_PROPERTIES: [&str; 2] = ["_links", "url"];
const HTTP_METHODS: [&str; 7] = ["get", "put", "post", "delete", "options", "head", "patch"];
/// Formats JSON Schema knows; the specs also use free text such as "array (string)".
const SCHEMA_FORMATS: [&str; 9] = [
    "int32",
    "int64",
    "date-time",
    "uuid",
    "date",
    "double",
    "float",
    "byte",
    "binary",
];

static ORG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("^[A-Za-z0-9][A-Za-z0-9-]{0,48}[A-Za-z0-9]?$").unwrap());
static PREVIEW_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^-preview\.\d+$").unwrap());
/// A first sentence that is a developer's note, not a description: where the code came from ("This was copied and
/// adapted from TeamFoundationConnectionService.Connect()"), or an example request to a developer's server
/// ("http://(tfsserver):8080/tfs/DefaultCollection/_apis/...").
static DEVELOPER_COMMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:copied|adapted|ported)(?: and \w+)? from\b|^https?://").unwrap()
});

/// area -> parsed spec files, in configuration order.
pub type Specs = IndexMap<String, Vec<Value>>;

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

pub fn valid_org(org: &str) -> bool {
    ORG_RE.is_match(org)
}

fn obj(v: &Value) -> &Map<String, Value> {
    static EMPTY: LazyLock<Map<String, Value>> = LazyLock::new(Map::new);
    v.as_object().unwrap_or(&EMPTY)
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn array_of<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

struct Operation<'a> {
    path: &'a str,
    method: &'a str,
    op: &'a Value,
    shared: &'a [Value],
}

/// Operations of `paths` and `x-ms-paths`, with their path-level parameters.
fn iter_operations(spec: &Value) -> Vec<Operation<'_>> {
    let mut out = Vec::new();
    for key in ["paths", "x-ms-paths"] {
        for (path, item) in obj(spec.get(key).unwrap_or(&Value::Null)) {
            let shared = array_of(item, "parameters");
            for (method, op) in obj(item) {
                if HTTP_METHODS.contains(&method.as_str()) {
                    out.push(Operation {
                        path,
                        method,
                        op,
                        shared,
                    });
                }
            }
        }
    }
    out
}

fn resolve_ref<'a>(spec: &'a Value, reference: &str) -> Result<(String, &'a Value)> {
    let Some(rest) = reference.strip_prefix("#/") else {
        return err(format!("external $ref not supported: {reference}"));
    };
    let mut node = spec;
    for part in rest.split('/') {
        node = match node.get(part) {
            Some(n) => n,
            None => return err(format!("unresolvable $ref: {reference}")),
        };
    }
    let name = reference.rsplit('/').next().unwrap_or("").to_string();
    Ok((name, node))
}

/// The api-version an operation needs: a library operation's own (`x-azdo-manuals`), else from its
/// `#/parameters/api-Version[-preview.N]` reference.
pub fn api_version_of(op: &Value, base_version: &str) -> Result<String> {
    if let Some(v) = op
        .get("x-azdo-manuals")
        .and_then(|x| x.get("api-version"))
        .and_then(Value::as_str)
    {
        return Ok(v.to_string());
    }
    let op_id = shown(op.get("operationId"));
    for p in array_of(op, "parameters") {
        let reference = str_of(p, "$ref");
        let name = reference.rsplit('/').next().unwrap_or("");
        if let Some(suffix) = name.strip_prefix("api-Version") {
            if !suffix.is_empty() && !PREVIEW_RE.is_match(suffix) {
                return err(format!("{op_id}: unknown api-version parameter {name}"));
            }
            return Ok(format!("{base_version}{suffix}"));
        }
        if p.get("name").and_then(Value::as_str) == Some("api-version")
            && p.get("in").and_then(Value::as_str) == Some("query")
        {
            return err(format!(
                "{op_id}: inline api-version parameter, expected a $ref"
            ));
        }
    }
    err(format!("{op_id}: no api-version parameter"))
}

pub(crate) fn scopes_of(spec: &Value, op: &Value) -> Vec<String> {
    let security = op.get("security").or_else(|| spec.get("security"));
    let mut scopes = BTreeSet::new();
    for entry in security
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        for s in array_of(entry, "oauth2") {
            if let Some(s) = s.as_str() {
                scopes.insert(s.to_string());
            }
        }
    }
    scopes.into_iter().collect()
}

/// What leaving out an optional path parameter means, for the two whose absence changes the call's scope.
fn scope_note(name: &str) -> Option<&'static str> {
    match name {
        "project" => {
            Some("Optional: leave it out for the whole organization instead of one project.")
        }
        "team" => Some("Optional: leave it out for the project's default team."),
        _ => None,
    }
}

/// Whether leaving out path parameter `name` keeps the route it belongs to. It does when the parameter is the end of the
/// path, or a scope before the API root (`{project}/_apis/...`, `{project}/_packaging/...`). With a fixed segment after
/// it (`changesets/{id}/changes`), the server would read that segment as the parameter, so it stays required.
pub(crate) fn can_leave_out(path: &str, name: &str) -> bool {
    let segments: Vec<&str> = path.split('/').collect();
    let Some(at) = segments.iter().position(|s| {
        s.strip_prefix('{')
            .and_then(|s| s.strip_suffix('}'))
            .is_some_and(|s| s.eq_ignore_ascii_case(name))
    }) else {
        return true;
    };
    let after = &segments[at + 1..];
    after.iter().any(|s| s.starts_with('_')) || after.iter().all(|s| s.starts_with('{'))
}

fn param_schema(p: &Value) -> Value {
    let mut schema = Map::new();
    let mut ptype = p.get("type").cloned().unwrap_or_else(|| json!("string"));
    let fmt = str_of(p, "format");
    let mut desc = one_line_default(str_of(p, "description"));
    if fmt.starts_with("array (") {
        // Azure DevOps takes these as one comma-separated string.
        ptype = json!("string");
        if !desc.to_lowercase().contains("comma") {
            desc = if desc.is_empty() {
                "(comma-separated)".into()
            } else {
                desc + " (comma-separated)"
            };
        }
    } else if SCHEMA_FORMATS.contains(&fmt) {
        schema.insert("format".into(), json!(fmt));
    }
    let is_array = ptype.as_str() == Some("array");
    schema.insert("type".into(), ptype);
    if is_array {
        if let Some(items) = p.get("items") {
            let kept: Map<String, Value> = obj(items)
                .iter()
                .filter(|(k, _)| *k == "type" || *k == "enum")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            schema.insert("items".into(), Value::Object(kept));
        }
    }
    for key in ["enum", "default"] {
        if let Some(v) = p.get(key) {
            schema.insert(key.into(), v.clone());
        }
    }
    if !desc.is_empty() {
        schema.insert("description".into(), json!(desc));
    }
    Value::Object(sorted_map(schema))
}

/// Inline `$ref` and `allOf`, down to BODY_DEPTH levels, without cycles. Read-only fields are dropped.
fn body_schema(
    spec: &Value,
    schema: &Value,
    depth: usize,
    seen: &[String],
) -> Result<Map<String, Value>> {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let (name, target) = resolve_ref(spec, reference)?;
        if seen.contains(&name) || depth >= BODY_DEPTH {
            let description = match target.get("description").and_then(Value::as_str) {
                Some(d) if !d.is_empty() => d.to_string(),
                _ => name,
            };
            let mut out = Map::new();
            out.insert("type".into(), json!("object"));
            out.insert("description".into(), json!(one_line_default(&description)));
            return Ok(out);
        }
        let mut seen = seen.to_vec();
        seen.push(name);
        return body_schema(spec, target, depth, &seen);
    }
    let mut out = Map::new();
    let mut props = Map::new();
    let mut required: Vec<String> = Vec::new();
    for part in array_of(schema, "allOf") {
        let merged = body_schema(spec, part, depth, seen)?;
        if let Some(Value::Object(p)) = merged.get("properties") {
            for (k, v) in p {
                props.insert(k.clone(), v.clone());
            }
        }
        required.extend(strings(merged.get("required")));
    }
    let stype = match schema.get("type") {
        Some(t) if truthy(Some(t)) => Some(t.clone()),
        _ if !props.is_empty() || schema.get("properties").is_some() => Some(json!("object")),
        _ => None,
    };
    if let Some(t) = &stype {
        out.insert("type".into(), t.clone());
    }
    if truthy(schema.get("description")) {
        out.insert(
            "description".into(),
            json!(one_line_default(str_of(schema, "description"))),
        );
    }
    if let Some(f) = schema.get("format") {
        if f.as_str().is_some_and(|f| SCHEMA_FORMATS.contains(&f)) {
            out.insert("format".into(), f.clone());
        }
    }
    if let Some(e) = schema.get("enum") {
        out.insert("enum".into(), e.clone());
    }
    for (name, prop) in obj(schema.get("properties").unwrap_or(&Value::Null)) {
        if DROP_BODY_PROPERTIES.contains(&name.as_str()) || truthy(prop.get("readOnly")) {
            continue;
        }
        props.insert(
            name.clone(),
            Value::Object(body_schema(spec, prop, depth + 1, seen)?),
        );
    }
    let has_props = !props.is_empty();
    let prop_names: HashSet<String> = props.keys().cloned().collect();
    if has_props {
        out.insert("properties".into(), Value::Object(sorted_map(props)));
    }
    required.extend(strings(schema.get("required")));
    if !required.is_empty() {
        let set: BTreeSet<String> = if has_props {
            required
                .into_iter()
                .filter(|r| prop_names.contains(r))
                .collect()
        } else {
            required.into_iter().collect()
        };
        out.insert(
            "required".into(),
            json!(set.into_iter().collect::<Vec<_>>()),
        );
    }
    if stype.as_ref().and_then(Value::as_str) == Some("array") {
        if let Some(items) = schema.get("items") {
            out.insert(
                "items".into(),
                Value::Object(body_schema(spec, items, depth + 1, seen)?),
            );
        }
    }
    if let Some(extra @ Value::Object(_)) = schema.get("additionalProperties") {
        out.insert(
            "additionalProperties".into(),
            Value::Object(body_schema(spec, extra, depth + 1, seen)?),
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

/// utcp fills `{name}` in a URL from the argument of exactly that name. A few specs spell a path parameter
/// differently in the path and in its parameter (`{DefinitionId}`, `definitionId`); the URL takes the input's
/// spelling. A placeholder with no input of any case stays as it is (utcp accepts it as an argument anyway).
pub(crate) fn placeholders_named_as_inputs(url: &str, props: &Map<String, Value>) -> String {
    let mut out = String::with_capacity(url.len());
    let mut rest = url;
    while let Some(start) = rest.find('{') {
        let Some(len) = rest[start..].find('}') else {
            break;
        };
        let name = &rest[start + 1..start + len];
        let input = props
            .keys()
            .find(|k| k.as_str() == name)
            .or_else(|| props.keys().find(|k| k.eq_ignore_ascii_case(name)))
            .map_or(name, String::as_str);
        out.push_str(&rest[..start]);
        out.push('{');
        out.push_str(input);
        out.push('}');
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    out
}

fn build_url(
    spec: &Value,
    path: &str,
    org: &str,
    version: &str,
    host_override: Option<&str>,
) -> Result<String> {
    let host = host_override.unwrap_or_else(|| str_of(spec, "host"));
    if host.is_empty() {
        let title = shown(spec.get("info").and_then(|i| i.get("title")));
        return err(format!("{title}: spec has no host"));
    }
    // A parameterized host such as notification's `{service}dev.azure.com`: its optional parameters are left out,
    // which gives the organization's own host.
    let mut host = host.to_string();
    if let Some(ph) = spec.get("x-ms-parameterized-host") {
        for p in array_of(ph, "parameters") {
            let name = str_of(p, "name");
            if truthy(p.get("required")) {
                return err(format!(
                    "host parameter '{name}' is required; not supported"
                ));
            }
            host = host.replace(&format!("{{{name}}}"), "");
        }
    }
    if host.contains('{') {
        return err(format!("host {host} has an unknown parameter"));
    }
    let base_path = match str_of(spec, "basePath") {
        "" => "/",
        b => b,
    };
    let base = base_path.trim_end_matches('/');
    let mut path = path.split('?').next().unwrap_or("").to_string();
    if host_override.is_some() && !path.contains("{organization}") {
        path = format!("/{{organization}}{path}");
    }
    // `valid_org` allows only letters, digits and `-`, none of which need percent-encoding.
    path = path.replace("{organization}", org);
    // utcp replaces ${NAME} with variables; a literal `$` before a path parameter has to be percent-encoded.
    path = path.replace("${", "%24{");
    Ok(format!("https://{host}{base}{path}?api-version={version}"))
}

/// A body sent as `application/json-patch+json`. The specs describe it as an object (`JsonPatchDocument`); the
/// service takes an array of operations.
fn is_json_patch(op: &Value) -> bool {
    strings(op.get("consumes"))
        .iter()
        .any(|c| c == "application/json-patch+json")
}

fn produces(op: &Value) -> Vec<String> {
    let p = strings(op.get("produces"));
    if p.is_empty() {
        vec!["application/json".into()]
    } else {
        p
    }
}

/// An answer that is only a file or zip: utcp saves it and prints its path (`"response_type": "file"`).
fn is_binary(op: &Value) -> bool {
    produces(op)
        .iter()
        .all(|t| BINARY_TYPES.iter().any(|b| t.starts_with(b)))
}

/// A body that is raw bytes (a file upload): utcp sends the file the call names (`"body_type": "file"`).
fn uploads_file(op: &Value) -> bool {
    let consumes = strings(op.get("consumes"));
    !consumes.is_empty()
        && consumes
            .iter()
            .all(|t| BINARY_TYPES.iter().any(|b| t.starts_with(b)))
}

fn in_list(map: &HashMap<String, Vec<String>>, area: &str, op_id: &str) -> bool {
    map.get(area)
        .is_some_and(|ops| ops.iter().any(|o| o == op_id))
}

/// GETs and the POSTs that only read.
fn is_read_op(area: &str, method: &str, op_id: &str, conf: &Config) -> bool {
    method == "get" || in_list(&conf.post_reads, area, op_id)
}

/// `None` to include the operation, or the reason it is left out.
fn select(area: &str, method: &str, op: &Value, conf: &Config) -> Option<String> {
    let op_id = str_of(op, "operationId");
    if ["head", "options"].contains(&method) {
        return Some(format!("{} never included", method.to_uppercase()));
    }
    if method == "get" {
        if let Some(reason) = conf.skip_gets.get(area).and_then(|m| m.get(op_id)) {
            return Some(format!("skipped: {reason}"));
        }
    }
    if is_read_op(area, method, op_id, conf) {
        return None;
    }
    if conf.read_only {
        return Some("write (--read-only)".into());
    }
    if method == "delete" && !in_list(&conf.deletes, area, op_id) {
        return Some("DELETE not selected".into());
    }
    None
}

/// The HTTP header a header parameter really is, when the spec names the parameter differently (as Microsoft's
/// client libraries map them).
pub fn real_header_name(param: &str) -> Option<&'static str> {
    match param {
        "Version" => Some("If-Match"),
        "contentRangeHeader" => Some("Content-Range"),
        "continuationToken" => Some("x-ms-continuationtoken"),
        _ => None,
    }
}

/// One generated tool before naming.
#[derive(Clone, Debug)]
pub struct Entry {
    pub area: String,
    pub operation_id: String,
    pub method_name: Option<String>,
    pub scopes: Vec<String>,
    /// A GET or a POST that only reads.
    pub read: bool,
    /// Where a tool that is not from the spec comes from: `go`, `node` (client libraries) or `odata`.
    pub source: Option<String>,
    /// Its scopes were inferred (a client-library operation; see `sdkops`).
    pub scopes_inferred: bool,
    pub name: String,
    pub tool: Map<String, Value>,
}

fn auth() -> Value {
    json!({"auth_type": "basic", "username": "pat", "password": format!("${{{PAT_VAR}}}")})
}

fn build_tool(
    spec: &Value,
    area: &str,
    o: &Operation,
    org: &str,
    api_version: &str,
    conf: &Config,
) -> Result<Entry> {
    let op = o.op;
    let Some(op_id) = op.get("operationId").and_then(Value::as_str) else {
        return err(format!(
            "{area}: an operation at {} has no operationId",
            o.path
        ));
    };
    let mut params = Vec::new();
    for p in o.shared.iter().chain(array_of(op, "parameters")) {
        match p.get("$ref").and_then(Value::as_str) {
            Some(r) => params.push(resolve_ref(spec, r)?.1),
            None => params.push(p),
        }
    }
    let mut props = Map::new();
    let mut required: Vec<String> = Vec::new();
    let mut body_field = false;
    let mut header_fields: Vec<String> = Vec::new();
    for p in &params {
        let name = p.get("name").and_then(Value::as_str);
        let place = p.get("in").and_then(Value::as_str);
        if let Some(n) = name {
            if n == "organization" || n == "api-version" {
                continue;
            }
        }
        match place {
            Some("body") => {
                body_field = true;
                let replaced = conf.body_overrides.get(area).and_then(|m| m.get(op_id));
                let mut schema = match replaced {
                    Some(Value::Object(s)) => s.clone(),
                    Some(_) => {
                        return err(format!(
                            "{area}/{op_id}: body_overrides entry is not an object"
                        ))
                    }
                    None if uploads_file(op) => match crate::bodies::file_upload() {
                        Value::Object(s) => s,
                        _ => unreachable!("a JSON object"),
                    },
                    None if is_json_patch(op) => match crate::bodies::json_patch_document() {
                        Value::Object(s) => s,
                        _ => unreachable!("a JSON object"),
                    },
                    None => body_schema(spec, p.get("schema").unwrap_or(&json!({})), 0, &[])?,
                };
                let ours = replaced.is_some() || is_json_patch(op) || uploads_file(op);
                if truthy(p.get("description")) && !(ours && schema.contains_key("description")) {
                    schema.insert(
                        "description".into(),
                        json!(one_line_default(str_of(p, "description"))),
                    );
                }
                props.insert("body".into(), Value::Object(schema));
                if truthy(p.get("required")) {
                    required.push("body".into());
                }
            }
            Some(w @ ("path" | "query" | "header")) => {
                let mut n = name.unwrap_or("").to_string();
                if n == "body" {
                    return err(format!(
                        "{op_id}: a {w} parameter named 'body' clashes with the body field"
                    ));
                }
                // utcp sends a header field under its own name, so it has to be the real header's.
                let real_header = (w == "header").then(|| real_header_name(&n)).flatten();
                if let Some(real) = real_header {
                    n = real.to_string();
                }
                // A create has no version to match yet, so If-Match is never required. An optional path parameter
                // (`x-ms-required: false`) is not required: utcp leaves its segment out when it is not passed, unless
                // that would make the URL another route.
                let optional_path = w == "path"
                    && p.get("x-ms-required") == Some(&Value::Bool(false))
                    && can_leave_out(o.path, &n);
                let mut schema = param_schema(p);
                if let (true, Some(note)) = (optional_path, scope_note(&n)) {
                    let desc = str_of(&schema, "description");
                    let desc = if desc.is_empty() {
                        note.to_string()
                    } else {
                        format!("{desc}. {note}")
                    };
                    schema["description"] = json!(desc);
                }
                props.insert(n.clone(), schema);
                if (w == "path" && !optional_path)
                    || (w != "path" && truthy(p.get("required")) && n != "If-Match")
                {
                    required.push(n.clone());
                }
                if w == "header" {
                    header_fields.push(n);
                }
            }
            other => {
                let other = other.unwrap_or("None");
                return err(format!("{op_id}: unsupported parameter location '{other}'"));
            }
        }
    }
    if let Some(names) = conf.required_params.get(area).and_then(|m| m.get(op_id)) {
        for name in names {
            if !props.contains_key(name) {
                return err(format!(
                    "{area}/{op_id}: REQUIRED_PARAMS names '{name}', which the operation does not have"
                ));
            }
            required.push(name.clone());
        }
    }
    let version = api_version_of(op, api_version)?;
    let produced = produces(op);
    let accept = if produced.iter().any(|t| t == "application/json") {
        "application/json".to_string()
    } else {
        produced[0].clone()
    };
    let mut headers = Map::new();
    headers.insert("Accept".into(), json!(accept));
    let mut template = Map::new();
    template.insert("call_template_type".into(), json!("http"));
    template.insert("http_method".into(), json!(o.method.to_uppercase()));
    let host_override = conf.host_overrides.get(area).map(String::as_str);
    let url = build_url(spec, o.path, org, &version, host_override)?;
    template.insert(
        "url".into(),
        json!(placeholders_named_as_inputs(&url, &props)),
    );
    if body_field {
        let consumes = op
            .get("consumes")
            .and_then(Value::as_array)
            .filter(|c| !c.is_empty());
        let content_type = consumes
            .map(|c| c[0].clone())
            .unwrap_or_else(|| json!("application/json"));
        headers.insert("Content-Type".into(), content_type);
    }
    template.insert("headers".into(), Value::Object(headers));
    if body_field {
        template.insert("body_field".into(), json!("body"));
        if uploads_file(op) {
            template.insert("body_type".into(), json!("file"));
        }
    }
    if is_binary(op) {
        template.insert("response_type".into(), json!("file"));
    }
    if !header_fields.is_empty() {
        header_fields.sort();
        template.insert("header_fields".into(), json!(header_fields));
    }
    // Only the organization's own APIs get the PAT. An operation outside any organization (the public service health
    // page) answers 401 to a PAT that is limited to one, and must not be sent it anyway. A host override puts the
    // operation under the organization.
    if o.path.contains("{organization}") || host_override.is_some() {
        template.insert("auth".into(), auth());
    }
    let mut inputs = Map::new();
    inputs.insert("type".into(), json!("object"));
    inputs.insert("properties".into(), Value::Object(sorted_map(props)));
    if !required.is_empty() {
        let set: BTreeSet<String> = required.into_iter().collect();
        inputs.insert(
            "required".into(),
            json!(set.into_iter().collect::<Vec<_>>()),
        );
    }
    let tag = op
        .get("tags")
        .and_then(Value::as_array)
        .and_then(|t| t.first())
        .and_then(Value::as_str)
        .unwrap_or("");
    let method_name = op
        .get("x-ms-vss-method")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty());
    let mut description =
        tool_description(str_of(op, "description"), area, op_id, method_name, conf);
    let mut tags = vec![json!("azure-devops"), json!(area)];
    if !tag.is_empty() {
        tags.push(json!(snake(tag)));
    }
    if !is_read_op(area, o.method, op_id, conf) && !in_list(&conf.tested_writes, area, op_id) {
        description = format!("{UNTESTED} {description}");
        tags.push(json!("untested"));
    }
    let mut tool = Map::new();
    tool.insert("name".into(), Value::Null);
    tool.insert("description".into(), json!(description));
    tool.insert("tags".into(), Value::Array(tags));
    tool.insert("inputs".into(), Value::Object(inputs));
    let outputs = if is_binary(op) {
        json!({"type": "object", "description": FILE_ANSWER})
    } else if accept.starts_with("text/") {
        json!({"type": "string"})
    } else {
        json!({"type": "object"})
    };
    tool.insert("outputs".into(), outputs);
    tool.insert("tool_call_template".into(), Value::Object(template));
    Ok(Entry {
        area: area.to_string(),
        operation_id: op_id.to_string(),
        method_name: method_name.map(String::from),
        scopes: scopes_of(spec, op),
        read: is_read_op(area, o.method, op_id, conf),
        source: op
            .get("x-azdo-manuals")
            .and_then(|x| x.get("source"))
            .and_then(Value::as_str)
            .map(String::from),
        scopes_inferred: op
            .get("x-azdo-manuals")
            .and_then(|x| x.get("scopes"))
            .and_then(Value::as_str)
            .is_some_and(|s| s != "none"),
        name: String::new(),
        tool,
    })
}

/// A tool's description, before `(untested)`. The operation's own text on one line; config `summaries` replaces its
/// first sentence, and a first sentence that is a developer comment ([`DEVELOPER_COMMENT`]) goes. When nothing is
/// left, a sentence made from the tool's name ([`plain_name`]: "Roledefinitions_List" with the method
/// GetRoleDefinitions -> "Get role definitions"). Config `notes` goes at the end and is kept when the rest is
/// shortened to MAX_DESCRIPTION characters.
pub(crate) fn tool_description(
    text: &str,
    area: &str,
    op_id: &str,
    method_name: Option<&str>,
    conf: &Config,
) -> String {
    let text = one_line(text, usize::MAX);
    let first = first_sentence_len(&text);
    let rest = text[first..].trim_start();
    let configured = |m: &HashMap<String, HashMap<String, String>>| {
        m.get(area).and_then(|ops| ops.get(op_id)).cloned()
    };
    let mut text = match configured(&conf.summaries) {
        Some(summary) if rest.is_empty() => summary,
        Some(summary) => format!("{summary} {rest}"),
        None if DEVELOPER_COMMENT.is_match(&text[..first]) => rest.to_string(),
        None => text.clone(),
    };
    if text.is_empty() {
        text = name_sentence(&plain_name(conf, area, op_id, method_name));
    }
    let Some(note) = configured(&conf.notes) else {
        return one_line(&text, MAX_DESCRIPTION);
    };
    // Room for the note, the space before it and a full stop the text may need.
    let budget = MAX_DESCRIPTION.saturating_sub(note.chars().count() + 2);
    let mut text = one_line(&text, budget);
    if !text.is_empty() && !text.ends_with(['.', '!', '?']) {
        text.push('.');
    }
    format!("{text} {note}").trim_start().to_string()
}

/// The name a tool gets before name clashes are settled: config `names`, else Microsoft's client method
/// (`x-ms-vss-method`, the most reliable source: "GetRoleDefinitions" has its words apart where the operation id
/// "Roledefinitions_List" does not), else the operation id ([`base_name`]).
pub(crate) fn plain_name(
    conf: &Config,
    area: &str,
    op_id: &str,
    method_name: Option<&str>,
) -> String {
    conf.names
        .get(area)
        .and_then(|m| m.get(op_id))
        .filter(|n| !n.is_empty())
        .cloned()
        .unwrap_or_else(|| default_name(method_name, op_id))
}

/// A tool's name from its client method, else from its operation id.
fn default_name(method_name: Option<&str>, op_id: &str) -> String {
    match method_name {
        Some(m) => snake(m),
        None => base_name(op_id),
    }
}

fn singular(words: &str) -> String {
    let (head, last) = match words.rfind('_') {
        Some(i) => (&words[..i], &words[i + 1..]),
        None => ("", words),
    };
    let last = if let Some(stem) = last.strip_suffix("ies") {
        format!("{stem}y")
    } else if last.ends_with("sses") || last.ends_with("xes") {
        last[..last.len() - 2].to_string()
    } else if last.ends_with('s') && !last.ends_with("ss") {
        last[..last.len() - 1].to_string()
    } else {
        last.to_string()
    };
    if head.is_empty() {
        last
    } else {
        format!("{head}_{last}")
    }
}

pub fn base_name(op_id: &str) -> String {
    let (tag, method) = op_id.split_once('_').unwrap_or((op_id, ""));
    let name = snake(if method.is_empty() { tag } else { method });
    if name.contains('_') {
        return name;
    }
    // A bare verb needs its resource: "Teams_List" -> "list_teams", "Teams_Get" -> "get_team".
    let resource = snake(tag);
    if name == "list" {
        format!("{name}_{resource}")
    } else {
        format!("{name}_{}", singular(&resource))
    }
}

/// Name tools after Microsoft's client method (`x-ms-vss-method`, e.g. GetTeamDaysOff -> get_team_days_off), else
/// after the operationId. On a clash, the tool of the manual's first area keeps the name (when that area has only
/// one of them), so azdo_git's get_branches is git's, not tfvc's. Of the others, a tool whose area has no other tool
/// of that name gets the area as prefix, the rest the operation's group, the operationId before `_`
/// (NuGet_Get... -> nuget_get...). `names` in the config overrides any of these.
pub fn assign_names(manual: &str, entries: &mut [Entry], conf: &Config) -> Result<()> {
    let overridden = |e: &Entry| {
        conf.names
            .get(&e.area)
            .and_then(|m| m.get(&e.operation_id))
            .filter(|n| !n.is_empty())
            .cloned()
    };
    let has_override = |e: &Entry| {
        conf.names
            .get(&e.area)
            .is_some_and(|m| m.contains_key(&e.operation_id))
    };
    for e in entries.iter_mut() {
        e.name = overridden(e)
            .unwrap_or_else(|| default_name(e.method_name.as_deref(), &e.operation_id));
    }
    let base: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
    let area_order: Vec<String> = conf
        .manuals
        .iter()
        .find(|m| m.name == manual)
        .map(|m| m.areas.clone())
        .unwrap_or_default();
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, name) in base.iter().enumerate() {
        groups.entry(name.clone()).or_default().push(i);
    }
    for members in groups.values().filter(|m| m.len() > 1) {
        let per_area = |i: usize| {
            members
                .iter()
                .filter(|&&j| entries[j].area == entries[i].area)
                .count()
        };
        let rank = |i: usize| {
            area_order
                .iter()
                .position(|a| *a == entries[i].area)
                .unwrap_or(usize::MAX)
        };
        let first = members.iter().copied().min_by_key(|&i| rank(i));
        let keeps = first.filter(|&i| per_area(i) == 1);
        let prefixed: Vec<(usize, String)> = members
            .iter()
            .filter(|&&i| !has_override(&entries[i]) && Some(i) != keeps)
            .map(|&i| {
                let e = &entries[i];
                let prefix = if per_area(i) > 1 {
                    let group = e.operation_id.split('_').next().unwrap_or("");
                    let words: Vec<String> =
                        group.split_whitespace().map(str::to_lowercase).collect();
                    words.join("_")
                } else {
                    snake(&e.area)
                };
                (i, format!("{prefix}_{}", base[i]))
            })
            .collect();
        for (i, name) in prefixed {
            entries[i].name = name;
        }
    }
    let mut seen: HashMap<String, String> = HashMap::new();
    for e in entries.iter_mut() {
        if let Some(first) = seen.get(&e.name) {
            return err(format!(
                "{manual}: tool name '{}' is used by {first} and {}/{}; add one of them to config names",
                e.name, e.area, e.operation_id
            ));
        }
        seen.insert(e.name.clone(), format!("{}/{}", e.area, e.operation_id));
        e.tool.insert("name".into(), json!(e.name));
    }
    Ok(())
}

/// Name the tools that do not come from the spec (client libraries, OData) after the spec's tools are named, so
/// adding them never renames a spec tool. A tool is named after its method; when that name is taken by a spec tool or
/// shared with another such tool, it gets its area as prefix (or, when its area has several of that name, its
/// resource). `names` in the config overrides any of these.
pub fn assign_extra_names(
    manual: &str,
    entries: &mut [Entry],
    taken: &HashSet<String>,
    conf: &Config,
) -> Result<()> {
    let overridden = |e: &Entry| {
        conf.names
            .get(&e.area)
            .and_then(|m| m.get(&e.operation_id))
            .filter(|n| !n.is_empty())
            .cloned()
    };
    let base: Vec<String> = entries
        .iter()
        .map(|e| default_name(e.method_name.as_deref(), &e.operation_id))
        .collect();
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for b in &base {
        *counts.entry(b.as_str()).or_default() += 1;
    }
    let names: Vec<String> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            if let Some(n) = overridden(e) {
                return n;
            }
            let b = &base[i];
            if counts[b.as_str()] == 1 && !taken.contains(b) {
                return b.clone();
            }
            let same_area = entries
                .iter()
                .enumerate()
                .filter(|(j, o)| base[*j] == *b && o.area == e.area)
                .count();
            let prefix = if same_area > 1 {
                snake(e.operation_id.split(['.', '_']).next().unwrap_or(""))
            } else {
                snake(&e.area)
            };
            format!("{prefix}_{b}")
        })
        .collect();
    let mut seen: HashMap<String, String> = HashMap::new();
    for (e, name) in entries.iter_mut().zip(names) {
        if taken.contains(&name) {
            return err(format!(
                "{manual}: tool name '{name}' of {}/{} is already a spec tool; add it to config names",
                e.area, e.operation_id
            ));
        }
        if let Some(first) = seen.get(&name) {
            return err(format!(
                "{manual}: tool name '{name}' is used by {first} and {}/{}; add one of them to config names",
                e.area, e.operation_id
            ));
        }
        seen.insert(name.clone(), format!("{}/{}", e.area, e.operation_id));
        e.tool.insert("name".into(), json!(name));
        e.name = name;
    }
    Ok(())
}

fn scope_info(specs: &Specs) -> HashMap<String, String> {
    let mut info = HashMap::new();
    for files in specs.values() {
        for spec in files {
            let scopes = spec
                .get("securityDefinitions")
                .and_then(|s| s.get("oauth2"))
                .and_then(|o| o.get("scopes"))
                .and_then(Value::as_object);
            for (name, desc) in scopes.into_iter().flatten() {
                info.entry(name.clone())
                    .or_insert_with(|| one_line(desc.as_str().unwrap_or(""), 300));
            }
        }
    }
    info
}

pub fn is_write_scope(scope: &str) -> bool {
    ["_write", "_manage", "_full", "_execute"]
        .iter()
        .any(|s| scope.ends_with(s))
}

/// Smallest practical set of scopes so every operation has one of its own (an operation listing several scopes
/// accepts any of them). Prefers read scopes and scopes already chosen.
pub fn covering_scopes(scope_sets: &[Vec<String>]) -> Vec<String> {
    let mut sets: Vec<&Vec<String>> = scope_sets.iter().filter(|s| !s.is_empty()).collect();
    sets.sort_by_key(|s| s.len());
    let mut chosen: BTreeSet<String> = BTreeSet::new();
    for scopes in sets {
        if !scopes.iter().any(|s| chosen.contains(s)) {
            let best = scopes
                .iter()
                .min_by_key(|s| (is_write_scope(s), s.as_str()))
                .expect("not empty");
            chosen.insert(best.clone());
        }
    }
    chosen.into_iter().collect()
}

/// Drop a scope when a broader one of the same family is also needed (vso.work_write covers vso.work,
/// vso.taskgroups_manage covers vso.taskgroups_read, vso.serviceendpoint_manage covers vso.serviceendpoint_query).
pub fn minimal_scopes(scopes: &BTreeSet<String>) -> Vec<String> {
    let order = [
        "", "_read", "_use", "_query", "_write", "_execute", "_manage", "_full",
    ];
    let split = |s: &str| -> (String, usize) {
        for (rank, suffix) in order.iter().enumerate().skip(1).rev() {
            if let Some(family) = s.strip_suffix(suffix) {
                return (family.to_string(), rank);
            }
        }
        (s.to_string(), 0)
    };
    let mut best: HashMap<String, (String, usize)> = HashMap::new();
    for s in scopes {
        let (family, rank) = split(s);
        if best.get(&family).is_none_or(|b| rank > b.1) {
            best.insert(family, (s.clone(), rank));
        }
    }
    let mut out: Vec<String> = best.into_values().map(|v| v.0).collect();
    out.sort();
    out
}

/// Every operationId the configuration names must exist in its area, so a typo cannot silently drop a tool.
/// Areas without a spec are skipped, and so are names of client-library operations (`<Resource>.<Method>`) when the
/// live routes were not read. A body override must name an operation that takes a body.
fn check_config_names(specs: &Specs, extra: &Extra, conf: &Config) -> Result<()> {
    for (setting, area, op_id) in conf.named_operations() {
        let files: Vec<&Value> = specs
            .get(area)
            .into_iter()
            .flatten()
            .chain(extra.specs.get(area).into_iter().flatten())
            .collect();
        if files.is_empty() {
            continue;
        }
        let found = files.iter().find_map(|spec| {
            iter_operations(spec)
                .into_iter()
                .find(|o| str_of(o.op, "operationId") == op_id)
                .map(|o| (*spec, o))
        });
        let Some((spec, o)) = found else {
            // A client-library operation (`<Resource>.<Method>`) exists only when the live routes were read.
            if !extra.live && op_id.contains('.') {
                continue;
            }
            return err(format!(
                "config {setting} names {area}/{op_id}, which the specs do not have"
            ));
        };
        if setting == "body_overrides" {
            let mut has_body = false;
            for p in o.shared.iter().chain(array_of(o.op, "parameters")) {
                let p = match p.get("$ref").and_then(Value::as_str) {
                    Some(r) => resolve_ref(spec, r)?.1,
                    None => p,
                };
                has_body |= str_of(p, "in") == "body";
            }
            if !has_body {
                return err(format!(
                    "config body_overrides names {area}/{op_id}, which takes no body"
                ));
            }
        }
    }
    Ok(())
}

/// utcp refuses a manual hint longer than this (`MAX_MANUAL_HINT_CHARS` in utcp-core).
const UTCP_MAX_HINT_CHARS: usize = 60;

/// The manuals' hints with `, org <org>` added, so an agent reading the skill knows which organization the tools
/// reach (the org is fixed in every URL; no tool takes it as a parameter). When one of them would pass utcp's limit
/// (a long organization name), every manual keeps its plain hint, so the manuals of one org always look alike.
pub fn manual_hints(conf: &Config, org: &str) -> HashMap<String, String> {
    let with_org: HashMap<String, String> = conf
        .manuals
        .iter()
        .map(|m| (m.name.clone(), format!("{}, org {org}", m.hint)))
        .collect();
    if with_org
        .values()
        .all(|h| h.chars().count() <= UTCP_MAX_HINT_CHARS)
    {
        return with_org;
    }
    conf.manuals
        .iter()
        .map(|m| (m.name.clone(), m.hint.clone()))
        .collect()
}

/// Inputs besides the spec: operations built from the client libraries and the live routes (`sdkops`), and tools
/// built elsewhere (`analytics`). `Extra::default()` is a spec-only run.
#[derive(Debug, Default)]
pub struct Extra {
    /// Area -> one-operation specs built from the client libraries.
    pub specs: Specs,
    /// The live routes were read (so every operation the configuration names can be checked).
    pub live: bool,
    /// Operations left out before generation, as report `left_out` entries.
    pub left_out: Vec<Value>,
    /// Finished tools (named later), with the manual each goes into.
    pub entries: Vec<(String, Entry)>,
    /// Descriptions of scopes the specs do not describe.
    pub scope_descriptions: HashMap<String, String>,
    /// The report's `extra` section (null in a spec-only run).
    pub report: Value,
}

/// Returns ({manual name: manual}, report).
pub fn generate(
    specs: &Specs,
    org: &str,
    api_version: &str,
    source: &Value,
    conf: &Config,
) -> Result<(IndexMap<String, Value>, Value)> {
    generate_with(specs, &Extra::default(), org, api_version, source, conf)
}

/// What a manual was generated from, for its description.
fn made_from(entries: &[Entry]) -> String {
    let has = |f: &dyn Fn(Option<&str>) -> bool| entries.iter().any(|e| f(e.source.as_deref()));
    let mut parts = Vec::new();
    if has(&|s| s.is_none()) {
        parts.push("Microsoft's REST API specs");
    }
    if has(&|s| matches!(s, Some("go" | "node"))) {
        parts.push("Microsoft's client libraries");
    }
    if has(&|s| s == Some("odata")) {
        parts.push("the Analytics service's OData metadata");
    }
    parts.join(" and ")
}

/// Returns ({manual name: manual}, report).
pub fn generate_with(
    specs: &Specs,
    extra: &Extra,
    org: &str,
    api_version: &str,
    source: &Value,
    conf: &Config,
) -> Result<(IndexMap<String, Value>, Value)> {
    if !valid_org(org) {
        return err(format!(
            "'{org}' is not a valid Azure DevOps organization name"
        ));
    }
    check_config_names(specs, extra, conf)?;
    let hints = manual_hints(conf, org);
    let mut manuals = IndexMap::new();
    let mut report_manuals = Map::new();
    let mut left_out = Vec::new();
    let mut descriptions = scope_info(specs);
    for (scope, text) in &extra.scope_descriptions {
        descriptions
            .entry(scope.clone())
            .or_insert_with(|| text.clone());
    }
    let mut all_scopes: BTreeSet<String> = BTreeSet::new();
    let mut all_scope_sets: Vec<Vec<String>> = Vec::new();
    let mut read_scope_sets: Vec<Vec<String>> = Vec::new();
    for mconf in &conf.manuals {
        let mut entries = Vec::new();
        for area in &mconf.all_areas() {
            let files = specs
                .get(area)
                .into_iter()
                .flatten()
                .chain(extra.specs.get(area).into_iter().flatten());
            for spec in files {
                for o in iter_operations(spec) {
                    let leave_out = |reason: String| {
                        json!({"manual": mconf.name, "area": area, "method": o.method.to_uppercase(),
                               "operation_id": o.op.get("operationId"), "reason": reason})
                    };
                    if let Some(reason) = select(area, o.method, o.op, conf) {
                        left_out.push(leave_out(reason));
                        continue;
                    }
                    let entry = build_tool(spec, area, &o, org, api_version, conf)?;
                    let write_only: Vec<&String> =
                        entry.scopes.iter().filter(|s| is_write_scope(s)).collect();
                    // Some reads accept only a write-level scope (there is no read scope for deployment groups,
                    // for example). A read-only PAT cannot use them.
                    if conf.read_only
                        && !entry.scopes.is_empty()
                        && write_only.len() == entry.scopes.len()
                    {
                        let names: Vec<&str> = write_only.iter().map(|s| s.as_str()).collect();
                        left_out.push(leave_out(format!(
                            "read needs a write scope: {}",
                            names.join(", ")
                        )));
                        continue;
                    }
                    entries.push(entry);
                }
            }
        }
        entries.extend(
            extra
                .entries
                .iter()
                .filter(|(m, _)| *m == mconf.name)
                .map(|(_, e)| e.clone()),
        );
        if entries.is_empty() {
            continue;
        }
        entries.sort_by(|a, b| (&a.area, &a.operation_id).cmp(&(&b.area, &b.operation_id)));
        let (mut spec_entries, mut other): (Vec<Entry>, Vec<Entry>) =
            entries.into_iter().partition(|e| e.source.is_none());
        assign_names(&mconf.name, &mut spec_entries, conf)?;
        let taken: HashSet<String> = spec_entries.iter().map(|e| e.name.clone()).collect();
        assign_extra_names(&mconf.name, &mut other, &taken, conf)?;
        let mut entries = spec_entries;
        entries.extend(other);
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let scope_sets: Vec<Vec<String>> = entries.iter().map(|e| e.scopes.clone()).collect();
        // Per manual, too, only the broadest level of each permission (Code: Read, write, & manage, not also
        // Code: Read and Code: Read & write).
        let scopes = minimal_scopes(&covering_scopes(&scope_sets).into_iter().collect());
        all_scopes.extend(scope_sets.iter().flatten().cloned());
        all_scope_sets.extend(scope_sets);
        // The scopes a read-only PAT needs: those of the reads that a read scope allows.
        read_scope_sets.extend(
            entries
                .iter()
                .filter(|e| e.read && !e.scopes.iter().all(|s| is_write_scope(s)))
                .map(|e| e.scopes.clone()),
        );
        let missing: Vec<&String> = mconf
            .areas
            .iter()
            .filter(|a| !specs.contains_key(*a))
            .collect();
        let manual = json!({
            "manual_version": "1.0.0",
            "utcp_version": "1.0.0",
            "info": {
                "title": mconf.title,
                "version": api_version,
                "description": format!(
                    "{} for the '{org}' organization (REST API {api_version}). Generated by azdo-manuals from \
                     {}. Authenticates with the PAT in {PAT_VAR}.",
                    mconf.title,
                    made_from(&entries)
                ),
            },
            "tools": entries.iter().map(|e| Value::Object(e.tool.clone())).collect::<Vec<_>>(),
        });
        let text = compact_ascii(&manual);
        let reads = entries.iter().filter(|e| e.read).count();
        let writes = entries.len() - reads;
        let untested = entries
            .iter()
            .filter(|e| {
                e.tool["tags"]
                    .as_array()
                    .is_some_and(|t| t.contains(&json!("untested")))
            })
            .count();
        let mut without_scope: Vec<&String> = entries
            .iter()
            .filter(|e| e.scopes.is_empty())
            .map(|e| &e.name)
            .collect();
        without_scope.sort();
        let mut sources = Map::new();
        for e in &entries {
            let key = e.source.clone().unwrap_or_else(|| "spec".into());
            let n = sources.get(&key).and_then(Value::as_u64).unwrap_or(0);
            sources.insert(key, json!(n + 1));
        }
        let mut inferred: Vec<&String> = entries
            .iter()
            .filter(|e| e.scopes_inferred)
            .map(|e| &e.name)
            .collect();
        inferred.sort();
        report_manuals.insert(
            mconf.name.clone(),
            json!({
                "hint": hints[&mconf.name],
                "areas": mconf.all_areas(),
                "areas_without_spec": missing,
                "tools": entries.len(),
                "sources": sources,
                "reads": reads,
                "writes": writes,
                "untested_writes": untested,
                "bytes": text.len(),
                "approx_tokens": text.len() / 4,
                "scopes": scopes,
                "tools_without_scope": without_scope,
                "tools_with_inferred_scopes": inferred,
            }),
        );
        manuals.insert(mconf.name.clone(), manual);
    }
    let described = |scopes: Vec<String>| -> Vec<Value> {
        scopes
            .into_iter()
            .map(|s| {
                let description = descriptions.get(&s).cloned().unwrap_or_default();
                json!({"scope": s, "description": description})
            })
            .collect()
    };
    let needed: BTreeSet<String> = covering_scopes(&all_scope_sets).into_iter().collect();
    let scopes = described(minimal_scopes(&needed));
    let needed: BTreeSet<String> = covering_scopes(&read_scope_sets).into_iter().collect();
    let read_scopes = described(minimal_scopes(&needed));
    let excluded: Vec<Value> = conf
        .excluded_areas
        .iter()
        .map(|(area, reason)| json!({"area": area, "reason": reason}))
        .collect();
    left_out.extend(extra.left_out.iter().cloned());
    let report = json!({
        "api_version": api_version,
        "source": source,
        "extra": extra.report,
        "manuals": report_manuals,
        "excluded_areas": excluded,
        "left_out": left_out,
        "scopes": scopes,
        "read_scopes": read_scopes,
        "scopes_all": all_scopes,
    });
    Ok((manuals, report))
}
