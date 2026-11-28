//! Join the client libraries to the live route list and turn the endpoints the spec lacks into spec operations.
//!
//! 1. Every spec operation is mapped to the route GUIDs it calls: by the spec's area and resource
//!    (`x-ms-vss-area`, `x-ms-vss-resource`) when they name a route, narrowed or else found by matching its path to
//!    the route templates (the rule of `check-routes`, most specific route wins). An endpoint and HTTP method the spec
//!    has is never generated again from a library.
//! 2. For every other live (GUID, HTTP method) that a library has, one library method is chosen: Go before Node (Go
//!    targets API 7.1), the JSON answer before file variants of the same endpoint.
//! 3. Its area comes from `route_areas`, else from the spec operations of the same route area, else it is left out
//!    with the reason. Its host is the one `resourceAreas` gives the area, else `dev.azure.com`, else the only host
//!    that serves it. Its api-version follows the route (see `api_version_for`).
//! 4. It becomes a Swagger 2.0 operation in a one-operation spec (`x-azdo-manuals` says where it came from), so
//!    selection, names, URLs, bodies, the report and the live checks treat it like any spec operation.
//!
//! The libraries carry no OAuth scopes. A generated operation takes the scopes of the spec's operations on the same
//! endpoint that read or write as it does, else the most common scopes of such operations in its area, else none
//! (reported under `tools_without_scope`). `read-test` with a PAT limited to the report's scopes checks them.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::config::Config;
use crate::generate::Specs;
use crate::live::{Live, Route, MAIN_HOST};
use crate::routes::{path_segments, route_segments, segments_match, version_tuple};
use crate::sdk::{Method, Param, Parsed, Place, Sdk, Ty, TypeDef, Types};

const HTTP_METHODS: [&str; 6] = ["get", "put", "post", "delete", "head", "patch"];

/// One form of an endpoint: (GUID, HTTP method, number of path parameters).
type Form = (String, String, usize);

/// What the spec already covers, by route GUID.
#[derive(Debug, Default)]
pub struct Coverage {
    /// (GUID, HTTP method, path parameters) of every spec operation. The parameter count tells the forms of one
    /// endpoint apart (`.../alerts` and `.../alerts/{alertId}` share a GUID).
    pub pairs: HashSet<Form>,
    /// GUID -> the spec operations on it: (area, operationId, HTTP method, reads, scopes).
    pub ops: HashMap<String, Vec<SpecUse>>,
    /// Route area (lower case) -> spec area -> number of operations.
    pub area_votes: HashMap<String, HashMap<String, usize>>,
}

#[derive(Clone, Debug)]
pub struct SpecUse {
    pub area: String,
    pub operation_id: String,
    pub method: String,
    /// The client method the spec names (`x-ms-vss-method`), lower case.
    pub client_method: String,
    pub read: bool,
    pub scopes: Vec<String>,
}

fn literal_count(route: &Route) -> usize {
    route_segments(&route.raw)
        .iter()
        .filter(|s| !s.ends_with('*'))
        .count()
}

