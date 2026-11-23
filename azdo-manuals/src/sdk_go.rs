//! Parse the Go client library (`azuredevops/v7/<package>/client.go` and `models.go`).
//!
//! The files are generated and regular: each method builds `routeValues["name"]`, `queryParams.Add("name", ...)`,
//! an optional `json.Marshal(...)` body and `additionalHeaders`, then calls `client.Client.Send(ctx,
//! http.Method<X>, locationId, "<version>", ..., "<content type>", "<accept>", ...)` with `locationId` parsed from
//! the route's GUID. Arguments are fields of a `<Method>Args` struct with `// (required)` or `// (optional)` comments.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

use crate::sdk::{
    balanced, comment_above, split_top, Method, Param, Parsed, Place, Sdk, Ty, TypeDef,
};

/// A struct field: (Go name, JSON name, type, description).
type Field = (String, String, Ty, String);

static TYPE_DECL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^type (\w+) (struct \{|string$)").unwrap());
static FIELD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\t(\w+)\s+(\S+)\s+`json:"([^",]*)[^"]*"`"#).unwrap());
static ARGS_FIELD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\t(\w+)\s+(\S+)$").unwrap());
static VALUE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"^\t\w+:\s*"([^"]*)",?$"#).unwrap());
static FUNC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^func \(client \*ClientImpl\) (\w+)\(ctx context\.Context, args (\w+)\)")
        .unwrap()
});
static ROUTE_VALUE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"routeValues\["([^"]+)"\] = (.+)"#).unwrap());
static QUERY_ADD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"queryParams\.Add\("([^"]+)", (.+)\)$"#).unwrap());
static HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"additionalHeaders\["([^"]+)"\] = (.+)"#).unwrap());
static ARG_REF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"args\.(\w+)(?:\.(\w+))?").unwrap());
static GUID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"uuid\.Parse\("([0-9a-fA-F-]{36})"\)"#).unwrap());
static MARSHAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"json\.Marshal\(\*?args\.(\w+)\)").unwrap());
static LIST_SOURCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:range \*args\.(\w+)|\(\*args\.(\w+)\)\[:\])").unwrap());

fn strip_preview(desc: &str) -> String {
    desc.trim_start_matches("[Preview API]").trim().to_string()
}

/// A Go type expression to a `Ty`. `pkg` qualifies unqualified names.
pub fn go_type(expr: &str, pkg: &str) -> Ty {
    let e = expr.trim().trim_start_matches('*');
    if let Some(inner) = e.strip_prefix("[]") {
        if inner == "byte" {
            return Ty::String;
        }
        return Ty::Array(Box::new(go_type(inner, pkg)));
    }
    if let Some(rest) = e.strip_prefix("map[") {
        let value = rest.split_once(']').map(|(_, v)| v).unwrap_or("");
        return Ty::Map(Box::new(go_type(value, pkg)));
    }
    match e {
        "string" => Ty::String,
        "int" | "int32" | "int16" | "uint16" | "byte" => Ty::Integer("int32"),
        "int64" | "uint64" | "uint32" | "uint" => Ty::Integer("int64"),
        "float32" | "float64" => Ty::Number,
        "bool" => Ty::Boolean,
        "uuid.UUID" => Ty::Uuid,
        "azuredevops.Time" | "time.Time" => Ty::DateTime,
        "interface{}" | "json.RawMessage" | "any" => Ty::Any,
        "io.Reader" | "io.ReadCloser" => Ty::Stream,
        _ => match e.split_once('.') {
            Some((p, name)) => Ty::Named(format!("go:{p}.{name}")),
            None => Ty::Named(format!("go:{pkg}.{e}")),
        },
    }
}

