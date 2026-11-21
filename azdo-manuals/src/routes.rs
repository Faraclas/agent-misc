//! Live route check: does every generated tool's URL exist on the real service, at a version it supports?
//!
//! Azure DevOps lists its routes at `OPTIONS https://<host>/<org>/_apis` (needs a PAT). Each route has a template
//! such as `{project}/_apis/{area}/{resource}/{id}`, plus `minVersion`, `maxVersion` and `releasedVersion`. Routes
//! carry no HTTP method, so this checks paths and versions only. Read-only; prints tool names and counts, never the
//! token.

use std::sync::LazyLock;

use base64::Engine;
use indexmap::IndexMap;
use regex::Regex;
use serde_json::Value;

use crate::http::{Request, Transport};
use crate::text::shown;

const USER_AGENT: &str = concat!("azdo-manuals/", env!("CARGO_PKG_VERSION"), " route-check");

pub fn auth_header(pat: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("pat:{pat}"))
    )
}

/// `base` is `https://<host>/<org>` (or just `https://<host>` for org-less hosts).
pub fn fetch_routes(
    transport: &mut Transport,
    base: &str,
    pat: &str,
) -> Result<Vec<Value>, String> {
    let url = format!("{}/_apis", base.trim_end_matches('/'));
    let req = Request {
        method: "OPTIONS",
        url,
        headers: vec![
            ("Authorization".into(), auth_header(pat)),
            ("Accept".into(), "application/json".into()),
            ("User-Agent".into(), USER_AGENT.into()),
        ],
        body: None,
    };
    let resp = transport(&req).map_err(|e| format!("OPTIONS {base}/_apis failed: {e}"))?;
    if !(200..300).contains(&resp.status) {
        return Err(format!("OPTIONS {base}/_apis failed: HTTP {}", resp.status));
    }
    let body: Value = serde_json::from_slice(&resp.body)
        .map_err(|e| format!("OPTIONS {base}/_apis: bad JSON ({e})"))?;
    let list = match body {
        Value::Object(mut o) => o.remove("value").unwrap_or(Value::Object(o)),
        other => other,
    };
    match list {
        Value::Array(routes) => Ok(routes),
        _ => Err(format!(
            "OPTIONS {base}/_apis: no route list in the response"
        )),
    }
}

fn version_tuple(v: &str) -> Vec<u64> {
    static DIGITS: LazyLock<Regex> = LazyLock::new(|| Regex::new("[0-9]+").unwrap());
    let head = v.split('-').next().unwrap_or("");
    let nums: Vec<u64> = DIGITS
        .find_iter(head)
        .map(|m| m.as_str().parse().unwrap_or(u64::MAX))
        .collect();
    if nums.is_empty() {
        vec![0]
    } else {
        nums
    }
}

/// Python's `urllib.parse.unquote`: decode `%XX` escapes as UTF-8, leaving malformed ones as they are.
fn unquote(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Literal segments compare case-insensitively; any segment with a parameter becomes a wildcard.
fn segment_token(seg: &str) -> String {
    let seg = unquote(seg);
    match seg.split_once('{') {
        Some((head, _)) => format!("{head}*"),
        None => seg.to_lowercase(),
    }
}

pub fn route_segments(route: &Value) -> Vec<String> {
    let field = |k: &str| route.get(k).and_then(Value::as_str).unwrap_or("");
    let template = field("routeTemplate")
        .replace("{area}", field("area"))
        .replace("{resource}", field("resourceName"));
    template
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .map(segment_token)
        .collect()
}

fn one(route_seg: &str, tool_seg: &str) -> bool {
    match route_seg.strip_suffix('*') {
        Some(prefix) => tool_seg.starts_with(prefix),
        None => route_seg == tool_seg,
    }
}

/// Route parameter segments match anything; trailing ones and leading ones (before `_apis`) may be absent.
pub fn segments_match(route_segs: &[String], tool_segs: &[String]) -> bool {
    let mut lead = 0;
    while lead < route_segs.len() && route_segs[lead].ends_with('*') && route_segs[lead] != "_apis"
    {
        lead += 1;
    }
    for skip in 0..=lead {
        let r = &route_segs[skip..];
        if tool_segs.len() > r.len() {
            continue;
        }
        if r[tool_segs.len()..].iter().any(|seg| !seg.ends_with('*')) {
            continue;
        }
        if r.iter().zip(tool_segs).all(|(rs, ts)| one(rs, ts)) {
            return true;
        }
    }
    false
}

/// (base for OPTIONS, path segments after the org, api-version).
fn split_tool_url(url: &str, org: &str) -> (String, Vec<String>, String) {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let rest = rest.split('#').next().unwrap_or("");
    let (before_query, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (netloc, path) = match before_query.find('/') {
        Some(i) => (&before_query[..i], &before_query[i..]),
        None => (before_query, ""),
    };
    let version = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, v)| unquote(&k.replace('+', " ")) == "api-version" && !v.is_empty())
        .map(|(_, v)| unquote(&v.replace('+', " ")))
        .unwrap_or_default();
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segs
        .first()
        .is_some_and(|s| s.to_lowercase() == org.to_lowercase())
    {
        let rest = segs[1..].iter().map(|s| segment_token(s)).collect();
        return (format!("{scheme}://{netloc}/{}", segs[0]), rest, version);
    }
    (
        format!("{scheme}://{netloc}"),
        segs.iter().map(|s| segment_token(s)).collect(),
        version,
    )
}

