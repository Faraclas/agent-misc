//! Offline tests: hand-made specs in, exact manuals out. No network.
//!
//! Set UPDATE_EXPECTED=1 to rewrite tests/fixtures/expected/ after an intended change (then review the diff).
//! The utcp load test runs the `utcp` from UTCP_BIN, else from PATH, and is skipped when there is none (CI sets
//! REQUIRE_UTCP=1 on Linux to make it fail instead).
//! AZDO_TEST_FIXTURES points the compiled test binary at a copy of tests/fixtures/, so it can run on a machine
//! without the source tree (a Windows test machine).

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;

use indexmap::IndexMap;
use serde_json::{json, Map, Value};

use crate::archive::{self, Archive};
use crate::cli::{scopes_markdown, which};
use crate::config::{list_map, manual, nested, Config};
use crate::generate::{
    api_version_of, assign_names, base_name, covering_scopes, generate, minimal_scopes, Entry,
    Specs,
};
use crate::http::{Request, Response, Transport};
use crate::readtest::{self, ReadTest};
use crate::routes;
use crate::text::{one_line, snake, write_json};

const ORG: &str = "contoso";

fn fixtures() -> PathBuf {
    if let Some(dir) = std::env::var_os("AZDO_TEST_FIXTURES") {
        return PathBuf::from(dir);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn expected_dir() -> PathBuf {
    fixtures().join("expected")
}

fn source() -> Value {
    json!({"revision": "0".repeat(40), "revision_date": "2026-01-01T00:00:00+00:00", "revision_message": "test"})
}

fn test_config() -> Config {
    Config {
        manuals: vec![
            manual(
                "t_main",
                &["alpha", "beta", "gamma"],
                "Test main",
                "test main",
            ),
            manual("t_empty", &["gamma"], "Test empty", "test empty"),
        ],
        post_reads: list_map(&[
            ("alpha", &["Query_Run Query"]),
            ("beta", &["Subject Query_Query"]),
        ]),
        writes: list_map(&[("alpha", &["Widgets_Create"])]),
        allow_write_scopes: HashSet::new(),
        skip_gets: nested(&[("beta", &[("Profiles_Get", "test skip".to_string())])]),
        required_params: nested(&[("alpha", &[("Widgets_List", vec!["kind".to_string()])])]),
        names: nested(&[(
            "beta",
            &[("Subject Query_Query", "query_subjects".to_string())],
        )]),
        drop_optional_path_params: HashMap::from([(
            "alpha".to_string(),
            HashSet::from(["team".to_string()]),
        )]),
        body_overrides: HashMap::new(),
    }
}

fn load_fixture_specs() -> Specs {
    let mut specs = Specs::new();
    for name in ["alpha", "beta"] {
        let text = fs::read_to_string(fixtures().join(format!("{name}.json"))).unwrap();
        specs.insert(name.to_string(), vec![serde_json::from_str(&text).unwrap()]);
    }
    specs
}

fn run_generate_with(conf: &Config) -> (IndexMap<String, Value>, Value) {
    generate(&load_fixture_specs(), ORG, "7.1", &source(), conf).unwrap()
}

fn run_generate() -> (IndexMap<String, Value>, Value) {
    run_generate_with(&test_config())
}

fn tools_by_name(manuals: &IndexMap<String, Value>) -> HashMap<String, Value> {
    manuals["t_main"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| (t["name"].as_str().unwrap().to_string(), t.clone()))
        .collect()
}

fn left_out_reasons(report: &Value) -> HashSet<(String, String)> {
    report["left_out"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| {
            (
                x["operation_id"].as_str().unwrap().to_string(),
                x["reason"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

// Generation

#[test]
fn manuals_match_expected_bytes() {
    let (manuals, report) = run_generate();
    let mut outputs: Vec<(String, Value)> = manuals
        .into_iter()
        .map(|(name, manual)| (format!("{name}.utcp.json"), manual))
        .collect();
    outputs.push(("report.json".into(), report));
    let dir = expected_dir();
    if std::env::var_os("UPDATE_EXPECTED").is_some() {
        fs::create_dir_all(&dir).unwrap();
        for old in fs::read_dir(&dir).unwrap() {
            let old = old.unwrap().path();
            if old.extension().is_some_and(|e| e == "json") {
                fs::remove_file(old).unwrap();
            }
        }
        for (name, data) in &outputs {
            write_json(&dir.join(name), data).unwrap();
        }
    }
    let mut on_disk: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.ends_with(".json"))
        .collect();
    on_disk.sort();
    let mut names: Vec<String> = outputs.iter().map(|(n, _)| n.clone()).collect();
    names.sort();
    assert_eq!(on_disk, names);
    let tmp = tempfile::tempdir().unwrap();
    for (name, data) in &outputs {
        let path = tmp.path().join(name);
        write_json(&path, data).unwrap();
        let got = fs::read_to_string(&path).unwrap();
        let want = fs::read_to_string(dir.join(name))
            .unwrap()
            .replace("\r\n", "\n");
        assert_eq!(
            got, want,
            "{name} differs from tests/fixtures/expected (UPDATE_EXPECTED=1 rewrites it)"
        );
    }
}

#[test]
fn selection_and_urls() {
    let (manuals, report) = run_generate();
    let tools = tools_by_name(&manuals);
    assert!(
        !manuals.contains_key("t_empty"),
        "a manual whose areas have no spec is not written"
    );
    assert_eq!(
        report["manuals"]["t_main"]["areas_without_spec"],
        json!(["gamma"])
    );
    let reasons = left_out_reasons(&report);
    for (op, reason) in [
        ("Widgets_Delete", "DELETE never included"),
        ("Widgets_Exists", "HEAD never included"),
        ("Archive_Get Zip", "binary response"),
        ("Profiles_Get", "skipped: test skip"),
    ] {
        assert!(
            reasons.contains(&(op.to_string(), reason.to_string())),
            "{op}: {reason}"
        );
    }
    assert!(!tools.contains_key("get_profile"));
    assert_eq!(
        tools["alpha_list_widgets"]["inputs"]["required"],
        json!(["kind", "project"])
    );
    // Same operationId in two areas of one manual: both get the area prefix.
    assert!(tools.contains_key("alpha_list_widgets"));
    assert!(tools.contains_key("beta_list_widgets"));
    let url = |n: &str| {
        tools[n]["tool_call_template"]["url"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(
        url("get_widget_by_id"),
        "https://dev.azure.com/contoso/{project}/_apis/alpha/widgets/{widgetId}?api-version=7.1-preview.2"
    );
    assert_eq!(
        url("run_query"),
        "https://dev.azure.com/contoso/{project}/_apis/alpha/query?api-version=7.1"
    );
    assert!(tools["run_query"]["inputs"]["properties"]
        .get("team")
        .is_none());
    assert_eq!(
        url("get_thing_template"),
        "https://dev.azure.com/contoso/{project}/_apis/alpha/things/%24{type}?api-version=7.1"
    );
    assert_eq!(
        url("get_widgets_batch"),
        "https://dev.azure.com/contoso/_apis/alpha/widgets?api-version=7.1"
    );
    assert_eq!(tools["get_log"]["outputs"], json!({"type": "string"}));
    assert_eq!(
        tools["get_log"]["tool_call_template"]["headers"]["Accept"],
        "text/plain"
    );
    assert_eq!(
        tools["get_widget_by_id"]["tool_call_template"]["header_fields"],
        json!(["X-Trace"])
    );
    assert_eq!(
        tools["get_widget_by_id"]["tool_call_template"]["auth"],
        json!({"auth_type": "basic", "username": "pat", "password": "${AZURE_DEVOPS_EXT_PAT}"})
    );
}

#[test]
fn body_schema() {
    let (manuals, _) = run_generate();
    let tools = tools_by_name(&manuals);
    let create = &tools["create_widget"];
    assert_eq!(create["tool_call_template"]["body_field"], "body");
    assert_eq!(
        create["tool_call_template"]["headers"]["Content-Type"],
        "application/json"
    );
    let keys: Vec<&String> = create["inputs"]["properties"]["body"]["properties"]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    assert_eq!(keys, ["name"]);
    let body = &tools["run_query"]["inputs"]["properties"]["body"];
    assert_eq!(body["description"], "The query.");
    let keys: Vec<&String> = body["properties"].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["$top", "options", "parent", "text"]);
    assert_eq!(body["required"], json!(["text"]));
    assert_eq!(
        body["properties"]["parent"],
        json!({"type": "object", "description": "QueryRequest"})
    );
    let level2 = &body["properties"]["options"]["properties"]["level1"]["properties"]["level2"];
    assert_eq!(
        level2,
        &json!({"type": "object", "description": "Too deep to inline."})
    );
}

#[test]
fn body_overrides() {
    let mut conf = test_config();
    let schema =
        json!({"type": "object", "required": ["wiql"], "properties": {"wiql": {"type": "string"}}});
    let mut described = schema.clone();
    described["description"] = json!("Ours.");
    conf.body_overrides = nested(&[(
        "alpha",
        &[
            ("Query_Run Query", schema.clone()),
            ("Widgets_Create", described.clone()),
        ],
    )]);
    let (manuals, _) = run_generate_with(&conf);
    let tools = tools_by_name(&manuals);
    // Without a description of its own, the override takes the body parameter's ("The query.").
    let mut want = schema;
    want["description"] = json!("The query.");
    assert_eq!(tools["run_query"]["inputs"]["properties"]["body"], want);
    assert_eq!(
        tools["create_widget"]["inputs"]["properties"]["body"],
        described
    );
    assert_eq!(
        tools["create_widget"]["tool_call_template"]["body_field"],
        "body"
    );
}

#[test]
fn config_must_name_real_operations() {
    let generate_err = |conf: &Config| {
        generate(&load_fixture_specs(), ORG, "7.1", &source(), conf)
            .unwrap_err()
            .0
    };
    let mut conf = test_config();
    conf.writes = list_map(&[("alpha", &["Widgets_Craete"])]);
    assert_eq!(
        generate_err(&conf),
        "config writes names alpha/Widgets_Craete, which the specs do not have"
    );
    let mut conf = test_config();
    conf.names = nested(&[("beta", &[("Nope_List", "x".to_string())])]);
    assert!(generate_err(&conf).contains("beta/Nope_List"));
    let mut conf = test_config();
    conf.body_overrides = nested(&[("alpha", &[("Widgets_Get", json!({"type": "object"}))])]);
    assert_eq!(
        generate_err(&conf),
        "config body_overrides names alpha/Widgets_Get, which takes no body"
    );
    // An area without a spec (gamma) is not checked: its spec may be missing at the pinned API version.
    let mut conf = test_config();
    conf.writes = list_map(&[("gamma", &["Anything_Create"])]);
    run_generate_with(&conf);
}

#[test]
fn read_only_leaves_out_writes() {
    let (manuals, report) = run_generate();
    assert!(tools_by_name(&manuals).contains_key("create_widget"));
    assert_eq!(report["manuals"]["t_main"]["writes"], 1);
    let (manuals, report) = run_generate_with(&test_config().read_only());
    assert!(!tools_by_name(&manuals).contains_key("create_widget"));
    assert_eq!(report["manuals"]["t_main"]["writes"], 0);
    assert!(left_out_reasons(&report).contains(&(
        "Widgets_Create".to_string(),
        "write not selected".to_string()
    )));
    let scopes: Vec<&str> = report["scopes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["scope"].as_str().unwrap())
        .collect();
    assert_eq!(scopes, ["vso.alpha", "vso.beta"]);
}

#[test]
fn real_writes_have_short_bodies() {
    let conf = Config::real();
    let writes: BTreeSet<(String, String)> = conf
        .writes
        .iter()
        .flat_map(|(a, ops)| ops.iter().map(move |o| (a.clone(), o.clone())))
        .collect();
    let overridden: BTreeSet<(String, String)> = conf
        .body_overrides
        .iter()
        .flat_map(|(a, ops)| ops.keys().map(move |o| (a.clone(), o.clone())))
        .collect();
    assert_eq!(
        writes, overridden,
        "every write has a hand-written body, and only writes do"
    );
    for ((area, op), schema) in conf
        .body_overrides
        .iter()
        .flat_map(|(a, ops)| ops.iter().map(move |(o, s)| ((a, o), s)))
    {
        let kind = schema["type"].as_str().unwrap_or("");
        assert!(kind == "object" || kind == "array", "{area}/{op}");
        assert!(
            schema.get("properties").is_some() || schema.get("items").is_some(),
            "{area}/{op}"
        );
        assert!(schema.to_string().len() < 4000, "{area}/{op} is not short");
    }
    let named = conf.named_operations();
    assert!(named.contains(&("writes", "git", "Pull Requests_Create")));
    assert!(named.contains(&("body_overrides", "wit", "Work Items_Update")));
}

#[test]
fn write_test_checks() {
    use crate::writetest::{check, is_test_project, lookup, Want};
    let data = json!({"status": "active", "comments": [{"content": "x"}], "result": null});
    assert_eq!(lookup(&data, "comments.0.content"), Some(&json!("x")));
    assert_eq!(lookup(&data, "comments.1.content"), None);
    assert_eq!(lookup(&data, "status.0"), None);
    let item = json!({"fields": {"System.Title": "t", "System": {"Other": 1}}});
    assert_eq!(lookup(&item, "fields.System.Title"), Some(&json!("t")));
    assert_eq!(lookup(&item, "fields.System.Other"), Some(&json!(1)));
    assert_eq!(lookup(&item, "fields.System.Missing"), None);
    let expect = [
        ("status", Want::Is(json!("active"))),
        ("comments.0.content", Want::Is(json!("x"))),
        ("missing", Want::Absent),
        ("result", Want::Absent),
        ("result", Want::OneOf(vec![Value::Null, json!("canceled")])),
        (
            "status",
            Want::OneOf(vec![json!("active"), json!("completed")]),
        ),
    ];
    assert!(check(&data, &expect).is_empty());
    let expect = [
        ("status", Want::Is(json!("closed"))),
        ("isDraft", Want::Is(json!(false))),
        ("comments.0.content", Want::Absent),
    ];
    assert_eq!(
        check(&data, &expect),
        [
            ("status".to_string(), json!("active")),
            ("isDraft".to_string(), Value::Null),
            ("comments.0.content".to_string(), json!("x")),
        ]
    );
    assert!(is_test_project("azdo-manuals-test-20261120-120000"));
    assert!(!is_test_project("azdo-manuals-test-"));
    assert!(!is_test_project("azdo-manuals"));
    assert!(!is_test_project("my-azdo-manuals-test-1"));
}

#[test]
fn scope_report() {
    let (_, report) = run_generate();
    assert_eq!(
        report["scopes_all"],
        json!(["vso.alpha", "vso.alpha_write", "vso.beta", "vso.beta_full"])
    );
    // Threads_List accepts vso.beta or vso.beta_full: the read scope covers it, no write scope is asked for.
    let scopes: Vec<&str> = report["scopes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["scope"].as_str().unwrap())
        .collect();
    assert_eq!(scopes, ["vso.alpha_write", "vso.beta"]);
    assert_eq!(
        report["scopes"][0]["description"],
        "Read and write alpha things."
    );
    let md = scopes_markdown(&report, ORG);
    assert!(md.contains("| `vso.alpha_write` | Read and write alpha things. |"));
}

#[test]
fn read_needing_write_scope() {
    let (manuals, report) = run_generate();
    assert!(!tools_by_name(&manuals).contains_key("list_pools"));
    assert!(left_out_reasons(&report).contains(&(
        "Pools_List".to_string(),
        "read needs a write scope: vso.beta_manage".to_string()
    )));
    let mut conf = test_config();
    conf.allow_write_scopes = HashSet::from(["vso.beta_manage".to_string()]);
    let (manuals, report) = run_generate_with(&conf);
    assert!(tools_by_name(&manuals).contains_key("list_pools"));
    assert!(report["scopes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["scope"] == "vso.beta_manage"));
}

#[test]
fn covering() {
    let sets = |s: &[&[&str]]| -> Vec<Vec<String>> {
        s.iter()
            .map(|x| x.iter().map(|y| y.to_string()).collect())
            .collect()
    };
    assert_eq!(
        covering_scopes(&sets(&[
            &["vso.code", "vso.threads_full"],
            &["vso.code"],
            &[]
        ])),
        ["vso.code"]
    );
    assert_eq!(
        covering_scopes(&sets(&[&["vso.profile", "vso.project"]])),
        ["vso.profile"]
    );
    assert_eq!(
        covering_scopes(&sets(&[&["vso.project"], &["vso.profile", "vso.project"]])),
        ["vso.project"]
    );
}

#[test]
fn output_has_no_other_org() {
    let (manuals, report) = run_generate();
    let manuals: Map<String, Value> = manuals.into_iter().collect();
    let text = serde_json::to_string(&json!([manuals, report])).unwrap();
    assert!(!text.contains("{organization}"));
}

// Units

#[test]
fn base_names() {
    for (op, name) in [
        ("Teams_List", "list_teams"),
        ("Teams_Get", "get_team"),
        ("Widgets_Create", "create_widget"),
        ("Classification Nodes_Get", "get_classification_node"),
        ("Policies_Get", "get_policy"),
        ("Work Items_Get Work Item", "get_work_item"),
        ("Wiql_Query By Wiql", "query_by_wiql"),
        (
            "Pull Requests_Get Pull Request By Id",
            "get_pull_request_by_id",
        ),
    ] {
        assert_eq!(base_name(op), name);
    }
}

#[test]
fn api_versions() {
    let op =
        json!({"operationId": "x", "parameters": [{"$ref": "#/parameters/api-Version-preview.3"}]});
    assert_eq!(api_version_of(&op, "7.1").unwrap(), "7.1-preview.3");
    assert!(api_version_of(&json!({"operationId": "x", "parameters": []}), "7.1").is_err());
    let beta =
        json!({"operationId": "x", "parameters": [{"$ref": "#/parameters/api-Version-beta"}]});
    assert!(api_version_of(&beta, "7.1").is_err());
}

#[test]
fn org_name_checked() {
    for bad in ["", "a/b", "my org", &"x".repeat(60), "-lead"] {
        assert!(
            generate(&load_fixture_specs(), bad, "7.1", &source(), &test_config()).is_err(),
            "{bad:?}"
        );
    }
}

#[test]
fn required_override_must_exist() {
    let mut conf = test_config();
    conf.required_params = nested(&[("alpha", &[("Widgets_List", vec!["nope".to_string()])])]);
    let e = generate(&load_fixture_specs(), ORG, "7.1", &source(), &conf).unwrap_err();
    assert!(e.0.contains("does not have"), "{e}");
}

#[test]
fn name_clash_needs_override() {
    let entry = |op: &str| Entry {
        area: "a".into(),
        operation_id: op.into(),
        method_name: None,
        scopes: vec![],
        name: String::new(),
        tool: Map::new(),
    };
    let mut entries = vec![entry("X_Get Thing"), entry("Y_Get Thing")];
    let e = assign_names("m", &mut entries, &Config::default()).unwrap_err();
    assert!(e.0.contains("add one of them"), "{e}");
}

#[test]
fn minimal() {
    let scopes: BTreeSet<String> = [
        "vso.code",
        "vso.code_write",
        "vso.work",
        "vso.build_execute",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(
        minimal_scopes(&scopes),
        ["vso.build_execute", "vso.code_write", "vso.work"]
    );
}

#[test]
fn text_helpers_follow_python() {
    assert_eq!(snake("GetTeamDaysOff"), "get_team_days_off");
    assert_eq!(snake("Resultsgroup Details"), "resultsgroup_details");
    assert_eq!(one_line("  a \n\t b\u{a0}c  ", 400), "a b c");
    assert_eq!(
        one_line("a\u{1c}b", 400),
        "a b",
        "Python counts U+001C as whitespace"
    );
    assert_eq!(one_line("alpha beta gamma", 12), "alpha...");
    assert_eq!(
        one_line("ééééé ééééé", 8),
        "ééééé...",
        "the limit counts characters, not bytes"
    );
    assert_eq!(
        crate::text::compact_ascii(&json!({"a": "é😀"})),
        r#"{"a":"\u00e9\ud83d\ude00"}"#
    );
}

// Archive

type Calls = Rc<RefCell<Vec<(String, Vec<(String, String)>)>>>;

fn fake(calls: Calls, mut answer: impl FnMut(&str) -> Response + 'static) -> Transport {
    Box::new(move |req: &Request| {
        calls
            .borrow_mut()
            .push((req.url.clone(), req.headers.clone()));
        let path = req.url.split("/api/1").nth(1).unwrap_or("").to_string();
        Ok(answer(&path))
    })
}

fn ok(body: &[u8], headers: &[(&str, &str)]) -> Response {
    Response {
        status: 200,
        headers: headers
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.to_string()))
            .collect(),
        body: body.to_vec(),
    }
}

#[test]
fn blob_hash() {
    assert_eq!(
        archive::git_blob_sha1(b"hello\n"),
        "ce013625030ba8dba906f756967f9e9ca394464a"
    );
}

#[test]
fn fetch_verifies_and_caches() {
    let tmp = tempfile::tempdir().unwrap();
    let data: &[u8] = b"\xef\xbb\xbf{\"swagger\": \"2.0\"}";
    let pin = json!({"file": "x.json", "sha1_git": archive::git_blob_sha1(data)});
    let calls: Calls = Rc::default();
    let mut a = Archive::new(
        archive::API,
        fake(calls.clone(), move |_| {
            ok(data, &[("X-RateLimit-Remaining", "7")])
        }),
    );
    assert_eq!(archive::fetch_file(&mut a, &pin, tmp.path()).unwrap(), data);
    assert_eq!(archive::fetch_file(&mut a, &pin, tmp.path()).unwrap(), data);
    assert_eq!(calls.borrow().len(), 1, "second read comes from the cache");
    let headers = &calls.borrow()[0].1;
    assert!(headers
        .iter()
        .any(|(k, v)| k == "User-Agent" && v.contains("azdo-manuals/")));
    assert_eq!(a.remaining.as_deref(), Some("7"));
    assert_eq!(archive::load_spec(data).unwrap(), json!({"swagger": "2.0"}));
}

#[test]
fn fetch_refuses_wrong_content() {
    let tmp = tempfile::tempdir().unwrap();
    let pin = json!({"file": "x.json", "sha1_git": archive::git_blob_sha1(b"expected")});
    let mut a = Archive::new(archive::API, fake(Rc::default(), |_| ok(b"tampered", &[])));
    let e = archive::fetch_file(&mut a, &pin, tmp.path()).unwrap_err();
    assert!(e.0.contains("pinned"), "{e}");
    assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);
}

#[test]
fn corrupt_cache_is_replaced() {
    let tmp = tempfile::tempdir().unwrap();
    let data: &[u8] = b"{}";
    let sha = archive::git_blob_sha1(data);
    fs::write(tmp.path().join(format!("{sha}.json")), b"corrupt").unwrap();
    let pin = json!({"file": "x.json", "sha1_git": sha});
    let mut a = Archive::new(archive::API, fake(Rc::default(), move |_| ok(data, &[])));
    assert_eq!(archive::fetch_file(&mut a, &pin, tmp.path()).unwrap(), data);
}

#[test]
fn rate_limit_message() {
    let limited = |_: &str| Response {
        status: 429,
        headers: vec![("x-ratelimit-reset".into(), "x".into())],
        body: vec![],
    };
    let mut a = Archive::new(archive::API, fake(Rc::default(), limited));
    let e = a.get_content(&"0".repeat(40)).unwrap_err();
    assert!(
        e.0.contains("rate limit") && e.0.ends_with("try again after later"),
        "{e}"
    );
}

#[test]
fn discover_pins_resumes_and_records_missing() {
    let listings: HashMap<&str, Value> = HashMap::from([
        (
            "/revision/r1/",
            json!({"directory": "root", "date": "d", "message": "m\nmore"}),
        ),
        (
            "/directory/root/specification/",
            json!({"type": "dir", "target": "spec"}),
        ),
        (
            "/directory/spec/",
            json!([{"name": "a1", "type": "dir", "target": "A1"}, {"name": "a2", "type": "dir", "target": "A2"}]),
        ),
        (
            "/directory/A1/",
            json!([{"name": "7.1", "type": "dir", "target": "A1v"}]),
        ),
        (
            "/directory/A1v/",
            json!([{"name": "a1.json", "type": "file", "target": "h1", "length": 3},
                   {"name": "httpExamples", "type": "dir", "target": "ex"}]),
        ),
        (
            "/directory/A2/",
            json!([{"name": "7.2", "type": "dir", "target": "A2v"}]),
        ),
    ]);
    let answer = move |path: &str| ok(listings[path].to_string().as_bytes(), &[]);
    let calls: Calls = Rc::default();
    let mut a = Archive::new(archive::API, fake(calls.clone(), answer.clone()));
    let areas = vec!["a1".to_string(), "a2".to_string()];
    let mut saved = Vec::new();
    let result = archive::discover_pins(&mut a, "r1", "7.1", &areas, None, &mut |_| {}, &mut |r| {
        saved.push(r.to_string());
        Ok(())
    })
    .unwrap();
    assert_eq!(
        result["areas"],
        json!({"a1": [{"file": "a1.json", "sha1_git": "h1", "length": 3}]})
    );
    assert_eq!(result["missing"], json!({"a2": ["7.2"]}));
    assert_eq!(result["revision_message"], "m");
    assert_eq!(saved.len(), 2);
    calls.borrow_mut().clear();
    let mut a = Archive::new(archive::API, fake(calls.clone(), answer));
    archive::discover_pins(
        &mut a,
        "r1",
        "7.1",
        &areas,
        Some(result),
        &mut |_| {},
        &mut |_| Ok(()),
    )
    .unwrap();
    assert!(
        calls.borrow().is_empty(),
        "a complete earlier result needs no requests"
    );
}

#[test]
fn shipped_pins_parse() {
    let pins = archive::load_pins(None).unwrap();
    assert_eq!(pins["revision"], crate::config::ARCHIVE_REVISION);
    assert_eq!(pins["api_version"], crate::config::API_VERSION);
    for area in Config::real().all_areas() {
        assert!(
            pins["areas"].get(&area).is_some() || pins["missing"].get(&area).is_some(),
            "{area}"
        );
    }
}

// Route check

fn route_lists() -> HashMap<&'static str, Vec<Value>> {
    let r = |area: &str, res: &str, tpl: &str, released: &str, rv: u64| {
        json!({"area": area, "resourceName": res, "routeTemplate": tpl, "minVersion": "1.0", "maxVersion": "7.2",
               "releasedVersion": released, "resourceVersion": rv})
    };
    HashMap::from([
        (
            "https://dev.azure.com/contoso",
            vec![
                r(
                    "alpha",
                    "widgets",
                    "{project}/_apis/{area}/{resource}/{widgetId}",
                    "7.1",
                    2,
                ),
                r(
                    "alpha",
                    "query",
                    "{project}/{team}/_apis/{area}/{resource}",
                    "7.1",
                    1,
                ),
                r(
                    "alpha",
                    "things",
                    "{project}/_apis/{area}/{resource}/${type}",
                    "7.1",
                    1,
                ),
                r("alpha", "logs", "_apis/{area}/{resource}/{logId}", "7.0", 1),
            ],
        ),
        (
            "https://vsbeta.dev.azure.com/contoso",
            vec![r("beta", "widgets", "_apis/{area}/{resource}", "7.1", 1)],
        ),
        ("https://vsbeta.dev.azure.com", vec![]),
    ])
}

#[test]
fn route_check() {
    let (manuals, _) = run_generate();
    let lists = route_lists();
    let result = routes::check(&manuals, ORG, &mut |base| Ok(lists[base].clone()));
    let bases: Vec<(String, usize)> = result.bases.into_iter().collect();
    assert_eq!(
        bases,
        [
            ("https://dev.azure.com/<org>".to_string(), 4),
            ("https://vsbeta.dev.azure.com/<org>".to_string(), 1)
        ]
    );
    // Widgets list/create/get/batch (project is optional), query, things, beta widgets match; logs is asked at
    // 7.1 but released only to 7.0.
    assert_eq!(result.matched, 7);
    let mut unmatched = result.unmatched;
    unmatched.sort();
    assert_eq!(unmatched, ["t_main.list_threads", "t_main.query_subjects"]);
}

#[test]
fn unreadable_route_list() {
    let (manuals, _) = run_generate();
    let lists = route_lists();
    let result = routes::check(&manuals, ORG, &mut |base| {
        if base.contains("vsbeta") {
            Err(format!("OPTIONS {base}/_apis failed: HTTP 401"))
        } else {
            Ok(lists[base].clone())
        }
    });
    let mut unchecked = result.unchecked;
    unchecked.sort();
    assert_eq!(
        unchecked,
        [
            "t_main.beta_list_widgets",
            "t_main.list_threads",
            "t_main.query_subjects"
        ]
    );
    let errors: Vec<(String, String)> = result.base_errors.into_iter().collect();
    assert_eq!(
        errors,
        [(
            "https://vsbeta.dev.azure.com/<org>".to_string(),
            "OPTIONS https://vsbeta.dev.azure.com/<org>/_apis failed: HTTP 401".to_string()
        )]
    );
    assert_eq!(result.version.len(), 1);
    assert!(
        result.version[0].contains("t_main.get_log (7.1): not released at 7.1"),
        "{:?}",
        result.version
    );
}

#[test]
fn versions() {
    let route = json!({"minVersion": "4.0", "maxVersion": "7.2", "releasedVersion": "7.1", "resourceVersion": 2});
    assert_eq!(routes::version_problem("7.1", &route), "");
    assert_eq!(routes::version_problem("7.1-preview.2", &route), "");
    assert!(routes::version_problem("7.1-preview.3", &route).contains("resourceVersion"));
    assert!(routes::version_problem("7.2", &route).contains("needs -preview"));
    assert!(routes::version_problem("3.0", &route).contains("minVersion"));
}

#[test]
fn literal_does_not_match_parameter() {
    let route = routes::route_segments(
        &json!({"area": "a", "resourceName": "r", "routeTemplate": "_apis/{area}/{resource}"}),
    );
    let segs = |s: &[&str]| -> Vec<String> { s.iter().map(|x| x.to_string()).collect() };
    assert!(routes::segments_match(&route, &segs(&["_apis", "a", "r"])));
    assert!(!routes::segments_match(&route, &segs(&["_apis", "a", "*"])));
    assert!(!routes::segments_match(
        &route,
        &segs(&["_apis", "a", "r", "extra"])
    ));
}

// Read test helpers

#[test]
fn resolve_references() {
    let responses = HashMap::from([(
        "list_things".to_string(),
        json!({"count": 2, "value": [{"id": 7, "name": "a"}, {"id": 8}]}),
    )]);
    assert_eq!(
        readtest::resolve(&json!("@list_things:value.1.id"), &responses).unwrap(),
        json!(8)
    );
    assert_eq!(
        readtest::resolve(&json!({"ids": ["@list_things:value.0.id"]}), &responses).unwrap(),
        json!({"ids": [7]})
    );
    assert_eq!(
        readtest::resolve(&json!("x/{@list_things:value.0.name}/y"), &responses).unwrap(),
        json!("x/a/y")
    );
    assert_eq!(
        readtest::resolve(&json!(true), &responses).unwrap(),
        json!(true)
    );
    for missing in [
        "@other:id",
        "@list_things:value.5.id",
        "@list_things:value.1.name",
    ] {
        assert!(
            readtest::resolve(&json!(missing), &responses).is_err(),
            "{missing}"
        );
    }
}

#[test]
fn first_item_and_value() {
    assert_eq!(
        readtest::first_item(&json!({"count": 1, "value": [{"id": 1}]})),
        Some(&json!({"id": 1}))
    );
    assert_eq!(
        readtest::first_item(&json!({"count": 0, "value": []})),
        None
    );
    assert_eq!(
        readtest::first_item(&json!({"id": 3})),
        Some(&json!({"id": 3}))
    );
    assert_eq!(
        readtest::value_for("pullRequestId", Some(&json!({"pullRequestId": 5, "id": 9}))),
        Some(json!(5))
    );
    assert_eq!(
        readtest::value_for("repositoryId", Some(&json!({"id": "r", "name": "n"}))),
        Some(json!("r"))
    );
}

#[test]
fn fill_chains_ids() {
    let (manuals, _) = run_generate();
    let tools = tools_by_name(&manuals);
    let mut seeds = Map::new();
    seeds.insert("project".into(), json!("P"));
    let mut test = ReadTest::new(PathBuf::from("utcp"), IndexMap::new(), seeds).unwrap();
    assert_eq!(
        test.fill(&tools["get_widget_by_id"]),
        Err("no value for {widgetId}".to_string())
    );
    let list = readtest::normalize(&readtest::template_of(&tools["alpha_list_widgets"]));
    test.items.insert(list, json!({"id": "w1"}));
    let args = test.fill(&tools["get_widget_by_id"]).unwrap();
    assert_eq!(
        Value::Object(args),
        json!({"project": "P", "widgetId": "w1"})
    );
    assert_eq!(
        test.fill(&tools["alpha_list_widgets"]),
        Err("required parameter kind".to_string())
    );
    test.set_seed("kind", json!("round"));
    let args = test.fill(&tools["alpha_list_widgets"]).unwrap();
    assert_eq!(
        Value::Object(args),
        json!({"project": "P", "kind": "round", "$top": 5})
    );
    assert_eq!(
        test.fill(&tools["run_query"]),
        Err("no sample body".to_string())
    );
}

// utcp load test

fn find_utcp() -> Option<PathBuf> {
    let path = std::env::var_os("UTCP_BIN")
        .map(PathBuf::from)
        .or_else(|| which("utcp"));
    let found = path.clone().filter(|p| p.is_file());
    if found.is_none() && std::env::var_os("REQUIRE_UTCP").is_some() {
        panic!("REQUIRE_UTCP is set but no usable utcp binary was found (UTCP_BIN={path:?})");
    }
    found
}

/// The expected manuals register in a throwaway utcp, and dry runs build the right requests.
#[test]
fn utcp_register_schema_dry_run() {
    let Some(utcp) = find_utcp() else {
        eprintln!("skipped: no utcp binary (set UTCP_BIN)");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let run = |args: &[&str], stdin: Option<&str>| -> String {
        use std::io::Write;
        use std::process::Stdio;
        let mut cmd = Command::new(&utcp);
        cmd.args(args)
            .arg("--direct")
            .env("AZURE_DEVOPS_EXT_PAT", "not-a-real-token");
        for name in ["config", "cache", "state"] {
            cmd.env(
                format!("UTCP_{}_DIR", name.to_uppercase()),
                tmp.path().join(name),
            );
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.unwrap_or("").as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "utcp {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    let file = expected_dir().join("t_main.utcp.json");
    run(&["register", "t_main", file.to_str().unwrap()], None);
    let manual: Value = serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
    for tool in manual["tools"].as_array().unwrap() {
        let name = format!("t_main.{}", tool["name"].as_str().unwrap());
        let schema: Value = serde_json::from_str(&run(&["schema", &name], None)).unwrap();
        assert_eq!(schema["inputs"], tool["inputs"], "{name}");
    }
    let out: Value = serde_json::from_str(&run(
        &["call", "t_main.run_query", "--dry-run", "--args-stdin"],
        Some(r#"{"project": "My Project", "body": {"text": "x"}, "$top": 2}"#),
    ))
    .unwrap();
    assert_eq!(
        out["url"],
        "https://dev.azure.com/contoso/My%20Project/_apis/alpha/query?api-version=7.1"
    );
    assert_eq!(out["method"], "POST");
    assert!(!out.to_string().contains("not-a-real-token"));
    let out: Value = serde_json::from_str(&run(
        &[
            "call",
            "t_main.get_thing_template",
            "--dry-run",
            "--args-stdin",
        ],
        Some(r#"{"project": "P", "type": "Bug"}"#),
    ))
    .unwrap();
    assert_eq!(
        out["url"],
        "https://dev.azure.com/contoso/P/_apis/alpha/things/%24Bug?api-version=7.1"
    );
}