/// Map every spec operation to the route GUIDs it calls.
pub fn coverage(specs: &Specs, live: &Live, conf: &Config) -> Coverage {
    let mut by_area_resource: HashMap<(String, String), Vec<&Route>> = HashMap::new();
    for r in &live.routes {
        by_area_resource
            .entry((r.area.to_lowercase(), r.resource.to_lowercase()))
            .or_default()
            .push(r);
    }
    let segments: Vec<(Vec<String>, &Route)> = live
        .routes
        .iter()
        .map(|r| (route_segments(&r.raw), r))
        .collect();
    let mut cov = Coverage::default();
    for (area, files) in specs {
        for spec in files {
            let spec_area = spec["x-ms-vss-area"].as_str().unwrap_or("").to_lowercase();
            for key in ["paths", "x-ms-paths"] {
                for (path, item) in spec[key].as_object().into_iter().flatten() {
                    for (method, op) in item.as_object().into_iter().flatten() {
                        if !HTTP_METHODS.contains(&method.as_str()) {
                            continue;
                        }
                        let tool_segs = path_segments(path);
                        let resource = op["x-ms-vss-resource"]
                            .as_str()
                            .unwrap_or("")
                            .to_lowercase();
                        let mut cands: Vec<&Route> = by_area_resource
                            .get(&(spec_area.clone(), resource))
                            .cloned()
                            .unwrap_or_default();
                        let guids = |c: &[&Route]| {
                            c.iter().map(|r| r.guid.clone()).collect::<BTreeSet<_>>()
                        };
                        if guids(&cands).len() > 1 {
                            let narrowed: Vec<&Route> = cands
                                .iter()
                                .copied()
                                .filter(|r| segments_match(&route_segments(&r.raw), &tool_segs))
                                .collect();
                            if !narrowed.is_empty() {
                                cands = narrowed;
                            }
                        }
                        if cands.is_empty() {
                            cands = segments
                                .iter()
                                .filter(|(s, _)| segments_match(s, &tool_segs))
                                .map(|(_, r)| *r)
                                .collect();
                        }
                        if guids(&cands).len() > 1 {
                            let best = cands.iter().map(|r| literal_count(r)).max().unwrap_or(0);
                            cands.retain(|r| literal_count(r) == best);
                        }
                        let op_id = op["operationId"].as_str().unwrap_or("").to_string();
                        let read = method == "get"
                            || conf
                                .post_reads
                                .get(area)
                                .is_some_and(|l| l.contains(&op_id));
                        let scopes = crate::generate::scopes_of(spec, op);
                        let shape =
                            path.matches('{').count() - path.matches("{organization}").count();
                        for guid in guids(&cands) {
                            cov.pairs.insert((guid.clone(), method.clone(), shape));
                            cov.ops.entry(guid).or_default().push(SpecUse {
                                area: area.clone(),
                                operation_id: op_id.clone(),
                                method: method.clone(),
                                client_method: op["x-ms-vss-method"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_lowercase(),
                                read,
                                scopes: scopes.clone(),
                            });
                        }
                        for route_area in cands
                            .iter()
                            .map(|r| r.area.to_lowercase())
                            .collect::<BTreeSet<_>>()
                        {
                            *cov.area_votes
                                .entry(route_area)
                                .or_default()
                                .entry(area.clone())
                                .or_default() += 1;
                        }
                    }
                }
            }
        }
    }
    cov
}

/// The api-version for an endpoint, from its route. The target is the generator's version (7.1), or the route's
/// `maxVersion` when that is lower. A route released at or after the target is called at the target; any other is
/// called as `<target>-preview.<n>`, where n is the library's preview number capped at the route's
/// `resourceVersion` (the library's parameters match that resource version).
pub fn api_version_for(route: &Route, library_version: &str, base: &str) -> String {
    let max = version_tuple(&route.max_version);
    let target = if max >= version_tuple(base) || route.max_version.is_empty() {
        base.to_string()
    } else {
        route.max_version.clone()
    };
    let released = version_tuple(&route.released_version);
    if released.iter().any(|&n| n > 0) && released >= version_tuple(&target) {
        return target;
    }
    let library_preview = library_version
        .split_once("-preview.")
        .and_then(|(_, n)| n.parse::<u64>().ok())
        .unwrap_or(route.resource_version);
    let n = library_preview.min(route.resource_version).max(1);
    format!("{target}-preview.{n}")
}

/// The area a route area's operations go into, or why they are left out.
fn area_for(route_area: &str, conf: &Config, cov: &Coverage) -> Result<String, String> {
    let ra = route_area.to_lowercase();
    let area = if let Some((_, a)) = conf.route_areas.iter().find(|(r, _)| *r == ra) {
        a.clone()
    } else if let Some((_, reason)) = conf.excluded_route_areas.iter().find(|(r, _)| *r == ra) {
        return Err(reason.clone());
    } else if let Some(votes) = cov.area_votes.get(&ra) {
        let mut v: Vec<(&String, &usize)> = votes.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        v[0].0.clone()
    } else {
        return Err(format!("route area {route_area} is in no manual yet"));
    };
    if let Some((_, reason)) = conf.excluded_areas.iter().find(|(a, _)| *a == area) {
        return Err(reason.clone());
    }
    if conf.manual_of(&area).is_none() {
        return Err(format!("area {area} is in no manual"));
    }
    Ok(area)
}

fn rank(m: &Method) -> (u8, bool, bool, bool) {
    let variant = ["Zip", "Text", "Content"]
        .iter()
        .any(|s| m.name.ends_with(s));
    (
        match m.sdk {
            Sdk::Go => 0,
            Sdk::Node => 1,
        },
        m.binary_answer,
        !m.accept.starts_with("application/json"),
        variant,
    )
}

/// A Swagger parameter for a path, query or header parameter.
fn swagger_param(p: &Param, types: &Types) -> Value {
    let place = match p.place {
        Place::Path => "path",
        Place::Query => "query",
        Place::Header => "header",
        Place::Body => "body",
    };
    let mut out = Map::new();
    out.insert("name".into(), json!(p.name));
    out.insert("in".into(), json!(place));
    out.insert(
        "required".into(),
        json!(p.required || p.place == Place::Path),
    );
    if p.place == Place::Path && !p.required {
        out.insert("x-ms-required".into(), json!(false));
    }
    if !p.description.is_empty() {
        out.insert("description".into(), json!(p.description));
    }
    let scalar = |t: &Ty| -> (&'static str, Option<&'static str>) {
        match t {
            Ty::Integer(f) => ("integer", Some(*f)),
            Ty::Number => ("number", None),
            Ty::Boolean => ("boolean", None),
            Ty::Uuid => ("string", Some("uuid")),
            Ty::DateTime => ("string", Some("date-time")),
            _ => ("string", None),
        }
    };
    let enum_of = |t: &Ty| match t {
        Ty::Named(k) => match types.get(k) {
            Some(TypeDef::Enum { values, .. }) if !values.is_empty() => Some(values.clone()),
            _ => None,
        },
        _ => None,
    };
    match &p.ty {
        Ty::CommaList(inner) | Ty::Array(inner) => {
            let (t, _) = scalar(inner);
            out.insert("type".into(), json!("string"));
            out.insert("format".into(), json!(format!("array ({t})")));
        }
        t => {
            let (kind, format) = scalar(t);
            out.insert("type".into(), json!(kind));
            if let Some(f) = format {
                out.insert("format".into(), json!(f));
            }
            if let Some(values) = enum_of(t) {
                out.insert("enum".into(), json!(values));
            }
        }
    }
    Value::Object(out)
}

/// A JSON schema for a type, with library types as `$ref`s into `definitions`; records the types used.
fn schema_of(t: &Ty, types: &Types, used: &mut BTreeSet<String>) -> Value {
    match t {
        Ty::String => json!({"type": "string"}),
        Ty::Integer(f) => json!({"type": "integer", "format": f}),
        Ty::Number => json!({"type": "number"}),
        Ty::Boolean => json!({"type": "boolean"}),
        Ty::Uuid => json!({"type": "string", "format": "uuid"}),
        Ty::DateTime => json!({"type": "string", "format": "date-time"}),
        Ty::Any => json!({}),
        Ty::Stream => json!({"type": "string", "format": "binary"}),
        Ty::Array(inner) | Ty::CommaList(inner) => {
            json!({"type": "array", "items": schema_of(inner, types, used)})
        }
        Ty::Map(inner) => {
            json!({"type": "object", "additionalProperties": schema_of(inner, types, used)})
        }
        Ty::Named(k) if types.contains_key(k) => {
            used.insert(k.clone());
            json!({"$ref": format!("#/definitions/{k}")})
        }
        Ty::Named(_) => json!({"type": "object"}),
    }
}

fn definition(def: &TypeDef, types: &Types, used: &mut BTreeSet<String>) -> Value {
    match def {
        TypeDef::Enum {
            description,
            values,
        } => {
            let mut d = json!({"type": "string", "enum": values});
            if !description.is_empty() {
                d["description"] = json!(description);
            }
            d
        }
        TypeDef::Object {
            description,
            properties,
        } => {
            let mut props = Map::new();
            for (name, t, desc) in properties {
                let mut s = schema_of(t, types, used);
                if !desc.is_empty() && s.get("$ref").is_none() {
                    s["description"] = json!(desc);
                }
                props.insert(name.clone(), s);
            }
            let mut d = json!({"type": "object", "properties": props});
            if !description.is_empty() {
                d["description"] = json!(description);
            }
            d
        }
    }
}

/// The definitions a body needs, down to `depth` levels of library types.
fn definitions_for(root: &BTreeSet<String>, types: &Types, depth: usize) -> Map<String, Value> {
    let mut out = Map::new();
    let mut level: BTreeSet<String> = root.clone();
    for _ in 0..=depth {
        let mut next = BTreeSet::new();
        for k in &level {
            if out.contains_key(k) {
                continue;
            }
            if let Some(def) = types.get(k) {
                out.insert(k.clone(), definition(def, types, &mut next));
            }
        }
        level = next;
    }
    // Types referenced at the last level get a plain stub, so every $ref resolves.
    for k in level {
        if !out.contains_key(&k) {
            let description = match types.get(&k) {
                Some(TypeDef::Object { description, .. } | TypeDef::Enum { description, .. }) => {
                    description.clone()
                }
                None => String::new(),
            };
            out.insert(k, json!({"type": "object", "description": description}));
        }
    }
    out
}

/// The path of an endpoint: the route template with `{area}` and `{resource}` filled in, each route parameter the
/// method supplies named as the method names it, fixed route values written in, and segments of parameters it does
/// not supply left out. Returns the path and the parameters it uses.
fn path_for(route: &Route, method: &Method) -> (String, Vec<String>) {
    static PARAM: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\{\*?([A-Za-z0-9_]+)[^}]*\}").unwrap());
    let mut segs = vec!["{organization}".to_string()];
    let mut used = Vec::new();
    for seg in route.template.split('/').filter(|s| !s.is_empty()) {
        let seg = seg
            .replace("{area}", &route.area)
            .replace("{resource}", &route.resource);
        let mut out = String::new();
        let mut last = 0;
        let mut complete = true;
        let mut names = Vec::new();
        for cap in PARAM.captures_iter(&seg) {
            let whole = cap.get(0).unwrap();
            out.push_str(&seg[last..whole.start()]);
            last = whole.end();
            let name = &cap[1];
            if let Some((_, value)) = method
                .route_constants
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
            {
                out.push_str(value);
            } else if let Some(p) = method
                .params
                .iter()
                .find(|p| p.place == Place::Path && p.name.eq_ignore_ascii_case(name))
            {
                out.push_str(&format!("{{{}}}", p.name));
                names.push(p.name.clone());
            } else {
                complete = false;
            }
        }
        out.push_str(&seg[last..]);
        // A segment with a parameter the method does not supply is left out, as the client libraries do.
        if complete {
            segs.push(out);
            used.extend(names);
        }
    }
    (format!("/{}", segs.join("/")), used)
}