fn route_str(route: &Value, key: &str, default: &str) -> String {
    match route.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(v) if !v.is_null() => v.to_string(),
        _ => default.to_string(),
    }
}

/// Empty when `version` is valid for the route, else why not.
pub fn version_problem(version: &str, route: &Value) -> String {
    static PREVIEW: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"preview\.([0-9]+)").unwrap());
    let v = version_tuple(version);
    if v < version_tuple(&route_str(route, "minVersion", "0")) {
        return format!("below minVersion {}", shown(route.get("minVersion")));
    }
    if version.contains("-preview") {
        if v > version_tuple(&route_str(route, "maxVersion", "0")) {
            return format!("above maxVersion {}", shown(route.get("maxVersion")));
        }
        if let Some(n) = PREVIEW.captures(version) {
            let n: u64 = n[1].parse().unwrap_or(u64::MAX);
            let resource_version: u64 = route_str(route, "resourceVersion", "1")
                .parse()
                .unwrap_or(1);
            if n > resource_version {
                return format!(
                    "preview.{n} above resourceVersion {}",
                    shown(route.get("resourceVersion"))
                );
            }
        }
        return String::new();
    }
    if v > version_tuple(&route_str(route, "releasedVersion", "0")) {
        return format!(
            "not released at {version} (releasedVersion {}); needs -preview",
            shown(route.get("releasedVersion"))
        );
    }
    String::new()
}

#[derive(Debug, Default)]
pub struct CheckResult {
    pub matched: usize,
    pub unmatched: Vec<String>,
    pub version: Vec<String>,
    pub unchecked: Vec<String>,
    /// Base (org shown as `<org>`) -> number of routes, in the order first asked.
    pub bases: IndexMap<String, usize>,
    pub base_errors: IndexMap<String, String>,
}

/// manuals: name -> manual. `fetch(base)` returns that base's route list.
pub fn check(
    manuals: &IndexMap<String, Value>,
    org: &str,
    fetch: &mut dyn FnMut(&str) -> Result<Vec<Value>, String>,
) -> CheckResult {
    type Routes = Vec<(Vec<String>, Value)>;
    let mut routes_by_base: IndexMap<String, Option<Routes>> = IndexMap::new();
    let mut results = CheckResult::default();
    for (manual_name, manual) in manuals {
        for tool in manual["tools"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
            let name = format!("{manual_name}.{}", tool["name"].as_str().unwrap_or(""));
            let url = tool["tool_call_template"]["url"].as_str().unwrap_or("");
            let (base, segs, version) = split_tool_url(url, org);
            let shown_base = base.replace(org, "<org>");
            if !routes_by_base.contains_key(&base) {
                match fetch(&base) {
                    Ok(routes) => {
                        let parsed: Routes = routes
                            .into_iter()
                            .map(|r| (route_segments(&r), r))
                            .collect();
                        results.bases.insert(shown_base, parsed.len());
                        routes_by_base.insert(base.clone(), Some(parsed));
                    }
                    Err(e) => {
                        results
                            .base_errors
                            .insert(shown_base, e.replace(org, "<org>"));
                        routes_by_base.insert(base.clone(), None);
                    }
                }
            }
            let Some(routes) = &routes_by_base[&base] else {
                results.unchecked.push(name);
                continue;
            };
            let candidates: Vec<&Value> = routes
                .iter()
                .filter(|(r, _)| segments_match(r, &segs))
                .map(|(_, r)| r)
                .collect();
            if candidates.is_empty() {
                results.unmatched.push(name);
                continue;
            }
            let problems: Vec<String> = candidates
                .iter()
                .map(|r| version_problem(&version, r))
                .collect();
            if problems.iter().all(|p| !p.is_empty()) {
                results
                    .version
                    .push(format!("{name} ({version}): {}", problems[0]));
            } else {
                results.matched += 1;
            }
        }
    }
    results
}