/// Struct fields and enum values of one `models.go`.
fn parse_models(
    pkg: &str,
    text: &str,
    structs: &mut HashMap<String, (String, Vec<Field>)>,
    parsed: &mut Parsed,
) {
    let lines: Vec<&str> = text.lines().collect();
    let line_of = |offset: usize| text[..offset].matches('\n').count();
    for cap in TYPE_DECL.captures_iter(text) {
        let name = &cap[1];
        let start = line_of(cap.get(0).unwrap().start());
        let description = comment_above(&lines, start);
        let key = format!("go:{pkg}.{name}");
        if &cap[2] == "string" {
            // var <Name>Values = <name>ValuesType{ Key: "value", ... }
            let marker = format!("var {name}Values = ");
            let values = match text.find(&marker) {
                Some(at) => text[at..]
                    .lines()
                    .skip(1)
                    .take_while(|l| !l.starts_with('}'))
                    .filter_map(|l| VALUE.captures(l).map(|c| c[1].to_string()))
                    .collect(),
                None => Vec::new(),
            };
            parsed.types.insert(
                key,
                TypeDef::Enum {
                    description,
                    values,
                },
            );
            continue;
        }
        let mut fields = Vec::new();
        for i in start + 1..lines.len() {
            let line = lines[i];
            if line.starts_with('}') {
                break;
            }
            if let Some(c) = FIELD.captures(line) {
                let json_name = if c[3].is_empty() {
                    c[1].to_string()
                } else {
                    c[3].to_string()
                };
                if json_name == "-" {
                    continue;
                }
                fields.push((
                    c[1].to_string(),
                    json_name,
                    go_type(&c[2], pkg),
                    comment_above(&lines, i),
                ));
            }
        }
        structs.insert(key, (description, fields));
    }
}

/// The `<Method>Args` structs of one `client.go`: name -> fields (Go name, type, required, description).
fn parse_args(pkg: &str, text: &str) -> HashMap<String, Vec<(String, Ty, bool, String)>> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = HashMap::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(name) = line
            .strip_prefix("type ")
            .and_then(|l| l.strip_suffix(" struct {"))
            .filter(|n| n.ends_with("Args"))
        else {
            continue;
        };
        let mut fields = Vec::new();
        for (j, field) in lines.iter().enumerate().skip(i + 1) {
            if field.starts_with('}') {
                break;
            }
            if let Some(c) = ARGS_FIELD.captures(field) {
                let comment = comment_above(&lines, j);
                let required = comment.starts_with("(required)");
                let description = comment
                    .trim_start_matches("(required)")
                    .trim_start_matches("(optional)")
                    .trim()
                    .to_string();
                fields.push((c[1].to_string(), go_type(&c[2], pkg), required, description));
            }
        }
        out.insert(name.to_string(), fields);
    }
    out
}