/// Scopes for a library operation: (scopes, how they were found).
fn infer_scopes(guid: &str, read: bool, area: &str, cov: &Coverage) -> (Vec<String>, &'static str) {
    let most_common = |uses: Vec<&SpecUse>| -> Option<Vec<String>> {
        let mut counts: HashMap<&Vec<String>, usize> = HashMap::new();
        for u in uses.iter().filter(|u| !u.scopes.is_empty()) {
            *counts.entry(&u.scopes).or_default() += 1;
        }
        let mut v: Vec<(&Vec<String>, usize)> = counts.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        v.first().map(|(s, _)| (*s).clone())
    };
    let same_endpoint: Vec<&SpecUse> = cov
        .ops
        .get(guid)
        .into_iter()
        .flatten()
        .filter(|u| u.read == read)
        .collect();
    if let Some(s) = most_common(same_endpoint) {
        return (s, "same endpoint");
    }
    let same_area: Vec<&SpecUse> = cov
        .ops
        .values()
        .flatten()
        .filter(|u| u.area == area && u.read == read)
        .collect();
    if let Some(s) = most_common(same_area) {
        return (s, "same area");
    }
    (Vec::new(), "none")
}

/// The result of the join.
#[derive(Debug, Default)]
pub struct Joined {
    /// Area -> one-operation specs, to add to the spec input.
    pub specs: Specs,
    /// Library operations left out before generation (their area is excluded or unknown), as report entries.
    pub left_out: Vec<Value>,
    /// Library methods whose endpoint the organization does not serve, as `<sdk> <client>.<Method>`.
    pub not_on_organization: Vec<String>,
    /// GUID -> the libraries that have a method for it.
    pub libraries: HashMap<String, BTreeSet<Sdk>>,
}

