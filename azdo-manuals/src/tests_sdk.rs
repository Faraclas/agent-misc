//! Offline tests of the inputs besides the spec: the client-library parsers and download, the live route list, the
//! join, the Analytics manual, and `route-gaps`-style coverage. Hand-made fixtures in tests/fixtures/sdk,
//! tests/fixtures/live.json and tests/fixtures/analytics-metadata.xml; no network.
//!
//! Set UPDATE_EXPECTED=1 to rewrite tests/fixtures/expected-extra/ after an intended change (then review the diff).

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::rc::Rc;

use indexmap::IndexMap;
use serde_json::{json, Value};

use crate::analytics;
use crate::archive::git_blob_sha1;
use crate::config::{manual, texts, Config};
use crate::generate::{generate_with, Extra};
use crate::http::{Request, Response, Transport};
use crate::live::{self, Live, Route};
use crate::routes;
use crate::sdk::{self, Method, Parsed, Place, Sdk, Ty, TypeDef};
use crate::sdk_go;
use crate::sdk_node;
use crate::sdkops;
use crate::tests::{expected_dir, fixtures, load_fixture_specs, source, test_config};
use crate::text::write_json;

const ORG: &str = "contoso";

/// The fixture library files of one SDK, as (path in the repository, text), like `sdk::load_files` returns them.
fn library_files(lib: Sdk) -> Vec<(String, String)> {
    let root = fixtures().join("sdk").join(lib.key());
    let mut out = Vec::new();
    fn walk(dir: &Path, root: &Path, lib: Sdk, out: &mut Vec<(String, String)>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(&p, root, lib, out);
            } else {
                let rel = p
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                if lib.wants(&rel) {
                    out.push((rel, fs::read_to_string(&p).unwrap().replace("\r\n", "\n")));
                }
            }
        }
    }
    walk(&root, &root, lib, &mut out);
    out.sort();
    out
}

fn go() -> Parsed {
    sdk_go::parse(&library_files(Sdk::Go))
}

fn node() -> Parsed {
    sdk_node::parse(&library_files(Sdk::Node))
}

fn method<'a>(p: &'a Parsed, name: &str) -> &'a Method {
    p.methods
        .iter()
        .find(|m| m.name == name)
        .unwrap_or_else(|| panic!("no method {name}"))
}

fn param<'a>(m: &'a Method, name: &str) -> &'a sdk::Param {
    m.params
        .iter()
        .find(|p| p.name == name)
        .unwrap_or_else(|| panic!("{} has no parameter {name}", m.name))
}

fn live_fixture() -> Value {
    serde_json::from_str(&fs::read_to_string(fixtures().join("live.json")).unwrap()).unwrap()
}

fn live() -> Live {
    let data = live_fixture();
    let mut live = Live {
        area_hosts: live::area_hosts(&data["resourceAreas"]),
        ..Live::default()
    };
    for (host, routes) in data["routes"].as_object().unwrap() {
        for r in routes.as_array().unwrap() {
            live.routes.push(live::parse_route(host, r).unwrap());
        }
        live.hosts
            .insert(host.clone(), Ok(routes.as_array().unwrap().len()));
    }
    live
}

/// The test configuration plus what the extra inputs need: a client-library area and the Analytics manual.
fn extra_config() -> Config {
    let mut conf = test_config();
    conf.manuals[0] = conf.manuals[0].clone().with_sdk_areas(&["location"]);
    conf.manuals.push(
        manual(analytics::MANUAL, &[], "Test analytics", "test analytics")
            .with_sdk_areas(&[analytics::AREA]),
    );
    conf.route_areas = vec![("location".into(), "location".into())];
    conf.excluded_route_areas = vec![("clienttrace".into(), "client telemetry".into())];
    // Client-library operations are named `<Resource>.<Method>`, as in the real configuration.
    conf.summaries.extend(texts(&[(
        "location",
        "ConnectionData.GetConnectionData",
        "Get the calling user.",
    )]));
    conf.notes.extend(texts(&[(
        "location",
        "ConnectionData.GetConnectionData",
        "Who am I.",
    )]));
    conf
}