fn parse_client(
    pkg: &str,
    text: &str,
    structs: &HashMap<String, (String, Vec<Field>)>,
    methods: &mut Vec<Method>,
) {
    let args_structs = parse_args(pkg, text);
    let lines: Vec<&str> = text.lines().collect();
    let starts: Vec<(usize, String, String)> = FUNC
        .captures_iter(text)
        .map(|c| {
            (
                c.get(0).unwrap().start(),
                c[1].to_string(),
                c[2].to_string(),
            )
        })
        .collect();
    for (n, (start, name, args_name)) in starts.iter().enumerate() {
        let end = starts.get(n + 1).map_or(text.len(), |s| s.0);
        let body = &text[*start..end];
        let Some(guid) = GUID.captures(body).map(|c| c[1].to_lowercase()) else {
            continue;
        };
        let Some(send_at) = body.find("client.Client.Send(") else {
            continue;
        };
        let Some(send) = balanced(body, send_at + "client.Client.Send".len()) else {
            continue;
        };
        let send_args = split_top(send);
        if send_args.len() < 9 {
            continue;
        }
        let unquote = |s: &str| s.trim().trim_matches('"').to_string();
        let http_method = send_args[1]
            .trim()
            .trim_start_matches("http.Method")
            .to_lowercase();
        let line_no = text[..*start].matches('\n').count();
        let fields = args_structs.get(args_name).cloned().unwrap_or_default();
        let field = |go_name: &str| fields.iter().find(|f| f.0 == go_name);
        // The type and description of `args.A` or `args.A.B`.
        let resolve = |a: &str, b: Option<&str>| -> Option<(Ty, bool, String)> {
            let (_, ty, required, desc) = field(a)?;
            let Some(b) = b else {
                return Some((ty.clone(), *required, desc.clone()));
            };
            let Ty::Named(key) = ty else { return None };
            let (_, sub) = structs.get(key)?;
            let (_, _, t, d) = sub.iter().find(|f| f.0 == b)?;
            Some((t.clone(), false, d.clone()))
        };
        let mut params: Vec<Param> = Vec::new();
        let mut route_constants: Vec<(String, String)> = Vec::new();
        let mut seen_lines: Vec<&str> = Vec::new();
        for line in body.lines().map(str::trim) {
            seen_lines.push(line);
            if let Some(c) = ROUTE_VALUE.captures(line) {
                if let Some(constant) = c[2].strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
                    route_constants.push((c[1].to_string(), constant.to_string()));
                    continue;
                }
                let Some(r) = ARG_REF.captures(&c[2]) else {
                    continue;
                };
                let (ty, required, description) = resolve(&r[1], r.get(2).map(|m| m.as_str()))
                    .unwrap_or((Ty::String, true, String::new()));
                params.push(Param {
                    name: c[1].to_string(),
                    place: Place::Path,
                    ty,
                    required,
                    description,
                });
            } else if let Some(c) = QUERY_ADD.captures(line) {
                let expr = &c[2];
                let (ty, required, description) = if expr.contains("listAsString") {
                    // Built a few lines above from `(*args.X)[:]` or `range *args.X`.
                    let source = seen_lines.iter().rev().find_map(|l| {
                        LIST_SOURCE
                            .captures(l)
                            .and_then(|m| m.get(1).or(m.get(2)).map(|g| g.as_str().to_string()))
                    });
                    match source.and_then(|s| resolve(&s, None)) {
                        Some((Ty::Array(inner), req, d)) => (Ty::CommaList(inner), req, d),
                        Some((t, req, d)) => (Ty::CommaList(Box::new(t)), req, d),
                        None => (Ty::CommaList(Box::new(Ty::String)), false, String::new()),
                    }
                } else {
                    match ARG_REF.captures(expr) {
                        Some(r) => resolve(&r[1], r.get(2).map(|m| m.as_str())).unwrap_or((
                            Ty::String,
                            false,
                            String::new(),
                        )),
                        None => continue,
                    }
                };
                if params
                    .iter()
                    .any(|p| p.place == Place::Query && p.name == c[1])
                {
                    continue;
                }
                params.push(Param {
                    name: c[1].to_string(),
                    place: Place::Query,
                    ty,
                    required,
                    description,
                });
            } else if let Some(c) = HEADER.captures(line) {
                let Some(r) = ARG_REF.captures(&c[2]) else {
                    continue;
                };
                let (ty, required, description) =
                    resolve(&r[1], None).unwrap_or((Ty::String, false, String::new()));
                params.push(Param {
                    name: c[1].to_string(),
                    place: Place::Header,
                    ty,
                    required,
                    description,
                });
            }
        }
        let body_arg = send_args[6].trim();
        let content_type = unquote(send_args[7]);
        let mut content = None;
        if body_arg != "nil" {
            let marshalled = MARSHAL.captures(body).map(|c| c[1].to_string());
            let source = match ARG_REF.captures(body_arg) {
                Some(r) => Some(r[1].to_string()),
                None => marshalled,
            };
            if let Some((ty, required, description)) =
                source.as_deref().and_then(|s| resolve(s, None))
            {
                params.push(Param {
                    name: "body".into(),
                    place: Place::Body,
                    ty,
                    required,
                    description,
                });
                content = Some(if content_type.is_empty() {
                    "application/json".to_string()
                } else {
                    content_type
                });
            }
        }
        methods.push(Method {
            sdk: Sdk::Go,
            client: pkg.to_string(),
            name: name.clone(),
            description: strip_preview(&comment_above(&lines, line_no)),
            guid,
            http_method,
            version: unquote(send_args[3]),
            params,
            route_constants,
            content_type: content,
            accept: unquote(send_args[8]),
            binary_answer: body.contains("return resp.Body, err"),
        });
    }
}

/// Every method and named type of the Go library. `files` are (path in the repository, text).
pub fn parse(files: &[(String, String)]) -> Parsed {
    let mut parsed = Parsed::default();
    let mut structs: HashMap<String, (String, Vec<Field>)> = HashMap::new();
    let pkg_of = |path: &str| path.split('/').nth(2).unwrap_or("").to_string();
    for (path, text) in files.iter().filter(|(p, _)| p.ends_with("/models.go")) {
        parse_models(&pkg_of(path), text, &mut structs, &mut parsed);
    }
    for (path, text) in files.iter().filter(|(p, _)| p.ends_with("/client.go")) {
        parse_client(&pkg_of(path), text, &structs, &mut parsed.methods);
    }
    for (key, (description, fields)) in structs {
        let properties = fields
            .into_iter()
            .map(|(_, json_name, ty, desc)| (json_name, ty, desc))
            .collect();
        parsed.types.insert(
            key,
            TypeDef::Object {
                description,
                properties,
            },
        );
    }
    parsed
}