/// Join both libraries to the live routes and build a spec operation for every live endpoint the spec lacks.
pub fn join(
    specs: &Specs,
    libraries: &[&Parsed],
    live: &Live,
    conf: &Config,
    api_version: &str,
) -> (Joined, Coverage) {
    let cov = coverage(specs, live, conf);
    let by_guid = live.by_guid();
    let mut joined = Joined::default();
    let mut candidates: HashMap<Form, Vec<(&Method, &Types)>> = HashMap::new();
    for lib in libraries {
        for m in &lib.methods {
            joined
                .libraries
                .entry(m.guid.clone())
                .or_default()
                .insert(m.sdk);
            let Some(routes) = by_guid.get(m.guid.as_str()) else {
                joined
                    .not_on_organization
                    .push(format!("{} {}.{}", m.sdk.key(), m.client, m.name));
                continue;
            };
            let shape = path_for(routes[0], m).1.len();
            let key = (m.guid.clone(), m.http_method.clone(), shape);
            // The same client method as a spec operation on this endpoint is that operation, whatever its form.
            let same_method = cov.ops.get(&m.guid).is_some_and(|uses| {
                uses.iter()
                    .any(|u| u.method == m.http_method && u.client_method == m.name.to_lowercase())
            });
            if cov.pairs.contains(&key) || same_method {
                continue;
            }
            candidates.entry(key).or_default().push((m, &lib.types));
        }
    }
    joined.not_on_organization.sort();
    let mut keys: Vec<&Form> = candidates.keys().collect();
    keys.sort();
    for key in keys {
        let mut cands = candidates[key].clone();
        cands.sort_by_key(|(m, _)| (rank(m), m.name.clone()));
        let (method, types) = cands[0];
        let routes = &by_guid[key.0.as_str()];
        let route_area = routes[0].area.clone();
        let operation_id = format!("{}.{}", routes[0].resource, method.name);
        let too_new = version_tuple(&routes[0].min_version) > version_tuple(api_version);
        let area = match area_for(&route_area, conf, &cov) {
            Ok(_) if too_new => Err(format!(
                "starts at API {}; these manuals are for {api_version}",
                routes[0].min_version
            )),
            other => other,
        };
        let area = match area {
            Ok(a) => a,
            Err(reason) => {
                joined.left_out.push(json!({
                    "manual": Value::Null,
                    "area": route_area.to_lowercase(),
                    "method": method.http_method.to_uppercase(),
                    "operation_id": operation_id,
                    "guid": key.0,
                    "reason": reason,
                }));
                continue;
            }
        };
        let preferred = live.area_hosts.get(&route_area.to_lowercase());
        let route = routes
            .iter()
            .find(|r| Some(&r.host) == preferred)
            .or_else(|| routes.iter().find(|r| r.host == MAIN_HOST))
            .unwrap_or(&routes[0]);
        let spec = synthesize(
            method,
            types,
            route,
            &area,
            &operation_id,
            &cov,
            api_version,
        );
        joined.specs.entry(area).or_default().push(spec);
    }
    (joined, cov)
}

