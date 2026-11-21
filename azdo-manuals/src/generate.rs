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
use crate::text::{compact_ascii, one_line, one_line_default, shown, snake, sorted_map, truthy};

pub const PAT_VAR: &str = "AZURE_DEVOPS_EXT_PAT";
const BINARY_TYPES: [&str; 3] = ["application/zip", "application/octet-stream", "image/"];
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

/// The api-version an operation needs, from its `#/parameters/api-Version[-preview.N]` reference.
pub fn api_version_of(op: &Value, base_version: &str) -> Result<String> {
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

fn scopes_of(spec: &Value, op: &Value) -> Vec<String> {
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

fn build_url(
    spec: &Value,
    path: &str,
    org: &str,
    version: &str,
    dropped: &[String],
) -> Result<String> {
    let host = str_of(spec, "host");
    if host.is_empty() {
        let title = shown(spec.get("info").and_then(|i| i.get("title")));
        return err(format!("{title}: spec has no host"));
    }
    let base_path = match str_of(spec, "basePath") {
        "" => "/",
        b => b,
    };
    let base = base_path.trim_end_matches('/');
    let mut path = path.split('?').next().unwrap_or("").to_string();
    for name in dropped {
        path = path.replacen(&format!("/{{{name}}}"), "", 1);
    }
    // `valid_org` allows only letters, digits and `-`, none of which need percent-encoding.
    path = path.replace("{organization}", org);
    // utcp replaces ${NAME} with variables; a literal `$` before a path parameter has to be percent-encoded.
    path = path.replace("${", "%24{");
    Ok(format!("https://{host}{base}{path}?api-version={version}"))
}

fn produces(op: &Value) -> Vec<String> {
    let p = strings(op.get("produces"));
    if p.is_empty() {
        vec!["application/json".into()]
    } else {
        p
    }
}

fn is_binary(op: &Value) -> bool {
    produces(op)
        .iter()
        .all(|t| BINARY_TYPES.iter().any(|b| t.starts_with(b)))
}

fn in_list(map: &HashMap<String, Vec<String>>, area: &str, op_id: &str) -> bool {
    map.get(area)
        .is_some_and(|ops| ops.iter().any(|o| o == op_id))
}

/// `None` to include the operation, or the reason it is left out.
fn select(area: &str, method: &str, op: &Value, conf: &Config) -> Option<String> {
    let op_id = str_of(op, "operationId");
    if method == "get" {
        if let Some(reason) = conf.skip_gets.get(area).and_then(|m| m.get(op_id)) {
            return Some(format!("skipped: {reason}"));
        }
        if is_binary(op) {
            return Some("binary response".into());
        }
        return None;
    }
    if ["delete", "head", "options"].contains(&method) {
        return Some(format!("{} never included", method.to_uppercase()));
    }
    if in_list(&conf.post_reads, area, op_id) || in_list(&conf.writes, area, op_id) {
        return None;
    }
    Some("write not selected".into())
}

/// One generated tool before naming.
#[derive(Clone, Debug)]
pub struct Entry {
    pub area: String,
    pub operation_id: String,
    pub method_name: Option<String>,
    pub scopes: Vec<String>,
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
    let drop = conf.drop_optional_path_params.get(area);
    let dropped: Vec<String> = params
        .iter()
        .filter(|p| {
            p.get("in").and_then(Value::as_str) == Some("path")
                && drop.is_some_and(|d| d.contains(str_of(p, "name")))
                && p.get("x-ms-required") == Some(&Value::Bool(false))
        })
        .map(|p| str_of(p, "name").to_string())
        .collect();
    let mut props = Map::new();
    let mut required: Vec<String> = Vec::new();
    let mut body_field = false;
    let mut header_fields: Vec<String> = Vec::new();
    for p in &params {
        let name = p.get("name").and_then(Value::as_str);
        let place = p.get("in").and_then(Value::as_str);
        if let Some(n) = name {
            if n == "organization" || n == "api-version" || dropped.iter().any(|d| d == n) {
                continue;
            }
        }
        match place {
            Some("body") => {
                body_field = true;
                let mut schema = body_schema(spec, p.get("schema").unwrap_or(&json!({})), 0, &[])?;
                if truthy(p.get("description")) {
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
                let n = name.unwrap_or("").to_string();
                if n == "body" {
                    return err(format!(
                        "{op_id}: a {w} parameter named 'body' clashes with the body field"
                    ));
                }
                props.insert(n.clone(), param_schema(p));
                if w == "path" || truthy(p.get("required")) {
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
    template.insert(
        "url".into(),
        json!(build_url(spec, o.path, org, &version, &dropped)?),
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
    }
    if !header_fields.is_empty() {
        header_fields.sort();
        template.insert("header_fields".into(), json!(header_fields));
    }
    template.insert("auth".into(), auth());
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
    let mut description = one_line_default(str_of(op, "description"));
    if description.is_empty() {
        description = op_id.replace('_', ": ");
    }
    let mut tags = vec![json!("azure-devops"), json!(area)];
    if !tag.is_empty() {
        tags.push(json!(snake(tag)));
    }
    let mut tool = Map::new();
    tool.insert("name".into(), Value::Null);
    tool.insert("description".into(), json!(description));
    tool.insert("tags".into(), Value::Array(tags));
    tool.insert("inputs".into(), Value::Object(inputs));
    let output_type = if accept.starts_with("text/") {
        "string"
    } else {
        "object"
    };
    tool.insert("outputs".into(), json!({"type": output_type}));
    tool.insert("tool_call_template".into(), Value::Object(template));
    let method_name = op
        .get("x-ms-vss-method")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty());
    Ok(Entry {
        area: area.to_string(),
        operation_id: op_id.to_string(),
        method_name: method_name.map(String::from),
        scopes: scopes_of(spec, op),
        name: String::new(),
        tool,
    })
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
/// after the operationId. On a clash, prefix the area; `names` in the config overrides either.
pub fn assign_names(manual: &str, entries: &mut [Entry], conf: &Config) -> Result<()> {
    let overridden = |e: &Entry| {
        conf.names
            .get(&e.area)
            .and_then(|m| m.get(&e.operation_id))
            .filter(|n| !n.is_empty())
            .cloned()
    };
    for e in entries.iter_mut() {
        e.name = overridden(e).unwrap_or_else(|| match &e.method_name {
            Some(m) => snake(m),
            None => base_name(&e.operation_id),
        });
    }
    let mut counts: HashMap<String, usize> = HashMap::new();
    for e in entries.iter() {
        *counts.entry(e.name.clone()).or_default() += 1;
    }
    for e in entries.iter_mut() {
        let has_override = conf
            .names
            .get(&e.area)
            .is_some_and(|m| m.contains_key(&e.operation_id));
        if counts[&e.name] > 1 && !has_override {
            e.name = format!("{}_{}", snake(&e.area), e.name);
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

/// Drop a scope when a broader one of the same family is also needed (vso.work_write covers vso.work).
pub fn minimal_scopes(scopes: &BTreeSet<String>) -> Vec<String> {
    let order = ["", "_write", "_execute", "_manage", "_full"];
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

/// Returns ({manual name: manual}, report).
pub fn generate(
    specs: &Specs,
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
    let mut manuals = IndexMap::new();
    let mut report_manuals = Map::new();
    let mut left_out = Vec::new();
    let descriptions = scope_info(specs);
    let mut all_scopes: BTreeSet<String> = BTreeSet::new();
    let mut all_scope_sets: Vec<Vec<String>> = Vec::new();
    for mconf in &conf.manuals {
        let mut entries = Vec::new();
        for area in &mconf.areas {
            for spec in specs.get(area).map(Vec::as_slice).unwrap_or(&[]) {
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
                    let is_write = in_list(&conf.writes, area, &entry.operation_id);
                    if !entry.scopes.is_empty()
                        && write_only.len() == entry.scopes.len()
                        && !is_write
                        && !write_only
                            .iter()
                            .any(|s| conf.allow_write_scopes.contains(*s))
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
        if entries.is_empty() {
            continue;
        }
        entries.sort_by(|a, b| (&a.area, &a.operation_id).cmp(&(&b.area, &b.operation_id)));
        assign_names(&mconf.name, &mut entries, conf)?;
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let scope_sets: Vec<Vec<String>> = entries.iter().map(|e| e.scopes.clone()).collect();
        let scopes = covering_scopes(&scope_sets);
        all_scopes.extend(scope_sets.iter().flatten().cloned());
        all_scope_sets.extend(scope_sets);
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
                     Microsoft's REST API specs. Authenticates with the PAT in {PAT_VAR}.",
                    mconf.title
                ),
            },
            "tools": entries.iter().map(|e| Value::Object(e.tool.clone())).collect::<Vec<_>>(),
        });
        let text = compact_ascii(&manual);
        let reads = entries
            .iter()
            .filter(|e| {
                e.tool["tool_call_template"]["http_method"] == "GET"
                    || in_list(&conf.post_reads, &e.area, &e.operation_id)
            })
            .count();
        let mut without_scope: Vec<&String> = entries
            .iter()
            .filter(|e| e.scopes.is_empty())
            .map(|e| &e.name)
            .collect();
        without_scope.sort();
        report_manuals.insert(
            mconf.name.clone(),
            json!({
                "hint": mconf.hint,
                "areas": mconf.areas,
                "areas_without_spec": missing,
                "tools": entries.len(),
                "reads": reads,
                "bytes": text.len(),
                "approx_tokens": text.len() / 4,
                "scopes": scopes,
                "tools_without_scope": without_scope,
            }),
        );
        manuals.insert(mconf.name.clone(), manual);
    }
    let needed: BTreeSet<String> = covering_scopes(&all_scope_sets).into_iter().collect();
    let scopes: Vec<Value> = minimal_scopes(&needed)
        .into_iter()
        .map(|s| {
            let description = descriptions.get(&s).cloned().unwrap_or_default();
            json!({"scope": s, "description": description})
        })
        .collect();
    let report = json!({
        "api_version": api_version,
        "source": source,
        "manuals": report_manuals,
        "left_out": left_out,
        "scopes": scopes,
        "scopes_all": all_scopes,
    });
    Ok((manuals, report))
}
