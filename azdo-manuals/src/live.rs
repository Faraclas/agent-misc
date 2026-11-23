//! The organization's live route list: every endpoint the service has, on every host, by location GUID.
//!
//! `GET https://dev.azure.com/<org>/_apis/resourceAreas` lists the service's areas with the host each lives on (the
//! client libraries find hosts the same way). `OPTIONS https://<host>/<org>/_apis` then lists that host's routes:
//! GUID (`id`), area, resource name, route template and versions. Both need the PAT. Read-only.

use std::collections::{BTreeSet, HashMap};

use indexmap::IndexMap;
use serde_json::Value;

use crate::http::{Request, Transport};
use crate::routes::{auth_header, fetch_routes};

const USER_AGENT: &str = concat!("azdo-manuals/", env!("CARGO_PKG_VERSION"), " routes");
pub const MAIN_HOST: &str = "dev.azure.com";

#[derive(Clone, Debug)]
pub struct Route {
    pub host: String,
    /// Lower case.
    pub guid: String,
    pub area: String,
    pub resource: String,
    pub template: String,
    pub resource_version: u64,
    pub min_version: String,
    pub max_version: String,
    pub released_version: String,
    /// The route as the service sent it (for `routes::route_segments` and `version_problem`).
    pub raw: Value,
}

#[derive(Debug, Default)]
pub struct Live {
    pub routes: Vec<Route>,
    /// Area (lower case) -> host, from `resourceAreas`.
    pub area_hosts: HashMap<String, String>,
    /// Host -> number of routes, or why its list could not be read. In the order asked.
    pub hosts: IndexMap<String, Result<usize, String>>,
}

impl Live {
    /// The routes with this GUID, one per host that serves it.
    pub fn by_guid(&self) -> HashMap<&str, Vec<&Route>> {
        let mut out: HashMap<&str, Vec<&Route>> = HashMap::new();
        for r in &self.routes {
            out.entry(r.guid.as_str()).or_default().push(r);
        }
        out
    }
}

fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    Some(rest.split('/').next()?.to_lowercase())
}

pub fn parse_route(host: &str, r: &Value) -> Option<Route> {
    let s = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let guid = s("id").to_lowercase();
    if guid.is_empty() {
        return None;
    }
    Some(Route {
        host: host.to_string(),
        guid,
        area: s("area"),
        resource: s("resourceName"),
        template: s("routeTemplate"),
        resource_version: r
            .get("resourceVersion")
            .and_then(Value::as_u64)
            .unwrap_or(1),
        min_version: s("minVersion"),
        max_version: s("maxVersion"),
        released_version: s("releasedVersion"),
        raw: r.clone(),
    })
}

/// The area -> host map from a `resourceAreas` answer.
pub fn area_hosts(answer: &Value) -> HashMap<String, String> {
    answer["value"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let name = a["name"].as_str()?.to_lowercase();
            let host = host_of(a["locationUrl"].as_str()?)?;
            Some((name, host))
        })
        .collect()
}

/// Read `resourceAreas`, then the route list of every host it names. A host whose list cannot be read is recorded
/// in `hosts` and skipped; only a failing `resourceAreas` is an error.
pub fn fetch(transport: &mut Transport, org: &str, pat: &str) -> Result<Live, String> {
    let url = format!("https://{MAIN_HOST}/{org}/_apis/resourceAreas?api-version=7.1-preview.1");
    let req = Request {
        method: "GET",
        url,
        headers: vec![
            ("Authorization".into(), auth_header(pat)),
            ("Accept".into(), "application/json".into()),
            ("User-Agent".into(), USER_AGENT.into()),
        ],
        body: None,
    };
    let resp = transport(&req).map_err(|e| format!("resourceAreas: {e}"))?;
    if resp.status == 401 || resp.status == 203 {
        return Err(format!(
            "resourceAreas: HTTP {} (the PAT is not valid for this organization)",
            resp.status
        ));
    }
    if !(200..300).contains(&resp.status) {
        return Err(format!("resourceAreas: HTTP {}", resp.status));
    }
    let answer: Value =
        serde_json::from_slice(&resp.body).map_err(|e| format!("resourceAreas: bad JSON ({e})"))?;
    let area_hosts = area_hosts(&answer);
    let mut hosts: BTreeSet<String> = area_hosts.values().cloned().collect();
    hosts.insert(MAIN_HOST.to_string());
    let mut live = Live {
        area_hosts,
        ..Live::default()
    };
    for host in hosts {
        match fetch_routes(transport, &format!("https://{host}/{org}"), pat) {
            Ok(list) => {
                let before = live.routes.len();
                live.routes
                    .extend(list.iter().filter_map(|r| parse_route(&host, r)));
                live.hosts.insert(host, Ok(live.routes.len() - before));
            }
            Err(e) => {
                live.hosts.insert(host, Err(e.replace(org, "<org>")));
            }
        }
    }
    Ok(live)
}