/// A one-operation Swagger 2.0 spec for a library method on a route.
pub fn synthesize(
    method: &Method,
    types: &Types,
    route: &Route,
    area: &str,
    operation_id: &str,
    cov: &Coverage,
    api_version: &str,
) -> Value {
    let (path, used) = path_for(route, method);
    let mut defs_used = BTreeSet::new();
    let mut parameters = Vec::new();
    let mut names: HashSet<String> = HashSet::new();
    for p in &method.params {
        let fresh = names.insert(format!("{:?}:{}", p.place, p.name));
        match p.place {
            Place::Path if !used.contains(&p.name) => continue,
            _ if !fresh => continue,
            Place::Body => {
                let mut body = json!({"name": "body", "in": "body", "required": p.required,
                                      "schema": schema_of(&p.ty, types, &mut defs_used)});
                if !p.description.is_empty() {
                    body["description"] = json!(p.description);
                }
                parameters.push(body);
            }
            _ => parameters.push(swagger_param(p, types)),
        }
    }
    let read = method.http_method == "get";
    let (scopes, how) = infer_scopes(&route.guid, read, area, cov);
    let mut op = json!({
        "operationId": operation_id,
        // A method without a doc comment gets a sentence made from its name later, as a spec operation without a
        // description does (`generate::tool_description`).
        "description": method.description,
        "tags": [route.resource],
        "x-ms-vss-method": method.name,
        "parameters": parameters,
        "produces": [method.accept],
        "security": [{"oauth2": scopes}],
        "x-azdo-manuals": {
            "source": method.sdk.key(),
            "client": method.client,
            "guid": route.guid,
            "api-version": api_version_for(route, &method.version, api_version),
            "scopes": how,
        },
    });
    if let Some(ct) = &method.content_type {
        op["consumes"] = json!([ct]);
    }
    let mut item = Map::new();
    item.insert(method.http_method.clone(), op);
    let mut paths = Map::new();
    paths.insert(path, Value::Object(item));
    json!({
        "swagger": "2.0",
        "info": {"title": format!("{} ({} client library)", route.area, method.sdk.key())},
        "host": route.host,
        "basePath": "/",
        "paths": paths,
        "definitions": definitions_for(&defs_used, types, 4),
    })
}
