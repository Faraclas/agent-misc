//! Parse the Node client library (`api/*Api.ts`, `api/*ApiBase.ts`, `api/interfaces/**/*.ts`).
//!
//! Each method is `public async <name>(<params>): Promise<T>` with a JSDoc comment (`@param {type} name - text`). It
//! builds `routeValues` and `queryValues` objects (keys are the names on the wire; `queryValues.x = ...` adds more),
//! calls `getVersioningData("<version>", "<area>", "<guid>", ...)`, then `this.rest.get|create|update|replace|del`,
//! `this.rest.uploadStream("<VERB>", ...)` or, for a stream answer, `this.http.get|post` with an Accept header. Nested
//! query objects are sent flattened (`searchCriteria.status=...`), like the REST spec describes them.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

use crate::sdk::{
    balanced, lower_first, split_top, upper_first, Method, Param, Parsed, Place, Sdk, Ty, TypeDef,
};

static IMPORT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^import (\w+) = require\(["'][^"']*/(\w+)["']\);"#).unwrap()
});
static INTERFACE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^export interface (\w+)(?:<[^>]*>)?(?: extends ([^{]+))? \{").unwrap()
});
static ENUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^export enum (\w+) \{").unwrap());
static MEMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^    (\w+)\??: (.+);$").unwrap());
static ENUM_VALUE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^    (\w+)(?: = [^,]+)?,?$").unwrap());
static METHOD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^    public async (\w+)\(").unwrap());
static PARAM_DOC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"@param \{[^}]*\} (\w+)(?: - (.*))?").unwrap());
static VERSIONING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"getVersioningData\(\s*"([^"]+)",\s*"[^"]*",\s*"([0-9a-fA-F-]{36})""#).unwrap()
});
static CALL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"this\.(rest|http)\.(get|create|update|replace|del|uploadStream|post|options)\s*(?:<[^(]*>)?\(").unwrap()
});
static QUERY_ASSIGN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"queryValues\.(\w+) = ([\w.]+);").unwrap());
static REQUEST_OPTIONS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"create(?:RequestOptions|AcceptHeader)\(\s*['"]([^'"]+)['"]"#).unwrap()
});
static CONTENT_TYPE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"customHeaders\["Content-Type"\] = "([^"]+)""#).unwrap());
static LEADING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([A-Za-z_$][\w$]*)(?:\.([A-Za-z_$][\w$]*))?").unwrap());

/// Interfaces before inheritance: key -> (description, `extends` list, own properties).
type RawInterfaces = HashMap<String, (String, Vec<Ty>, Vec<(String, Ty, String)>)>;

/// The JSDoc block (`/** ... */`) ending just above `offset`, without its stars.
fn jsdoc_above(text: &str, offset: usize) -> String {
    let before = text[..offset].trim_end();
    let Some(head) = before.strip_suffix("*/") else {
        return String::new();
    };
    let Some(start) = head.rfind("/**") else {
        return String::new();
    };
    head[start + 3..]
        .lines()
        .map(|l| l.trim().trim_start_matches('*').trim())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Text of a JSDoc block before its first `@tag`, as one line.
fn doc_text(doc: &str) -> String {
    let lines: Vec<&str> = doc.lines().take_while(|l| !l.starts_with('@')).collect();
    lines
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// A TypeScript type expression to a `Ty`. `aliases` maps import names to modules; `module` qualifies bare names.
pub fn ts_type(expr: &str, module: &str, aliases: &HashMap<String, String>) -> Ty {
    let e = expr.trim();
    if let Some(inner) = e.strip_suffix("[]") {
        return Ty::Array(Box::new(ts_type(inner, module, aliases)));
    }
    if e.starts_with('{') && e.contains("[key:") {
        let value = e
            .split_once("]")
            .map(|(_, v)| {
                v.trim()
                    .trim_start_matches(':')
                    .trim()
                    .trim_end_matches('}')
                    .trim()
                    .trim_end_matches(';')
            })
            .unwrap_or("any");
        return Ty::Map(Box::new(ts_type(value, module, aliases)));
    }
    if e.contains('|') || e.contains('{') || e.contains('<') {
        return Ty::Any;
    }
    match e {
        "string" => Ty::String,
        "number" => Ty::Number,
        "boolean" => Ty::Boolean,
        "Date" => Ty::DateTime,
        "any" | "Object" | "object" | "void" => Ty::Any,
        "NodeJS.ReadableStream" | "ArrayBuffer" | "Buffer" => Ty::Stream,
        _ => match e.split_once('.') {
            Some((alias, name)) => match aliases.get(alias) {
                Some(m) => Ty::Named(format!("node:{m}.{name}")),
                None => Ty::Any,
            },
            None => Ty::Named(format!("node:{module}.{e}")),
        },
    }
}

fn aliases_of(text: &str) -> HashMap<String, String> {
    IMPORT
        .captures_iter(text)
        .map(|c| (c[1].to_string(), c[2].to_string()))
        .collect()
}

/// Interfaces (with their `extends` list, resolved later) and enums of one interfaces file.
fn parse_interfaces(module: &str, text: &str, raw: &mut RawInterfaces, parsed: &mut Parsed) {
    let aliases = aliases_of(text);
    for cap in INTERFACE.captures_iter(text) {
        let at = cap.get(0).unwrap();
        let description = doc_text(&jsdoc_above(text, at.start()));
        let parents: Vec<Ty> = cap
            .get(2)
            .map(|p| {
                split_top(p.as_str())
                    .iter()
                    .map(|t| ts_type(t, module, &aliases))
                    .collect()
            })
            .unwrap_or_default();
        let body = balanced(text, at.end() - 1).unwrap_or("");
        let mut properties = Vec::new();
        let mut offset = 0;
        for line in body.split_inclusive('\n') {
            let trimmed = line.trim_end_matches(['\n', '\r']);
            if let Some(m) = MEMBER.captures(trimmed) {
                let doc = doc_text(&jsdoc_above(body, offset));
                properties.push((m[1].to_string(), ts_type(&m[2], module, &aliases), doc));
            }
            offset += line.len();
        }
        raw.insert(
            format!("node:{module}.{}", &cap[1]),
            (description, parents, properties),
        );
    }
    for cap in ENUM.captures_iter(text) {
        let at = cap.get(0).unwrap();
        let description = doc_text(&jsdoc_above(text, at.start()));
        let body = balanced(text, at.end() - 1).unwrap_or("");
        let values = body
            .lines()
            .filter_map(|l| ENUM_VALUE.captures(l).map(|c| lower_first(&c[1])))
            .collect();
        parsed.types.insert(
            format!("node:{module}.{}", &cap[1]),
            TypeDef::Enum {
                description,
                values,
            },
        );
    }
}

/// `key: value` pairs of an object literal (keys unquoted).
fn object_pairs(literal: &str) -> Vec<(String, String)> {
    split_top(literal)
        .into_iter()
        .filter_map(|pair| {
            let (k, v) = pair.split_once(':')?;
            let key = k.trim().trim_matches(|c| c == '\'' || c == '"').to_string();
            Some((key, v.trim().to_string()))
        })
        .collect()
}

/// The object literal assigned to `let <var>: any = {...}` in `body`.
fn literal_of<'a>(body: &'a str, var: &str) -> Option<&'a str> {
    let at = body.find(&format!("let {var}: any = {{"))?;
    let open = at + body[at..].find('{')?;
    balanced(body, open)
}

fn parse_api(
    module: &str,
    text: &str,
    types: &HashMap<String, TypeDef>,
    methods: &mut Vec<Method>,
) {
    let aliases = aliases_of(text);
    let starts: Vec<(usize, usize, String)> = METHOD
        .captures_iter(text)
        .map(|c| {
            let m = c.get(0).unwrap();
            (m.start(), m.end() - 1, c[1].to_string())
        })
        .collect();
    for (n, (start, open, name)) in starts.iter().enumerate() {
        let end = starts.get(n + 1).map_or(text.len(), |s| s.0);
        let body = &text[*start..end];
        let Some(v) = VERSIONING.captures(body) else {
            continue;
        };
        let Some(call) = CALL.captures(body) else {
            continue;
        };
        let doc = jsdoc_above(text, *start);
        let param_docs: HashMap<String, String> = PARAM_DOC
            .captures_iter(&doc)
            .map(|c| {
                (
                    c[1].to_string(),
                    c.get(2).map_or("", |d| d.as_str()).trim().to_string(),
                )
            })
            .collect();
        // Signature: name -> (type, required).
        let signature: Vec<(String, Ty, bool)> = balanced(text, *open)
            .map(split_top)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|p| {
                let (n, t) = p.split_once(':')?;
                let optional = n.trim().ends_with('?');
                let n = n.trim().trim_end_matches('?').to_string();
                Some((n, ts_type(t, module, &aliases), !optional))
            })
            .filter(|(n, _, _)| n != "customHeaders")
            .collect();
        let arg = |n: &str| signature.iter().find(|s| s.0 == n);
        let describe = |n: &str| param_docs.get(n).cloned().unwrap_or_default();
        let mut params: Vec<Param> = Vec::new();
        let mut route_constants: Vec<(String, String)> = Vec::new();
        if let Some(literal) = literal_of(body, "routeValues") {
            for (key, expr) in object_pairs(literal) {
                let quoted = expr
                    .strip_prefix(['"', '\''])
                    .and_then(|v| v.strip_suffix(['"', '\'']));
                if let Some(constant) = quoted {
                    route_constants.push((key, constant.to_string()));
                    continue;
                }
                let (ty, required, description) = match arg(&expr) {
                    Some((n, t, r)) => (t.clone(), *r, describe(n)),
                    // `project = teamContext.projectId || teamContext.project;`
                    None if body.contains(&format!("{expr} = teamContext.")) => (
                        Ty::String,
                        true,
                        if key == "team" {
                            "Team ID or team name".to_string()
                        } else {
                            "Project ID or project name".to_string()
                        },
                    ),
                    None => (Ty::String, true, String::new()),
                };
                params.push(Param {
                    name: key,
                    place: Place::Path,
                    ty,
                    required,
                    description,
                });
            }
        }
        let mut query: Vec<(String, String)> = literal_of(body, "queryValues")
            .map(object_pairs)
            .unwrap_or_default();
        query.extend(
            QUERY_ASSIGN
                .captures_iter(body)
                .map(|c| (c[1].to_string(), c[2].to_string())),
        );
        for (key, expr) in query {
            if params
                .iter()
                .any(|p| p.place == Place::Query && p.name == key)
            {
                continue;
            }
            let Some(lead) = LEADING.captures(&expr) else {
                continue;
            };
            // `x`, `x.field`, or `x && x.join(",")` (a list sent as one value).
            let (head, field) = (
                lead[1].to_string(),
                lead.get(2).map(|f| f.as_str().to_string()),
            );
            let Some((pname, ty, required)) = arg(&head).cloned() else {
                continue;
            };
            let object = match &ty {
                Ty::Named(k) => match types.get(k) {
                    Some(TypeDef::Object { properties, .. }) => Some(properties.clone()),
                    _ => None,
                },
                _ => None,
            };
            match (field, object) {
                // `queryValues.baseVersion = baseVersionDescriptor.version`
                (Some(f), Some(props)) => {
                    if let Some((_, t, d)) = props.iter().find(|p| p.0 == f) {
                        params.push(Param {
                            name: key,
                            place: Place::Query,
                            ty: t.clone(),
                            required: false,
                            description: d.clone(),
                        });
                    }
                }
                // `searchCriteria: searchCriteria`, sent as searchCriteria.<property>
                (None, Some(props)) => {
                    for (prop, t, d) in props {
                        if matches!(t, Ty::Named(ref k) if matches!(types.get(k), Some(TypeDef::Object { .. })))
                        {
                            continue;
                        }
                        params.push(Param {
                            name: format!("{key}.{prop}"),
                            place: Place::Query,
                            ty: t,
                            required: false,
                            description: d,
                        });
                    }
                }
                _ => {
                    let ty = match ty {
                        Ty::Array(inner) => Ty::CommaList(inner),
                        t => t,
                    };
                    params.push(Param {
                        name: key,
                        place: Place::Query,
                        ty,
                        required,
                        description: describe(&pname),
                    });
                }
            }
        }
        let call_open = call.get(0).unwrap().end() - 1;
        let call_args = balanced(body, call_open).map(split_top).unwrap_or_default();
        let (http_method, body_arg, binary) = match (&call[1], &call[2]) {
            ("rest", "get") => ("get".to_string(), None, false),
            ("rest", "create") => (
                "post".to_string(),
                call_args.get(1).map(|s| s.to_string()),
                false,
            ),
            ("rest", "update") => (
                "patch".to_string(),
                call_args.get(1).map(|s| s.to_string()),
                false,
            ),
            ("rest", "replace") => (
                "put".to_string(),
                call_args.get(1).map(|s| s.to_string()),
                false,
            ),
            ("rest", "del") => ("delete".to_string(), None, false),
            ("rest", "options") => ("options".to_string(), None, false),
            ("rest", "uploadStream") => (
                call_args
                    .first()
                    .map(|s| s.trim_matches('"').to_lowercase())
                    .unwrap_or_else(|| "put".into()),
                call_args.get(2).map(|s| s.to_string()),
                false,
            ),
            ("http", verb) => (verb.to_string(), None, true),
            _ => continue,
        };
        let accept = REQUEST_OPTIONS
            .captures(body)
            .map(|c| c[1].to_string())
            .unwrap_or_else(|| "application/json".into());
        let mut content_type = None;
        if let Some(b) = body_arg.filter(|b| b != "null" && b != "undefined") {
            if let Some((n, ty, required)) = arg(&b).cloned() {
                content_type = Some(match CONTENT_TYPE.captures(body) {
                    Some(c) => c[1].to_string(),
                    None if ty == Ty::Stream => "application/octet-stream".to_string(),
                    None => "application/json".to_string(),
                });
                params.push(Param {
                    name: "body".into(),
                    place: Place::Body,
                    ty,
                    required,
                    description: describe(&n),
                });
            }
        }
        methods.push(Method {
            sdk: Sdk::Node,
            client: module.to_string(),
            name: upper_first(name),
            description: doc_text(&doc),
            guid: v[2].to_lowercase(),
            http_method,
            version: v[1].to_string(),
            params,
            route_constants,
            content_type,
            accept,
            binary_answer: binary,
        });
    }
}

/// Every method and named type of the Node library. `files` are (path in the repository, text).
pub fn parse(files: &[(String, String)]) -> Parsed {
    let mut parsed = Parsed::default();
    let module_of = |path: &str| {
        path.rsplit('/')
            .next()
            .unwrap_or("")
            .trim_end_matches(".ts")
            .to_string()
    };
    let mut raw = HashMap::new();
    for (path, text) in files.iter().filter(|(p, _)| p.contains("/interfaces/")) {
        parse_interfaces(&module_of(path), text, &mut raw, &mut parsed);
    }
    // Inherited properties first, then the interface's own (which win).
    fn collect(key: &str, raw: &RawInterfaces, depth: usize) -> Vec<(String, Ty, String)> {
        let Some((_, parents, own)) = raw.get(key) else {
            return Vec::new();
        };
        let mut out: Vec<(String, Ty, String)> = Vec::new();
        if depth < 8 {
            for p in parents {
                if let Ty::Named(pk) = p {
                    for prop in collect(pk, raw, depth + 1) {
                        out.retain(|o| o.0 != prop.0);
                        out.push(prop);
                    }
                }
            }
        }
        for prop in own {
            out.retain(|o| o.0 != prop.0);
            out.push(prop.clone());
        }
        out
    }
    for key in raw.keys() {
        let description = raw[key].0.clone();
        let properties = collect(key, &raw, 0);
        parsed.types.insert(
            key.clone(),
            TypeDef::Object {
                description,
                properties,
            },
        );
    }
    for (path, text) in files.iter().filter(|(p, _)| !p.contains("/interfaces/")) {
        parse_api(&module_of(path), text, &parsed.types, &mut parsed.methods);
    }
    parsed
}