fn run_extra() -> (IndexMap<String, Value>, Value, sdkops::Joined) {
    let conf = extra_config();
    let specs = load_fixture_specs();
    let (go, node) = (go(), node());
    let (joined, _) = sdkops::join(&specs, &[&go, &node], &live(), &conf, "7.1");
    let csdl = fs::read_to_string(fixtures().join("analytics-metadata.xml")).unwrap();
    let extra = Extra {
        specs: joined.specs.clone(),
        live: true,
        left_out: joined.left_out.clone(),
        entries: analytics::entries(&csdl, ORG)
            .unwrap()
            .into_iter()
            .map(|e| (analytics::MANUAL.to_string(), e))
            .collect(),
        scope_descriptions: HashMap::from([(
            analytics::SCOPE.into(),
            analytics::SCOPE_DESCRIPTION.into(),
        )]),
        report: json!({"test": true}),
    };
    let (manuals, report) = generate_with(&specs, &extra, ORG, "7.1", &source(), &conf).unwrap();
    (manuals, report, joined)
}

fn tool<'a>(manuals: &'a IndexMap<String, Value>, manual: &str, name: &str) -> &'a Value {
    manuals[manual]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == name)
        .unwrap_or_else(|| panic!("{manual} has no tool {name}"))
}

fn names(manuals: &IndexMap<String, Value>, manual: &str) -> Vec<String> {
    manuals[manual]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

// Parsers

#[test]
fn go_parser_reads_methods_and_models() {
    let p = go();
    let m = method(&p, "GetGadgets");
    assert_eq!(
        (m.guid.as_str(), m.http_method.as_str(), m.version.as_str()),
        (
            "11111111-0000-0000-0000-000000000002",
            "get",
            "7.1-preview.1"
        )
    );
    assert_eq!(
        m.description, "List gadgets.",
        "the [Preview API] mark is removed"
    );
    assert_eq!(
        param(m, "ids").ty,
        Ty::CommaList(Box::new(Ty::Integer("int32")))
    );
    assert_eq!(param(m, "ids").description, "Only these gadgets.");
    assert_eq!(
        param(m, "searchCriteria.kind").ty,
        Ty::Named("go:alpha.GadgetKind".into())
    );
    assert_eq!(
        param(m, "searchCriteria.kind").description,
        "Only gadgets of this kind."
    );
    assert_eq!(param(m, "since").ty, Ty::DateTime);
    assert_eq!(param(m, "project").place, Place::Path);
    let get = method(&p, "GetGadget");
    assert!(!param(get, "project").required, "(optional) route value");
    assert!(param(get, "gadgetId").required);
    let create = method(&p, "CreateGadget");
    assert_eq!(
        param(create, "body").ty,
        Ty::Named("go:alpha.Gadget".into())
    );
    assert_eq!(create.content_type.as_deref(), Some("application/json"));
    let update = method(&p, "UpdateGadget");
    assert_eq!(param(update, "If-Match").place, Place::Header);
    assert!(method(&p, "GetWidgetArchive").binary_answer);
    assert_eq!(method(&p, "GetWidgetArchive").accept, "application/zip");
    let conn = method(&p, "GetConnectionData");
    assert_eq!(param(conn, "lastChangeId64").ty, Ty::Integer("int64"));
    assert_eq!(
        param(conn, "connectOptions").ty,
        Ty::Named("go:webapi.ConnectOptions".into())
    );
    match &p.types["go:alpha.GadgetKind"] {
        TypeDef::Enum {
            values,
            description,
        } => {
            assert_eq!(values, &["small", "large"]);
            assert_eq!(description, "Kinds of gadgets.");
        }
        other => panic!("{other:?}"),
    }
    match &p.types["go:alpha.Gadget"] {
        TypeDef::Object { properties, .. } => {
            let by: HashMap<&str, &Ty> =
                properties.iter().map(|(n, t, _)| (n.as_str(), t)).collect();
            assert_eq!(by["owner"], &Ty::Named("go:webapi.IdentityRef".into()));
            assert_eq!(
                by["parts"],
                &Ty::Array(Box::new(Ty::Named("go:alpha.GadgetPart".into())))
            );
            assert_eq!(by["labels"], &Ty::Map(Box::new(Ty::String)));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn node_parser_reads_methods_and_interfaces() {
    let p = node();
    let m = method(&p, "GetGadgets");
    assert_eq!(m.version, "7.2-preview.3");
    assert_eq!(
        param(m, "$top").ty,
        Ty::Number,
        "the wire name, not the argument name"
    );
    let h = method(&p, "GetGadgetHistory");
    assert_eq!(
        param(h, "criteria.kind").ty,
        Ty::Named("node:AlphaInterfaces.GadgetKind".into())
    );
    assert_eq!(param(h, "criteria.includeDeleted").ty, Ty::Boolean);
    assert!(
        !h.params.iter().any(|p| p.name == "criteria.owner"),
        "nested objects are not flattened twice"
    );
    assert_eq!(
        param(h, "fields").ty,
        Ty::CommaList(Box::new(Ty::String)),
        "x && x.join(\",\")"
    );
    assert_eq!(param(h, "since").ty, Ty::DateTime);
    assert_eq!(param(h, "gadgetId").description, "The gadget's id.");
    let f = method(&p, "GetBranchFilters");
    assert_eq!(
        f.route_constants,
        vec![("action".to_string(), "Branches".to_string())]
    );
    assert!(!f.params.iter().any(|p| p.name == "action"));
    let up = method(&p, "CreateAttachment");
    assert_eq!(up.http_method, "put");
    assert_eq!(param(up, "body").ty, Ty::Stream);
    assert_eq!(up.content_type.as_deref(), Some("application/octet-stream"));
    let patch = method(&p, "UpdateGadgetParts");
    assert_eq!(patch.http_method, "patch");
    assert_eq!(
        patch.content_type.as_deref(),
        Some("application/json-patch+json")
    );
    let log = method(&p, "GetGadgetLog");
    assert!(log.binary_answer);
    assert_eq!(log.accept, "text/plain");
    let board = method(&p, "GetBoardSettings");
    assert_eq!(param(board, "team").description, "Team ID or team name");
    match &p.types["node:AlphaInterfaces.GadgetRevision"] {
        TypeDef::Object { properties, .. } => {
            let names: Vec<&str> = properties.iter().map(|(n, _, _)| n.as_str()).collect();
            assert_eq!(
                names,
                ["id", "kind", "owner", "rev", "fields"],
                "inherited first"
            );
            assert_eq!(properties[4].1, Ty::Map(Box::new(Ty::Any)));
        }
        other => panic!("{other:?}"),
    }
    match &p.types["node:AlphaInterfaces.GadgetKind"] {
        TypeDef::Enum { values, .. } => assert_eq!(values, &["small", "large"]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn library_file_selection() {
    assert!(Sdk::Go.wants("azuredevops/v7/git/client.go"));
    assert!(Sdk::Go.wants("azuredevops/v7/git/models.go"));
    assert!(!Sdk::Go.wants("azuredevops/v7/client.go"));
    assert!(!Sdk::Go.wants("azuredevops/v6/git/client.go"));
    assert!(Sdk::Node.wants("api/GitApi.ts"));
    assert!(Sdk::Node.wants("api/TaskAgentApiBase.ts"));
    assert!(Sdk::Node.wants("api/interfaces/common/VSSInterfaces.ts"));
    assert!(!Sdk::Node.wants("api/VsoClient.ts"));
    assert!(!Sdk::Node.wants("test/units/GitApi.ts"));
}

#[test]
fn shipped_pins_name_both_libraries() {
    let pins = crate::archive::load_pins(None).unwrap();
    for lib in Sdk::all() {
        let pin =
            sdk::pin_of(&pins, lib).unwrap_or_else(|| panic!("pins.json has no sdk.{}", lib.key()));
        assert_eq!(pin["commit"].as_str().unwrap().len(), 40);
        let files = pin["files"].as_array().unwrap();
        assert!(
            files.len() > 20,
            "{} pins only {} files",
            lib.key(),
            files.len()
        );
        for f in files {
            let path = f["file"].as_str().unwrap();
            assert!(lib.wants(path), "{path} is not a file the parser reads");
            assert_eq!(f["sha1_git"].as_str().unwrap().len(), 40);
        }
    }
}

// Download

/// A gzip-compressed tar with `<root>/` before every path, as GitHub's codeload serves it. A path over 100 bytes
/// gets a pax header, like git archive writes.
fn tarball(root: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
    fn header(name: &str, size: usize, kind: u8) -> [u8; 512] {
        let mut h = [0u8; 512];
        h[..name.len().min(100)].copy_from_slice(&name.as_bytes()[..name.len().min(100)]);
        h[100..107].copy_from_slice(b"0000644");
        let size = format!("{size:011o}");
        h[124..135].copy_from_slice(size.as_bytes());
        h[156] = kind;
        h[257..263].copy_from_slice(b"ustar\0");
        h
    }
    let mut tar = Vec::new();
    let pad = |tar: &mut Vec<u8>| tar.resize(tar.len().div_ceil(512) * 512, 0);
    tar.extend_from_slice(&header("pax_global_header", 0, b'g'));
    for (path, data) in files {
        let name = format!("{root}/{path}");
        if name.len() > 100 {
            let record = format!("path={name}\n");
            let record = format!("{} {record}", record.len() + 3);
            tar.extend_from_slice(&header("PaxHeader", record.len(), b'x'));
            tar.extend_from_slice(record.as_bytes());
            pad(&mut tar);
        }
        tar.extend_from_slice(&header(&name, data.len(), b'0'));
        tar.extend_from_slice(data);
        pad(&mut tar);
    }
    tar.extend_from_slice(&[0u8; 1024]);
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&tar).unwrap();
    gz.finish().unwrap()
}

type Seen = Rc<RefCell<Vec<String>>>;

fn serve(seen: Seen, mut answer: impl FnMut(&Request) -> Response + 'static) -> Transport {
    Box::new(move |req: &Request| {
        seen.borrow_mut()
            .push(format!("{} {}", req.method, req.url));
        Ok(answer(req))
    })
}

fn status(code: u16, body: &[u8]) -> Response {
    Response {
        status: code,
        headers: vec![],
        body: body.to_vec(),
    }
}

#[test]
fn untar_reads_paths_and_long_names() {
    let long = format!("api/{}/X.ts", "d".repeat(120));
    let tgz = tarball("repo-abc", &[("api/GitApi.ts", b"one"), (&long, b"two")]);
    let files = sdk::untar_gz(&tgz).unwrap();
    let got: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, d)| (p.as_str(), d.as_slice()))
        .collect();
    assert_eq!(
        got,
        [("api/GitApi.ts", &b"one"[..]), (long.as_str(), &b"two"[..])]
    );
}

#[test]
fn library_files_are_verified_and_cached() {
    let tmp = tempfile::tempdir().unwrap();
    let tgz = tarball(
        "r-c",
        &[("api/GitApi.ts", b"git"), ("api/Other.ts", b"other")],
    );
    let pin = json!({"repo": "o/r", "commit": "c", "files": [{"file": "api/GitApi.ts", "sha1_git": git_blob_sha1(b"git")}]});
    let seen: Seen = Rc::default();
    let served = tgz.clone();
    let mut t = serve(seen.clone(), move |_| status(200, &served));
    let (files, fetched) = sdk::load_files(&mut t, &pin, tmp.path()).unwrap();
    assert!(fetched);
    assert_eq!(files, [("api/GitApi.ts".to_string(), "git".to_string())]);
    assert_eq!(
        seen.borrow().as_slice(),
        ["GET https://codeload.github.com/o/r/tar.gz/c"]
    );
    let (again, fetched) = sdk::load_files(&mut t, &pin, tmp.path()).unwrap();
    assert!(!fetched, "the second run reads the cache");
    assert_eq!(again, files);
    assert_eq!(seen.borrow().len(), 1);

    let wrong = json!({"repo": "o/r", "commit": "c", "files": [{"file": "api/Other.ts", "sha1_git": git_blob_sha1(b"x")}]});
    let e = sdk::load_files(&mut t, &wrong, tmp.path()).unwrap_err();
    assert!(e.0.contains("pinned"), "{e}");
    let missing = json!({"repo": "o/r", "commit": "c", "files": [{"file": "api/Gone.ts", "sha1_git": git_blob_sha1(b"x")}]});
    let e = sdk::load_files(&mut t, &missing, tmp.path()).unwrap_err();
    assert!(e.0.contains("not in the tarball"), "{e}");
}

// Live routes

#[test]
fn live_routes_follow_resource_areas() {
    let data = live_fixture();
    let seen: Seen = Rc::default();
    let mut t = serve(seen.clone(), move |req| {
        if req.url.contains("resourceAreas") {
            return status(200, data["resourceAreas"].to_string().as_bytes());
        }
        let host = req
            .url
            .split("://")
            .nth(1)
            .unwrap()
            .split('/')
            .next()
            .unwrap();
        match data["routes"].get(host) {
            Some(routes) => status(
                200,
                json!({"count": 1, "value": routes}).to_string().as_bytes(),
            ),
            None => status(404, b""),
        }
    });
    let live = live::fetch(&mut t, ORG, "pat").unwrap();
    assert_eq!(live.area_hosts["beta"], "vsbeta.dev.azure.com");
    assert_eq!(live.hosts.len(), 2);
    assert!(live
        .routes
        .iter()
        .any(|r| r.host == "vsbeta.dev.azure.com" && r.area == "beta"));
    let calls = seen.borrow();
    assert!(calls[0].starts_with("GET https://dev.azure.com/contoso/_apis/resourceAreas"));
    assert!(calls.contains(&"OPTIONS https://vsbeta.dev.azure.com/contoso/_apis".to_string()));

    let mut refused = serve(Rc::default(), |_| status(401, b""));
    let e = live::fetch(&mut refused, ORG, "pat").unwrap_err();
    assert!(e.contains("not valid for this organization"), "{e}");
}

// Join

fn route(min: &str, max: &str, released: &str, resource_version: u64) -> Route {
    live::parse_route(
        "dev.azure.com",
        &json!({"id": "x", "area": "a", "resourceName": "r", "routeTemplate": "_apis/{area}/{resource}",
                "resourceVersion": resource_version, "minVersion": min, "maxVersion": max,
                "releasedVersion": released}),
    )
    .unwrap()
}

#[test]
fn api_version_follows_the_route() {
    let v = |r: Route, lib: &str| sdkops::api_version_for(&r, lib, "7.1");
    assert_eq!(
        v(route("1.0", "7.2", "7.1", 3), "7.2-preview.3"),
        "7.1",
        "released at 7.1"
    );
    assert_eq!(
        v(route("1.0", "7.2", "0.0", 3), "7.1-preview.1"),
        "7.1-preview.1",
        "the library's preview"
    );
    assert_eq!(
        v(route("1.0", "7.2", "0.0", 2), "7.2-preview.4"),
        "7.1-preview.2",
        "capped at resourceVersion"
    );
    assert_eq!(
        v(route("1.0", "7.2", "5.0", 1), "7.1"),
        "7.1-preview.1",
        "released before 7.1 only"
    );
    assert_eq!(
        v(route("1.0", "5.0", "0.0", 2), "7.2-preview.1"),
        "5.0-preview.1",
        "maxVersion below 7.1"
    );
    assert_eq!(v(route("1.0", "6.0", "6.0", 1), "7.1"), "6.0");
}

#[test]
fn join_adds_what_the_spec_lacks() {
    let (manuals, report, joined) = run_extra();
    let main = names(&manuals, "t_main");
    for added in [
        "create_gadget",
        "get_gadget",
        "get_gadgets",
        "update_gadget",
        "get_gadget_history",
        "get_branch_filters",
        "create_attachment",
        "update_gadget_parts",
        "get_gadget_log",
        "get_board_settings",
        "get_connection_data",
    ] {
        assert!(
            main.contains(&added.to_string()),
            "{added} missing from {main:?}"
        );
    }
    // Covered by the spec: the same client method, and the same endpoint, method and form.
    assert!(!main.contains(&"get_widget_archive".to_string()));
    assert_eq!(
        main.iter().filter(|n| n.contains("widget_by_id")).count(),
        1
    );

    let gadgets = tool(&manuals, "t_main", "get_gadgets");
    let t = &gadgets["tool_call_template"];
    assert_eq!(
        t["url"],
        "https://dev.azure.com/contoso/{project}/_apis/alpha/gadgets?api-version=7.1-preview.1",
        "Go wins over Node; the library's preview number"
    );
    let inputs = &gadgets["inputs"]["properties"];
    assert_eq!(inputs["ids"]["type"], "string");
    assert!(inputs["ids"]["description"]
        .as_str()
        .unwrap()
        .contains("comma"));
    assert_eq!(
        inputs["searchCriteria.kind"]["enum"],
        json!(["small", "large"])
    );
    assert_eq!(inputs["since"]["format"], "date-time");
    assert!(
        inputs.get("$top").is_none(),
        "Node's parameters are not mixed in"
    );

    let get = tool(&manuals, "t_main", "get_gadget");
    assert_eq!(get["inputs"]["required"], json!(["gadgetId"]));

    let create = tool(&manuals, "t_main", "create_gadget");
    assert!(create["description"]
        .as_str()
        .unwrap()
        .starts_with("(untested)"));
    let body = &create["inputs"]["properties"]["body"];
    assert_eq!(
        body["properties"]["kind"]["enum"],
        json!(["small", "large"])
    );
    assert_eq!(
        body["properties"]["owner"]["properties"]["displayName"]["type"],
        "string"
    );
    assert_eq!(
        body["properties"]["parts"]["items"]["properties"]["weight"]["type"],
        "number"
    );
    assert!(
        body["properties"].get("url").is_none(),
        "url is dropped like in spec bodies"
    );

    let update = tool(&manuals, "t_main", "update_gadget");
    assert_eq!(
        update["tool_call_template"]["header_fields"],
        json!(["If-Match"])
    );

    let history = tool(&manuals, "t_main", "get_gadget_history");
    assert!(history["tool_call_template"]["url"]
        .as_str()
        .unwrap()
        .ends_with("/gadgets/{gadgetId}/history?api-version=7.1"));
    assert_eq!(
        history["inputs"]["properties"]["criteria.includeDeleted"]["type"],
        "boolean"
    );

    let filters = tool(&manuals, "t_main", "get_branch_filters");
    assert!(filters["tool_call_template"]["url"]
        .as_str()
        .unwrap()
        .contains("/repositories/{repository}/filters/Branches?"));

    let upload = tool(&manuals, "t_main", "create_attachment");
    assert_eq!(upload["tool_call_template"]["body_type"], "file");
    let patch = tool(&manuals, "t_main", "update_gadget_parts");
    assert_eq!(
        patch["tool_call_template"]["headers"]["Content-Type"],
        "application/json-patch+json"
    );
    assert!(patch["tool_call_template"]["url"]
        .as_str()
        .unwrap()
        .ends_with("api-version=5.0-preview.1"));
    let log = tool(&manuals, "t_main", "get_gadget_log");
    assert_eq!(log["outputs"], json!({"type": "string"}));
    let board = tool(&manuals, "t_main", "get_board_settings");
    assert!(board["tool_call_template"]["url"]
        .as_str()
        .unwrap()
        .contains("/{project}/{team}/_apis/"));

    let conn = tool(&manuals, "t_main", "get_connection_data");
    assert_eq!(
        conn["tool_call_template"]["url"],
        "https://dev.azure.com/contoso/_apis/ConnectionData?api-version=7.1-preview.1",
        "the host resourceAreas names, though vsbeta serves it too"
    );
    assert_eq!(
        conn["inputs"]["properties"]["connectOptions"]["enum"],
        json!(["none", "includeServices"])
    );
    assert_eq!(
        conn["description"], "Get the calling user. Who am I.",
        "config summaries and notes reach client-library tools"
    );

    // Left out, with reasons.
    let reasons: HashMap<String, String> = report["left_out"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| {
            (
                x["operation_id"].as_str().unwrap().to_string(),
                x["reason"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(reasons["gadgets.DeleteGadget"], "DELETE not selected");
    assert_eq!(reasons["Events.PublishEvents"], "client telemetry");
    assert_eq!(
        reasons["things.GetMystery"],
        "route area Mystery is in no manual yet"
    );
    assert!(reasons["future.GetFuture"].starts_with("starts at API 7.2"));
    assert_eq!(joined.not_on_organization, ["go alpha.GetOrphan"]);

    // Scopes: inferred from the spec's operations in the same area, none for an area without any.
    let m = &report["manuals"]["t_main"];
    let inferred: Vec<&str> = m["tools_with_inferred_scopes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(inferred.contains(&"get_gadgets") && inferred.contains(&"create_gadget"));
    assert!(m["tools_without_scope"]
        .as_array()
        .unwrap()
        .contains(&json!("get_connection_data")));
    assert_eq!(m["sources"]["spec"], 14, "every spec tool stays");
    assert!(manuals["t_main"]["info"]["description"]
        .as_str()
        .unwrap()
        .contains("from Microsoft's REST API specs and Microsoft's client libraries"));
}

#[test]
fn spec_tools_are_unchanged_by_the_libraries() {
    let (plain, _) = crate::generate::generate(
        &load_fixture_specs(),
        ORG,
        "7.1",
        &source(),
        &extra_config(),
    )
    .unwrap();
    let (with, _, _) = run_extra();
    for t in plain["t_main"]["tools"].as_array().unwrap() {
        let name = t["name"].as_str().unwrap();
        assert_eq!(tool(&with, "t_main", name), t, "{name} changed");
    }
}

#[test]
fn library_names_yield_to_spec_names() {
    let conf = extra_config();
    let specs = load_fixture_specs();
    let mut lib = go();
    // A library method whose name is a spec tool's: it gets its area as prefix.
    lib.methods
        .iter_mut()
        .find(|m| m.name == "GetGadgets")
        .unwrap()
        .name = "ListWidgets".into();
    let (joined, _) = sdkops::join(&specs, &[&lib], &live(), &conf, "7.1");
    let extra = Extra {
        specs: joined.specs,
        live: true,
        ..Extra::default()
    };
    let (manuals, _) = generate_with(&specs, &extra, ORG, "7.1", &source(), &conf).unwrap();
    let main = names(&manuals, "t_main");
    assert!(main.contains(&"list_widgets".to_string()));
    assert!(main.contains(&"alpha_list_widgets".to_string()), "{main:?}");
}

#[test]
fn config_names_of_library_operations() {
    let mut conf = extra_config();
    conf.skip_gets
        .entry("alpha".into())
        .or_default()
        .insert("gadgets.GetGadget".into(), "test skip".into());
    let specs = load_fixture_specs();
    // Without the live routes a library operation's name cannot be checked, and is not.
    crate::generate::generate(&specs, ORG, "7.1", &source(), &conf).unwrap();
    let (go, node) = (go(), node());
    let (joined, _) = sdkops::join(&specs, &[&go, &node], &live(), &conf, "7.1");
    let extra = Extra {
        specs: joined.specs,
        live: true,
        ..Extra::default()
    };
    let (manuals, report) = generate_with(&specs, &extra, ORG, "7.1", &source(), &conf).unwrap();
    assert!(!names(&manuals, "t_main").contains(&"get_gadget".to_string()));
    assert!(report["left_out"]
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x["reason"] == "skipped: test skip"));
    // With them, a typo is an error.
    conf.skip_gets
        .get_mut("alpha")
        .unwrap()
        .insert("gadgets.GetGadgett".into(), "typo".into());
    let e = generate_with(&specs, &extra, ORG, "7.1", &source(), &conf).unwrap_err();
    assert!(e.0.contains("gadgets.GetGadgett"), "{e}");
}

#[test]
fn route_check_reports_odata_apart() {
    let (manuals, _, _) = run_extra();
    let data = live_fixture();
    let mut fetch = |base: &str| -> Result<Vec<Value>, String> {
        let host = base.split("://").nth(1).unwrap().split('/').next().unwrap();
        Ok(data["routes"]
            .get(host)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    };
    let result = routes::check(&manuals, ORG, &mut fetch);
    assert_eq!(result.odata.len(), 2);
    let library: Vec<&String> = result
        .unmatched
        .iter()
        .chain(&result.version)
        .filter(|n| n.contains("gadget") || n.contains("connection"))
        .collect();
    assert!(
        library.is_empty(),
        "library tools fail the route check: {library:?}"
    );
}

// Analytics

#[test]
fn analytics_tools_from_metadata() {
    let csdl = fs::read_to_string(fixtures().join("analytics-metadata.xml")).unwrap();
    let entries = analytics::entries(&csdl, ORG).unwrap();
    assert_eq!(
        entries.len(),
        2,
        "an entity set with an unknown type is skipped"
    );
    let items = &entries[0].tool;
    assert_eq!(entries[0].method_name.as_deref(), Some("QueryWorkItems"));
    assert!(items["description"]
        .as_str()
        .unwrap()
        .starts_with("Current state of work items & their fields."));
    let select = items["inputs"]["properties"]["$select"]["description"]
        .as_str()
        .unwrap();
    assert!(select.contains("WorkItemId (Int32), Title (String)"));
    assert!(select.contains("and 2 custom fields"), "{select}");
    assert!(!select.contains("Custom_Alpha"));
    let expand = items["inputs"]["properties"]["$expand"]["description"]
        .as_str()
        .unwrap();
    assert!(expand.contains("Area, Children") && !expand.contains("Custom_Person"));
    assert_eq!(
        items["tool_call_template"]["url"],
        "https://analytics.dev.azure.com/contoso/{project}/_odata/v4.0-preview/WorkItems"
    );
    assert_eq!(entries[0].scopes, ["vso.analytics"]);
    let snapshot = entries[1].tool["description"].as_str().unwrap();
    assert!(
        snapshot.contains("filter on DateSK") && snapshot.contains("meant for aggregation"),
        "{snapshot}"
    );
    assert!(analytics::parse_xml("<a><b></a>").is_err());
    assert!(analytics::entries("<x/>", ORG).is_err());
}

#[test]
fn readtest_dates_are_relative() {
    let v = crate::readtest::resolve(&json!("DateSK ge {@date:0}"), &HashMap::new()).unwrap();
    let today = chrono::Local::now()
        .date_naive()
        .format("%Y%m%d")
        .to_string();
    assert_eq!(v, json!(format!("DateSK ge {today}")));
}

// Whole output

#[test]
fn extra_manuals_match_expected_bytes() {
    let (manuals, report, _) = run_extra();
    let dir = expected_dir().with_file_name("expected-extra");
    let mut outputs: Vec<(String, Value)> = manuals
        .into_iter()
        .map(|(name, m)| (format!("{name}.utcp.json"), m))
        .collect();
    outputs.push(("report.json".into(), report));
    if std::env::var_os("UPDATE_EXPECTED").is_some() {
        fs::create_dir_all(&dir).unwrap();
        for old in fs::read_dir(&dir).unwrap() {
            fs::remove_file(old.unwrap().path()).unwrap();
        }
        for (name, data) in &outputs {
            write_json(&dir.join(name), data).unwrap();
        }
    }
    let tmp = tempfile::tempdir().unwrap();
    let mut on_disk: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    on_disk.sort();
    let mut want: Vec<String> = outputs.iter().map(|(n, _)| n.clone()).collect();
    want.sort();
    assert_eq!(on_disk, want);
    for (name, data) in &outputs {
        let path = tmp.path().join(name);
        write_json(&path, data).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            fs::read_to_string(dir.join(name))
                .unwrap()
                .replace("\r\n", "\n"),
            "{name} differs from tests/fixtures/expected-extra (UPDATE_EXPECTED=1 rewrites it)"
        );
    }
}
