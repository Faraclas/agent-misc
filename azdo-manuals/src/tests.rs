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
use crate::cli::{help, help_alias, next_steps, permission_lines, scopes_markdown, which};
use crate::config::{list_map, manual, nested, texts, Config};
use crate::generate::{
    api_version_of, assign_names, base_name, can_leave_out, covering_scopes, generate,
    minimal_scopes, placeholders_named_as_inputs, tool_description, Entry, Specs,
};
use crate::http::{Request, Response, Transport};
use crate::readtest::{self, ReadTest};
use crate::routes;
use crate::text::{
    first_sentence_len, name_sentence, one_line, snake, write_json, MAX_DESCRIPTION,
};
use crate::utcp::Utcp;

const ORG: &str = "contoso";

pub(crate) fn fixtures() -> PathBuf {
    if let Some(dir) = std::env::var_os("AZDO_TEST_FIXTURES") {
        return PathBuf::from(dir);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

pub(crate) fn expected_dir() -> PathBuf {
    fixtures().join("expected")
}

pub(crate) fn source() -> Value {
    json!({"revision": "0".repeat(40), "revision_date": "2026-01-01T00:00:00+00:00", "revision_message": "test"})
}

pub(crate) fn test_config() -> Config {
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
        excluded_areas: vec![("delta".to_string(), "test exclusion".to_string())],
        post_reads: list_map(&[
            ("alpha", &["Query_Run Query"]),
            ("beta", &["Subject Query_Query"]),
        ]),
        tested_writes: list_map(&[("alpha", &["Widgets_Create"])]),
        deletes: HashMap::new(),
        read_only: false,
        skip_gets: nested(&[("beta", &[("Profiles_Get", "test skip".to_string())])]),
        required_params: nested(&[("alpha", &[("Widgets_List", vec!["kind".to_string()])])]),
        names: nested(&[(
            "beta",
            &[("Subject Query_Query", "query_subjects".to_string())],
        )]),
        body_overrides: HashMap::new(),
        host_overrides: HashMap::new(),
        route_areas: vec![],
        excluded_route_areas: vec![],
        summaries: texts(&[("beta", "Threads_List", "List the discussion threads.")]),
        notes: texts(&[("alpha", "Widgets_Update", "Send only the fields to change.")]),
    }
}

pub(crate) fn load_fixture_specs() -> Specs {
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

pub(crate) fn tools_by_name(manuals: &IndexMap<String, Value>) -> HashMap<String, Value> {
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
        ("Widgets_Delete", "DELETE not selected"),
        ("Widgets_Exists", "HEAD never included"),
        ("Profiles_Get", "skipped: test skip"),
    ] {
        assert!(
            reasons.contains(&(op.to_string(), reason.to_string())),
            "{op}: {reason}"
        );
    }
    // A file answer and a file upload are tools: utcp saves the answer and reads the file to send.
    let zip = &tools["get_zip"];
    assert_eq!(zip["tool_call_template"]["response_type"], "file");
    assert_eq!(
        zip["tool_call_template"]["headers"]["Accept"],
        "application/zip"
    );
    assert!(zip["outputs"]["description"]
        .as_str()
        .unwrap()
        .contains("\"type\": \"file\""));
    let upload = &tools["upload_widget"];
    assert_eq!(upload["tool_call_template"]["body_type"], "file");
    assert_eq!(upload["tool_call_template"]["body_field"], "body");
    assert_eq!(
        upload["tool_call_template"]["headers"]["Content-Type"],
        "application/octet-stream"
    );
    assert_eq!(
        upload["inputs"]["properties"]["body"]["required"],
        json!(["file"])
    );
    assert!(upload["tool_call_template"].get("response_type").is_none());
    // Header parameters the spec names differently from the header they are.
    assert_eq!(
        upload["tool_call_template"]["header_fields"],
        json!(["Content-Range", "If-Match"])
    );
    let required = upload["inputs"]["required"].as_array().unwrap();
    assert!(required.contains(&json!("Content-Range")));
    assert!(
        !required.contains(&json!("If-Match")),
        "a create has no version to match"
    );
    assert!(!tools.contains_key("get_profile"));
    // `project` is `x-ms-required: false`: optional, with what leaving it out means.
    assert_eq!(tools["list_widgets"]["inputs"]["required"], json!(["kind"]));
    assert_eq!(
        tools["list_widgets"]["inputs"]["properties"]["project"]["description"],
        "Project ID or project name. Optional: leave it out for the whole organization instead of one project."
    );
    // Same operationId in two areas of one manual: the manual's first area keeps the name, the other gets its prefix.
    assert!(tools.contains_key("list_widgets"));
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
    // An optional `{team}` stays in the URL; utcp leaves its segment out when it is not passed.
    assert_eq!(
        url("run_query"),
        "https://dev.azure.com/contoso/{project}/{team}/_apis/alpha/query?api-version=7.1"
    );
    let query_inputs = &tools["run_query"]["inputs"];
    assert!(!query_inputs["required"]
        .as_array()
        .unwrap()
        .contains(&json!("team")));
    assert!(query_inputs["properties"]["team"]["description"]
        .as_str()
        .unwrap()
        .ends_with("Optional: leave it out for the project's default team."));
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
    conf.summaries = texts(&[("alpha", "Widgets_Lsit", "List widgets.")]);
    assert_eq!(
        generate_err(&conf),
        "config summaries names alpha/Widgets_Lsit, which the specs do not have"
    );
    let mut conf = test_config();
    conf.notes = texts(&[("beta", "Nope_Get", "x.")]);
    assert_eq!(
        generate_err(&conf),
        "config notes names beta/Nope_Get, which the specs do not have"
    );
    let mut conf = test_config();
    conf.tested_writes = list_map(&[("alpha", &["Widgets_Craete"])]);
    assert_eq!(
        generate_err(&conf),
        "config tested_writes names alpha/Widgets_Craete, which the specs do not have"
    );
    let mut conf = test_config();
    conf.deletes = list_map(&[("alpha", &["Widgets_Remove"])]);
    assert_eq!(
        generate_err(&conf),
        "config deletes names alpha/Widgets_Remove, which the specs do not have"
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
    conf.tested_writes = list_map(&[("gamma", &["Anything_Create"])]);
    run_generate_with(&conf);
}

#[test]
fn read_only_leaves_out_writes() {
    let (manuals, report) = run_generate();
    assert!(tools_by_name(&manuals).contains_key("create_widget"));
    assert_eq!(report["manuals"]["t_main"]["writes"], 3);
    assert_eq!(report["manuals"]["t_main"]["untested_writes"], 2);
    let (manuals, report) = run_generate_with(&test_config().read_only());
    assert!(!tools_by_name(&manuals).contains_key("create_widget"));
    assert_eq!(report["manuals"]["t_main"]["writes"], 0);
    assert!(left_out_reasons(&report).contains(&(
        "Widgets_Create".to_string(),
        "write (--read-only)".to_string()
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
        .tested_writes
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
    assert!(named.contains(&("tested_writes", "git", "Pull Requests_Create")));
    assert!(named.contains(&("deletes", "wit", "Comments_Delete")));
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
        json!([
            "vso.alpha",
            "vso.alpha_manage",
            "vso.alpha_write",
            "vso.beta",
            "vso.beta_full",
            "vso.beta_manage"
        ])
    );
    // Threads_List accepts vso.beta or vso.beta_full: the read scope covers it, no write scope is asked for. Pools_List
    // needs vso.beta_manage, which covers vso.beta.
    let ids = |key: &str| -> Vec<String> {
        report[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["scope"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(ids("scopes"), ["vso.alpha_write", "vso.beta_manage"]);
    // A read-only PAT: the reads that a read scope allows (not Pools_List).
    assert_eq!(ids("read_scopes"), ["vso.alpha", "vso.beta"]);
    // Each manual lists only the broadest level of each permission it needs.
    for (name, m) in report["manuals"].as_object().unwrap() {
        let scopes: Vec<String> = serde_json::from_value(m["scopes"].clone()).unwrap();
        assert!(!scopes.is_empty(), "{name} lists no scopes");
        assert_eq!(
            crate::generate::minimal_scopes(&scopes.iter().cloned().collect()),
            scopes,
            "{name} lists a level that a broader one already covers"
        );
    }
    assert_eq!(
        report["scopes"][0]["description"],
        "Read and write alpha things."
    );
    let md = scopes_markdown(&report, ORG);
    assert!(md.contains("| (not known) | `vso.alpha_write` | Read and write alpha things. |"));
    assert!(md.contains("https://dev.azure.com/contoso/_usersSettings/tokens"));
    assert!(md.contains(
        "For a PAT that only reads (the write tools then answer 401): vso.alpha, vso.beta."
    ));
}

#[test]
fn pat_names_as_on_the_token_page() {
    assert_eq!(crate::pat::label("vso.code_write"), "Code: Read & write");
    assert_eq!(crate::pat::label("vso.alpha"), "vso.alpha");
    assert_eq!(
        crate::pat::rows(["vso.work_write", "vso.agentpools", "vso.alpha"]),
        [
            ("Agent Pools".to_string(), "Read".to_string()),
            ("Work Items".to_string(), "Read & write".to_string()),
            ("vso.alpha".to_string(), String::new()),
        ]
    );
    assert_eq!(
        permission_lines(["vso.code_write", "vso.agentpools"], None),
        ["  Agent Pools  Read", "  Code         Read & write"]
    );
    // Same permission name: one row. A broader level of a family replaces the narrower one before this.
    assert_eq!(
        crate::pat::rows(["vso.code_write", "vso.code_status"]),
        [("Code".to_string(), "Read & write + Status".to_string())]
    );
    assert_eq!(
        permission_lines(
            ["vso.code_write", "vso.security_manage"],
            Some(vec!["vso.code"])
        ),
        [
            "  Permission  All tools     Reads only",
            "  Code        Read & write  Read",
            "  Security    Manage        -",
        ]
    );
}

#[test]
fn help_shows_getting_started() {
    let main = help(None);
    assert_eq!(main.kind(), clap::error::ErrorKind::DisplayHelp);
    let text = main.render().to_string();
    assert!(text.contains("Getting started"));
    assert!(text.contains("azdo-manuals generate --org <your-org> --stage"));
    assert!(text.contains("utcp promote 'azdo_*'"));
    assert!(text.contains("azdo-manuals help generate"));
    assert!(!text.contains("subcommand"));
    assert!(!text.contains("--register"));
    let generate = help(Some("generate")).render().to_string();
    assert!(generate.contains("--stage"));
    assert!(generate.contains("With a PAT in AZURE_DEVOPS_EXT_PAT"));
    assert!(generate.contains("--spec-only"));
    assert!(!generate.contains("Getting started"));
    let read_test = help(Some("read-test")).render().to_string();
    assert!(
        read_test.contains("no PAT\nis needed in this shell"),
        "{read_test}"
    );
    assert!(read_test.contains("utcp call --staged"));
    let write_test = help(Some("write-test")).render().to_string();
    assert!(write_test.contains("throwaway utcp setup"));
    assert!(write_test.contains("AZURE_DEVOPS_EXT_PAT"));
    let unknown = help(Some("nonsense"));
    assert_ne!(unknown.kind(), clap::error::ErrorKind::DisplayHelp);
    assert!(unknown.to_string().contains("unknown command 'nonsense'"));
}

#[test]
fn next_steps_cover_the_pat_review_test_and_promote() {
    let steps = next_steps("contoso", None);
    assert_eq!(steps.len(), 6);
    assert!(steps[0].contains("https://dev.azure.com/contoso/_usersSettings/tokens"));
    assert!(steps[1].contains("utcp doctor: \"Secrets File\""));
    assert!(steps[1].contains("add AZURE_DEVOPS_EXT_PAT to it)\n"));
    assert!(steps[1].ends_with("- either way, then run: utcp daemon reload-env"));
    assert!(steps[2].contains("[SET] AZURE_DEVOPS_EXT_PAT"));
    assert_eq!(
        steps[3],
        "See what changes for your agents: utcp diff 'azdo_*'"
    );
    assert!(steps[4].contains("azdo-manuals read-test --org contoso --project <a project>"));
    assert_eq!(
        steps[5],
        "Make them active for your agents: utcp promote 'azdo_*'"
    );
    let custom = next_steps("contoso", Some(Path::new("manuals-here")));
    assert!(custom[4].contains("read-test --manuals manuals-here --project"));
    for step in steps {
        for line in step.lines() {
            assert!(line.len() + 5 <= 120, "too long: {line}");
        }
    }
}

/// The release version is the REST API version plus a counter: 7.1.x for API 7.1.
#[test]
fn version_starts_with_the_api_version() {
    let version = env!("CARGO_PKG_VERSION");
    assert!(
        version.starts_with(&format!("{}.", crate::config::API_VERSION)),
        "version {version} does not start with API version {}",
        crate::config::API_VERSION
    );
    assert_eq!(version.split('.').count(), 3, "{version}");
}

#[test]
fn command_help_alias() {
    let os = |v: &[&str]| v.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
    assert_eq!(
        help_alias(os(&["azdo-manuals", "generate", "help"])),
        os(&["azdo-manuals", "help", "generate"])
    );
    assert_eq!(
        help_alias(os(&["azdo-manuals", "nonsense", "help"])),
        os(&["azdo-manuals", "nonsense", "help"])
    );
    assert_eq!(
        help_alias(os(&["azdo-manuals", "generate", "--org", "help"])),
        os(&["azdo-manuals", "generate", "--org", "help"])
    );
}

#[test]
fn read_needing_write_scope() {
    let (manuals, report) = run_generate();
    assert!(tools_by_name(&manuals).contains_key("list_pools"));
    assert!(report["scopes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["scope"] == "vso.beta_manage"));
    let (manuals, report) = run_generate_with(&test_config().read_only());
    assert!(!tools_by_name(&manuals).contains_key("list_pools"));
    assert!(left_out_reasons(&report).contains(&(
        "Pools_List".to_string(),
        "read needs a write scope: vso.beta_manage".to_string()
    )));
}

#[test]
fn untested_writes_are_marked() {
    let (manuals, _) = run_generate();
    let tools = tools_by_name(&manuals);
    let update = &tools["update_widget"];
    assert_eq!(
        update["description"],
        "(untested) Updates a widget. Send only the fields to change."
    );
    assert_eq!(
        update["tags"],
        json!(["azure-devops", "alpha", "widgets", "untested"])
    );
    // The tested write and the POST that only reads are not marked.
    for name in ["create_widget", "run_query"] {
        assert!(
            !tools[name]["tags"]
                .as_array()
                .unwrap()
                .contains(&json!("untested")),
            "{name}"
        );
    }
    // A JSON Patch body is an array of operations, whatever the spec says.
    let body = &update["inputs"]["properties"]["body"];
    assert_eq!(body, &crate::bodies::json_patch_document());
    assert_eq!(
        update["tool_call_template"]["headers"]["Content-Type"],
        "application/json-patch+json"
    );
}

#[test]
fn deletes_only_when_listed() {
    let mut conf = test_config();
    conf.deletes = list_map(&[("alpha", &["Widgets_Delete"])]);
    let (manuals, report) = run_generate_with(&conf);
    let tools = tools_by_name(&manuals);
    assert_eq!(
        tools["delete_widget"]["tool_call_template"]["http_method"],
        "DELETE"
    );
    assert!(tools["delete_widget"]["description"]
        .as_str()
        .unwrap()
        .starts_with("(untested) "));
    assert!(!left_out_reasons(&report)
        .iter()
        .any(|(op, _)| op == "Widgets_Delete"));
    let (manuals, _) = run_generate_with(&conf.read_only());
    assert!(!tools_by_name(&manuals).contains_key("delete_widget"));
}

#[test]
fn no_pat_outside_the_organization() {
    let mut conf = test_config();
    conf.skip_gets.clear();
    let (manuals, _) = run_generate_with(&conf);
    let tools = tools_by_name(&manuals);
    let profile = &tools["get_profile"]["tool_call_template"];
    assert_eq!(
        profile["url"],
        "https://vsbeta.dev.azure.com/_apis/beta/profiles/{id}?api-version=7.1"
    );
    assert!(profile.get("auth").is_none(), "{profile}");
    assert!(tools["beta_list_widgets"]["tool_call_template"]
        .get("auth")
        .is_some());
}

#[test]
fn hints_name_the_organization_when_all_fit() {
    let (_, report) = run_generate();
    assert_eq!(
        report["manuals"]["t_main"]["hint"],
        "test main, org contoso"
    );

    let conf = crate::config::Config::real();
    let hints = crate::generate::manual_hints(&conf, ORG);
    assert_eq!(hints["azdo_git"], "Azure DevOps repos and PRs, org contoso");
    assert!(hints.values().all(|h| h.chars().count() <= 60), "{hints:?}");

    // An org name long enough to push one hint past utcp's limit leaves the org out of all of them.
    let long = "a".repeat(20);
    let hints = crate::generate::manual_hints(&conf, &long);
    for m in &conf.manuals {
        assert_eq!(hints[&m.name], m.hint);
    }
}

#[test]
fn parameterized_host() {
    let mut specs = load_fixture_specs();
    let beta = &mut specs["beta"][0];
    beta["host"] = json!("{service}dev.azure.com");
    beta["x-ms-parameterized-host"] = json!({"hostTemplate": "{service}dev.azure.com",
        "parameters": [{"name": "service", "required": false, "type": "string", "in": "path"}]});
    let (manuals, _) = generate(&specs, ORG, "7.1", &source(), &test_config()).unwrap();
    let url = tools_by_name(&manuals)["beta_list_widgets"]["tool_call_template"]["url"].clone();
    assert!(
        url.as_str()
            .unwrap()
            .starts_with("https://dev.azure.com/contoso/"),
        "{url}"
    );
    specs["beta"][0]["x-ms-parameterized-host"]["parameters"][0]["required"] = json!(true);
    assert!(generate(&specs, ORG, "7.1", &source(), &test_config()).is_err());
}

#[test]
fn host_override_adds_the_organization() {
    let mut specs = load_fixture_specs();
    let beta = &mut specs["beta"][0];
    beta["host"] = json!("app.vsbeta.visualstudio.com");
    let paths = beta["paths"].as_object_mut().unwrap();
    let moved: Map<String, Value> = std::mem::take(paths)
        .into_iter()
        .map(|(p, v)| (p.replacen("/{organization}", "", 1), v))
        .collect();
    *paths = moved;
    let mut conf = test_config();
    conf.host_overrides = HashMap::from([("beta".to_string(), "vsbeta.dev.azure.com".to_string())]);
    let (manuals, _) = generate(&specs, ORG, "7.1", &source(), &conf).unwrap();
    let tool = tools_by_name(&manuals)["beta_list_widgets"].clone();
    let url = tool["tool_call_template"]["url"].clone();
    assert!(
        url.as_str()
            .unwrap()
            .starts_with("https://vsbeta.dev.azure.com/contoso/_apis/beta/widgets"),
        "{url}"
    );
    assert!(
        tool["tool_call_template"].get("auth").is_some(),
        "an operation moved under the organization is sent the PAT"
    );
}

#[test]
fn host_overrides_name_included_areas() {
    let conf = Config::real();
    let areas: BTreeSet<String> = conf.all_areas().into_iter().collect();
    for area in conf.host_overrides.keys() {
        assert!(
            areas.contains(area),
            "host_overrides names {area}, which no manual includes"
        );
    }
}

#[test]
fn left_out_summary() {
    let (_, report) = run_generate();
    let lines = crate::cli::left_out_lines(&report, &[("gamma".to_string(), "7.2".to_string())]);
    assert_eq!(
        lines,
        [
            "  area gamma           no 7.1 description (has 7.2)",
            "  area delta           test exclusion",
            "  1 DELETE operations  deleting cannot be undone (the small deletes are included)",
            "  1 HEAD operations    the GET of the same thing is included",
            "  1 operations that do not work:",
            "    beta Profiles_Get: test skip",
        ]
    );
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
fn description_without_text_is_a_sentence_from_the_tool_name() {
    let conf = test_config();
    let describe =
        |op: &str, method: Option<&str>| tool_description("", "alpha", op, method, &conf);
    // The client method has the words apart where the operation id glues them ("Roledefinitions").
    assert_eq!(
        describe("Roledefinitions_List", Some("GetRoleDefinitions")),
        "Get role definitions"
    );
    assert_eq!(
        describe("Oidctoken_Create", Some("CreateOidcToken")),
        "Create oidc token"
    );
    // .NET's Async suffix says nothing to an agent.
    assert_eq!(
        describe(
            "Npm_DownloadScopedPackage",
            Some("GetContentScopedPackageAsync")
        ),
        "Get content scoped package"
    );
    // Without a method, the operation id as the tool name takes it.
    assert_eq!(describe("Settings_Get", None), "Get setting");
    assert_eq!(describe("Archive_Get Zip", None), "Get zip");
    // A `names` entry wins, as it does for the tool's name.
    let mut named = test_config();
    named.names = nested(&[(
        "alpha",
        &[("Archive_Get Zip", "download_archive".to_string())],
    )]);
    assert_eq!(
        tool_description("   ", "alpha", "Archive_Get Zip", None, &named),
        "Download archive"
    );
    assert_eq!(name_sentence("async"), "Async");
    assert_eq!(name_sentence(""), "");

    // In the manual: no "Things: Get Thing Template" any more.
    let (manuals, _) = run_generate();
    let tools = tools_by_name(&manuals);
    assert_eq!(
        tools["get_thing_template"]["description"],
        "Get thing template"
    );
    assert_eq!(
        tools["upload_widget"]["description"],
        "(untested) Upload widget"
    );
}

#[test]
fn developer_comments_are_dropped() {
    let conf = test_config();
    let describe =
        |text: &str| tool_description(text, "alpha", "X_Get", Some("GetConnectionData"), &conf);
    assert_eq!(
        describe("This was copied and adapted from TeamFoundationConnectionService.Connect()"),
        "Get connection data"
    );
    assert_eq!(
        describe(
            "http://(tfsserver):8080/tfs/DefaultCollection/_apis/test/CodeCoverage?buildId=10 Request: Json of code \
             coverage summary"
        ),
        "Get connection data"
    );
    // Only the first sentence: what follows it stays.
    assert_eq!(
        describe("Ported from the old client. Returns who is calling."),
        "Returns who is calling."
    );
    // Words that only look alike.
    for kept in [
        "Gets the release definitions adapted for the stage.",
        "Copies a test plan. The new plan is ported from the old one.",
        "Gets a build. See https://example.com for more.",
    ] {
        assert_eq!(describe(kept), kept);
    }
}

#[test]
fn summaries_replace_the_first_sentence() {
    let mut conf = test_config();
    conf.summaries = texts(&[
        ("alpha", "Widgets_List", "List the widgets of a project."),
        ("alpha", "Widgets_Create", "Add a widget."),
    ]);
    let describe = |op: &str, text: &str| tool_description(text, "alpha", op, None, &conf);
    assert_eq!(
        describe("Widgets_List", "Lists\n widgets. Up to 100 at a time."),
        "List the widgets of a project. Up to 100 at a time."
    );
    // Also a sentence made from the name, and a developer comment.
    assert_eq!(describe("Widgets_Create", ""), "Add a widget.");
    assert_eq!(
        describe(
            "Widgets_Create",
            "Copied from WidgetService.Add(). Needs a name."
        ),
        "Add a widget. Needs a name."
    );
    assert_eq!(first_sentence_len("One. Two."), 4);
    assert_eq!(first_sentence_len("No end"), 6);

    let (manuals, _) = run_generate();
    let tools = tools_by_name(&manuals);
    assert_eq!(
        tools["list_threads"]["description"],
        "List the discussion threads."
    );
}

#[test]
fn notes_survive_shortening() {
    let note =
        "The authenticated user is the account the PAT belongs to: who am I, the current user, me.";
    let mut conf = test_config();
    conf.notes = texts(&[
        ("alpha", "Widgets_Get", note),
        ("alpha", "Widgets_List", note),
    ]);
    let describe = |op: &str, text: &str| tool_description(text, "alpha", op, None, &conf);
    let long = "Gets a widget with every one of its many details. ".repeat(20);
    let text = describe("Widgets_Get", &long);
    assert!(text.ends_with(&format!("... {note}")), "{text}");
    assert!(
        text.chars().count() <= MAX_DESCRIPTION,
        "{}",
        text.chars().count()
    );
    assert!(text.starts_with("Gets a widget with every one"));
    // A text without a full stop gets one before the note.
    assert_eq!(
        describe("Widgets_List", "Lists widgets"),
        format!("Lists widgets. {note}")
    );
    assert_eq!(
        describe("Widgets_List", ""),
        format!("List widgets. {note}")
    );
    // Without a note, a long text is cut as before.
    assert_eq!(
        describe("Widgets_Create", &long),
        one_line(&long, MAX_DESCRIPTION)
    );

    // In the manual, `(untested)` still comes first, and a long description keeps the note.
    let mut specs = load_fixture_specs();
    specs["alpha"][0]["paths"]["/{organization}/{project}/_apis/alpha/widgets/{widgetId}"]
        ["patch"]["description"] = json!(long);
    let (manuals, _) = generate(&specs, ORG, "7.1", &source(), &test_config()).unwrap();
    let update = tools_by_name(&manuals)["update_widget"]["description"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(update.starts_with("(untested) Gets a widget"), "{update}");
    assert!(
        update.ends_with("... Send only the fields to change."),
        "{update}"
    );
    assert!(update.chars().count() <= MAX_DESCRIPTION + "(untested) ".len());
}

#[test]
fn real_summaries_and_notes_are_short_sentences() {
    let conf = Config::real();
    for (setting, map) in [("summaries", &conf.summaries), ("notes", &conf.notes)] {
        for (area, ops) in map {
            for (op, text) in ops {
                assert!(
                    text.ends_with('.') && text.chars().count() <= 200 && text.trim() == text,
                    "{setting} {area}/{op}: {text:?}"
                );
            }
        }
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
        method_name: Some("GetThing".into()),
        scopes: vec![],
        read: true,
        source: None,
        scopes_inferred: false,
        name: String::new(),
        tool: Map::new(),
    };
    // Same area and group: the group prefix cannot tell them apart.
    let mut entries = vec![entry("X_Get Thing"), entry("X_Get Thing By Name")];
    let e = assign_names("m", &mut entries, &Config::default()).unwrap_err();
    assert!(e.0.contains("add one of them"), "{e}");
    // Different groups of one area: each gets its group (NuGet -> nuget).
    let mut entries = vec![entry("NuGet_Get Thing"), entry("Python Packages_Get Thing")];
    assign_names("m", &mut entries, &Config::default()).unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["nuget_get_thing", "python_packages_get_thing"]);
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

pub(crate) type Calls = Rc<RefCell<Vec<(String, Vec<(String, String)>)>>>;

pub(crate) fn fake(calls: Calls, mut answer: impl FnMut(&str) -> Response + 'static) -> Transport {
    Box::new(move |req: &Request| {
        calls
            .borrow_mut()
            .push((req.url.clone(), req.headers.clone()));
        let path = req.url.split("/api/1").nth(1).unwrap_or("").to_string();
        Ok(answer(&path))
    })
}

pub(crate) fn ok(body: &[u8], headers: &[(&str, &str)]) -> Response {
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

/// Every area of Microsoft's spec folder is in a manual or left out with a reason, so none is dropped silently.
#[test]
fn every_archive_area_is_accounted_for() {
    let pins = archive::load_pins(None).unwrap();
    let conf = Config::real();
    let mut known: BTreeSet<String> = conf.all_areas().into_iter().collect();
    for (area, reason) in &conf.excluded_areas {
        assert!(!reason.is_empty(), "{area}");
        assert!(
            known.insert(area.clone()),
            "{area} is both in a manual and excluded"
        );
    }
    let archive: BTreeSet<String> = pins["archive_areas"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap().to_string())
        .collect();
    assert_eq!(known, archive);
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
                r("alpha", "archive", "_apis/{area}/{resource}", "7.1", 1),
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
    let mut bases: Vec<(String, usize)> = result.bases.into_iter().collect();
    bases.sort();
    assert_eq!(
        bases,
        [
            ("https://dev.azure.com/<org>".to_string(), 5),
            ("https://vsbeta.dev.azure.com/<org>".to_string(), 1)
        ]
    );
    // Widgets list/create/get/update/upload/batch (project is optional), query, things, the archive (a file answer),
    // beta widgets match; logs is asked at 7.1 but released only to 7.0.
    assert_eq!(result.matched, 10);
    let mut unmatched = result.unmatched;
    unmatched.sort();
    assert_eq!(
        unmatched,
        [
            "t_main.list_pools",
            "t_main.list_threads",
            "t_main.query_subjects"
        ]
    );
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
            "t_main.list_pools",
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
fn host_without_route_list() {
    let (mut manuals, _) = run_generate();
    manuals["t_main"]["tools"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "name": "get_service_status",
            "tool_call_template": {
                "url": "https://status.dev.azure.com/_apis/status/health?api-version=7.1"
            }
        }));
    let lists = route_lists();
    let result = routes::check(&manuals, ORG, &mut |base| {
        assert!(
            !base.contains("status."),
            "fetched the route list of {base}"
        );
        Ok(lists[base].clone())
    });
    assert_eq!(result.no_route_list, ["t_main.get_service_status"]);
    assert_eq!(result.matched, 10);
    assert!(result.unchecked.is_empty(), "{:?}", result.unchecked);
    assert!(!result
        .unmatched
        .contains(&"t_main.get_service_status".to_string()));
    assert!(result.base_errors.is_empty());
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
    assert_eq!(
        readtest::resolve(&json!("@list_things:value.[name=a].id"), &responses).unwrap(),
        json!(7)
    );
    for missing in [
        "@other:id",
        "@list_things:value.5.id",
        "@list_things:value.1.name",
        "@list_things:value.[name=b].id",
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
    let process = json!({"typeId": "adcc42ab", "name": "Agile", "referenceName": null});
    assert_eq!(
        readtest::value_for("processId", Some(&process)),
        Some(json!("adcc42ab"))
    );
    assert_eq!(
        readtest::value_for("processTypeId", Some(&process)),
        Some(json!("adcc42ab"))
    );
    let wit = json!({"name": "Bug", "referenceName": "Microsoft.VSTS.WorkItemTypes.Bug"});
    assert_eq!(
        readtest::value_for("witRefName", Some(&wit)),
        Some(json!("Microsoft.VSTS.WorkItemTypes.Bug"))
    );
}

#[test]
fn fill_chains_ids() {
    let (manuals, _) = run_generate();
    let tools = tools_by_name(&manuals);
    let mut seeds = Map::new();
    seeds.insert("project".into(), json!("P"));
    let mut test = ReadTest::new(
        Utcp::shared(PathBuf::from("utcp")),
        IndexMap::new(),
        BTreeSet::new(),
        seeds,
    );
    assert_eq!(
        test.fill(&tools["get_widget_by_id"]),
        Err("no value for {widgetId}".to_string())
    );
    let list = readtest::normalize(&readtest::template_of(&tools["list_widgets"]));
    test.items.insert(list, json!({"id": "w1"}));
    let args = test.fill(&tools["get_widget_by_id"]).unwrap();
    assert_eq!(
        Value::Object(args),
        json!({"project": "P", "widgetId": "w1"})
    );
    assert_eq!(
        test.fill(&tools["list_widgets"]),
        Err("required parameter kind".to_string())
    );
    test.set_seed("kind", json!("round"));
    let args = test.fill(&tools["list_widgets"]).unwrap();
    assert_eq!(
        Value::Object(args),
        json!({"project": "P", "kind": "round", "$top": 5})
    );
    // The default team comes from get_project.
    assert_eq!(
        test.fill(&tools["run_query"]),
        Err("waiting for get_project".to_string())
    );

    // An optional path parameter with no value is left out instead of skipping the tool.
    let mut seeds = Map::new();
    seeds.insert("kind".into(), json!("round"));
    let test = ReadTest::new(
        Utcp::shared(PathBuf::from("utcp")),
        IndexMap::new(),
        BTreeSet::new(),
        seeds,
    );
    let args = test.fill(&tools["list_widgets"]).unwrap();
    assert_eq!(Value::Object(args), json!({"kind": "round", "$top": 5}));
}

/// A path parameter with a list of values takes one of them, not an id from the URL chain (a classification node's
/// `structureGroup` is `areas` or `iterations`; read-test used to pass the node's numeric id).
#[test]
fn fill_takes_a_listed_value_for_an_enum_parameter() {
    let tool = json!({
        "name": "get_node",
        "inputs": {"type": "object", "required": ["project", "structureGroup"], "properties": {
            "project": {"type": "string"},
            "structureGroup": {"type": "string", "enum": ["areas", "iterations"]}}},
        "tool_call_template": {"call_template_type": "http", "http_method": "GET",
            "url": "https://h/o/{project}/_apis/wit/nodes/{structureGroup}?api-version=7.1"}
    });
    let mut seeds = Map::new();
    seeds.insert("project".into(), json!("P"));
    let mut test = ReadTest::new(
        Utcp::shared(PathBuf::from("utcp")),
        IndexMap::new(),
        BTreeSet::new(),
        seeds,
    );
    let list = readtest::normalize("https://h/o/{project}/_apis/wit/nodes");
    test.items.insert(list.clone(), json!({"id": 243}));
    let args = test.fill(&tool).unwrap();
    assert_eq!(
        Value::Object(args),
        json!({"project": "P", "structureGroup": "areas"})
    );
    test.items
        .insert(list, json!({"structureGroup": "Iterations"}));
    let args = test.fill(&tool).unwrap();
    assert_eq!(args["structureGroup"], json!("Iterations"));
}

// utcp load test

pub(crate) fn find_utcp() -> Option<PathBuf> {
    let path = std::env::var_os("UTCP_BIN")
        .map(PathBuf::from)
        .or_else(|| which("utcp"));
    let found = path.clone().filter(|p| p.is_file()).filter(|p| {
        let out = Command::new(p).arg("--version").output();
        let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
        let version = text.ok().and_then(|t| crate::cli::utcp_version(&t));
        let new_enough = version.is_some_and(|v| v >= crate::cli::MIN_UTCP);
        if !new_enough {
            eprintln!(
                "{} is utcp {version:?}, older than {:?}",
                p.display(),
                crate::cli::MIN_UTCP
            );
        }
        new_enough
    });
    if found.is_none() && std::env::var_os("REQUIRE_UTCP").is_some() {
        panic!("REQUIRE_UTCP is set but no usable utcp binary was found (UTCP_BIN={path:?})");
    }
    found
}

#[test]
fn utcp_versions_are_read_from_utcp_version() {
    assert_eq!(crate::cli::utcp_version("utcp 0.2.4\n"), Some((0, 2, 4)));
    assert_eq!(
        crate::cli::utcp_version("utcp 1.10.0-rc1"),
        Some((1, 10, 0))
    );
    assert_eq!(crate::cli::utcp_version("utcp"), None);
    assert!(crate::cli::utcp_version("utcp 0.2.3").unwrap() < crate::cli::MIN_UTCP);
}

/// The expected manuals stage in a throwaway utcp, staged dry runs build the right requests, and they promote.
#[test]
fn utcp_stage_schema_dry_run_promote() {
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
    run(
        &["stage", "t_main", file.to_str().unwrap(), "--hint", "test"],
        None,
    );
    let listed: Value = serde_json::from_str(&run(&["list", "--json"], None)).unwrap();
    assert_eq!(
        listed,
        json!([{"name": "t_main", "tools": 14, "hint": "test", "staged": true}])
    );
    let manual: Value = serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
    for tool in manual["tools"].as_array().unwrap() {
        let name = format!("t_main.{}", tool["name"].as_str().unwrap());
        let schema: Value =
            serde_json::from_str(&run(&["schema", "--staged", &name], None)).unwrap();
        assert_eq!(schema["inputs"], tool["inputs"], "{name}");
    }
    let out: Value = serde_json::from_str(&run(
        &[
            "call",
            "--staged",
            "t_main.run_query",
            "--dry-run",
            "--args-stdin",
        ],
        Some(r#"{"project": "My Project", "body": {"text": "x", "$top": 2}}"#),
    ))
    .unwrap();
    // `team` is optional: left out, its segment is too.
    assert_eq!(
        out["url"],
        "https://dev.azure.com/contoso/My%20Project/_apis/alpha/query?api-version=7.1"
    );
    let out: Value = serde_json::from_str(&run(
        &[
            "call",
            "--staged",
            "t_main.run_query",
            "--dry-run",
            "--args-stdin",
        ],
        Some(r#"{"project": "P", "team": "Red Team", "body": {"text": "x"}}"#),
    ))
    .unwrap();
    assert_eq!(
        out["url"],
        "https://dev.azure.com/contoso/P/Red%20Team/_apis/alpha/query?api-version=7.1"
    );
    assert_eq!(out["method"], "POST");
    assert!(!out.to_string().contains("not-a-real-token"));
    let out: Value = serde_json::from_str(&run(
        &[
            "call",
            "--staged",
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
    run(&["promote", "t_main"], None);
    run(&["schema", "t_main.run_query"], None);
}

/// write-test's isolated utcp: registered manuals are active, none staged.
#[test]
fn isolated_utcp_registers_and_lists() {
    let Some(bin) = find_utcp() else {
        eprintln!("skipped: no utcp binary (set UTCP_BIN)");
        return;
    };
    let utcp = Utcp::isolated(bin, "azdo-test-").unwrap();
    utcp.register(&expected_dir(), [&"t_main".to_string()])
        .unwrap();
    assert_eq!(
        utcp.manual_names(false).unwrap(),
        BTreeSet::from(["t_main".to_string()])
    );
    assert!(utcp.manual_names(true).unwrap().is_empty());
}

#[test]
fn url_placeholders_take_the_inputs_spelling() {
    let props: Map<String, Value> =
        serde_json::from_value(json!({"project": {}, "definitionId": {}})).unwrap();
    assert_eq!(
        placeholders_named_as_inputs(
            "https://h/o/{project}/_apis/build/definitions/{DefinitionId}/tags",
            &props
        ),
        "https://h/o/{project}/_apis/build/definitions/{definitionId}/tags"
    );
    // No input of any case: left as it is.
    assert_eq!(
        placeholders_named_as_inputs("https://h/{other}", &props),
        "https://h/{other}"
    );
}

/// An optional path parameter is left out only where that keeps the route: at the end, or as a scope before the API
/// root. Before a fixed segment, the server would read that segment as the parameter.
#[test]
fn optional_path_parameters_are_left_out_only_where_the_route_stays() {
    let p = "/{organization}/{project}/{team}/_apis/wit/wiql/{id}";
    assert!(can_leave_out(p, "project"));
    assert!(can_leave_out(p, "team"));
    assert!(can_leave_out(p, "id"));
    assert!(can_leave_out(
        "/{organization}/{project}/_packaging/{feedId}/upack/packages/{packageName}/versions",
        "project"
    ));
    assert!(can_leave_out("/{organization}/_apis/x/{a}/{b}", "a"));
    assert!(!can_leave_out(
        "/{organization}/_apis/tfvc/changesets/{id}/changes",
        "id"
    ));
    assert!(!can_leave_out(
        "/{organization}/_apis/tfvc/changesets/{Id}/workItems",
        "id"
    ));
}
