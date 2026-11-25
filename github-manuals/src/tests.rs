//! Offline tests: a hand-made description in, exact manuals out. No network.
//!
//! Set UPDATE_EXPECTED=1 to rewrite tests/fixtures/expected/ after an intended change (then review the diff).
//! The utcp load test runs the `utcp` from UTCP_BIN, else from PATH, and is skipped when there is none or it is
//! older than 0.2.4 (CI sets REQUIRE_UTCP=1 on Linux to make it fail instead).
//! GITHUB_MANUALS_TEST_FIXTURES points the compiled test binary at a copy of tests/fixtures/, so it can run on a
//! machine without the source tree (a Windows test machine).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;

use indexmap::IndexMap;
use serde_json::{json, Value};

use crate::cli::{help, help_alias, left_out_lines, next_steps, which};
use crate::config::{why, Config, API_VERSION, PAT_VAR};
use crate::generate::{generate, DELETE_NOTE, FILE_ANSWER, UNTESTED, UTCP_MAX_HINT_CHARS};
use crate::http::{Request, Response, Transport};
use crate::names;
use crate::source::{self, sha256_hex, Pins, PINS};
use crate::text::{one_line, plain, restates, sentences, snake, tidy, write_json};

fn fixtures() -> PathBuf {
    if let Some(dir) = std::env::var_os("GITHUB_MANUALS_TEST_FIXTURES") {
        return PathBuf::from(dir);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn expected_dir() -> PathBuf {
    fixtures().join("expected")
}

fn source_json() -> Value {
    json!({"repository": "github/rest-api-description", "commit": "0".repeat(40),
           "commit_date": "2026-01-01T00:00:00Z", "file": "test.json", "sha256": "0".repeat(64)})
}

fn description() -> Value {
    serde_json::from_str(&fs::read_to_string(fixtures().join("description.json")).unwrap()).unwrap()
}

/// The real configuration, with only the name overrides the hand-made description has operations for.
fn test_config() -> Config {
    let spec = description();
    let ids: HashSet<String> = crate::generate::operations(&spec)
        .iter()
        .map(|o| o.op_id.to_string())
        .collect();
    let mut conf = Config::real();
    conf.names.retain(|k, _| ids.contains(k));
    conf.tested_writes = vec!["issues/create-comment".into()];
    conf
}

fn run_generate_with(conf: &Config) -> (IndexMap<String, Value>, Value) {
    generate(&description(), API_VERSION, &source_json(), conf).unwrap()
}

fn run_generate() -> (IndexMap<String, Value>, Value) {
    run_generate_with(&test_config())
}

fn tools_by_name(manuals: &IndexMap<String, Value>) -> HashMap<String, Value> {
    manuals
        .values()
        .flat_map(|m| m["tools"].as_array().unwrap().clone())
        .map(|t| (t["name"].as_str().unwrap().to_string(), t))
        .collect()
}

fn reason_of(report: &Value, op_id: &str) -> Option<String> {
    report["left_out"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["operation_id"] == op_id)
        .map(|x| x["reason"].as_str().unwrap().to_string())
}

fn input_names(tool: &Value) -> Vec<&str> {
    tool["inputs"]["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}

fn required(tool: &Value) -> Vec<&str> {
    tool["inputs"]["required"]
        .as_array()
        .map(|a| a.iter().map(|v| v.as_str().unwrap()).collect())
        .unwrap_or_default()
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
fn left_out_with_reasons() {
    let (manuals, report) = run_generate();
    let tools = tools_by_name(&manuals);
    for (op, reason) in [
        ("repos/update", why::REPO_SETTINGS),
        ("orgs/list-members", why::ORG),
        ("teams/get-legacy", "deprecated by GitHub"),
    ] {
        assert_eq!(reason_of(&report, op).as_deref(), Some(reason), "{op}");
    }
    // Kept by the org rule's exception.
    assert!(tools.contains_key("get_org"));
    assert!(reason_of(&report, "orgs/get").is_none());
    let total = crate::generate::operations(&description()).len();
    assert_eq!(
        tools.len() + report["left_out"].as_array().unwrap().len(),
        total
    );
    let summary: usize = report["left_out_summary"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["operations"].as_u64().unwrap() as usize)
        .sum();
    assert_eq!(summary, report["left_out"].as_array().unwrap().len());
    let lines = left_out_lines(&report);
    assert!(lines.iter().any(|l| l.ends_with(why::ORG)), "{lines:?}");
}

#[test]
fn manuals_by_category_and_subcategory() {
    let (manuals, report) = run_generate();
    let manual_of = |name: &str| {
        manuals
            .iter()
            .find(|(_, m)| {
                m["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t["name"] == name)
            })
            .map(|(n, _)| n.as_str())
    };
    assert_eq!(manual_of("get_repo"), Some("github_repos"));
    assert_eq!(manual_of("create_issue"), Some("github_issues_pulls"));
    assert_eq!(
        manual_of("list_reactions_for_issue"),
        Some("github_issues_pulls")
    );
    // repos/attestations moves to security.
    assert_eq!(manual_of("create_attestation"), Some("github_security"));
    assert_eq!(manual_of("create_check_run"), Some("github_actions"));
    assert_eq!(
        manual_of("list_agent_tasks_for_repo"),
        Some("github_copilot")
    );
    assert_eq!(manual_of("search_code"), Some("github_search"));
    assert_eq!(manual_of("get_authenticated_user"), Some("github_users"));
    let order: Vec<&String> = manuals.keys().collect();
    assert_eq!(order[0], "github_repos");
    let m = &report["manuals"]["github_issues_pulls"];
    assert_eq!(
        m["reads"].as_u64().unwrap() + m["writes"].as_u64().unwrap(),
        m["tools"].as_u64().unwrap()
    );
    assert_eq!(manuals["github_repos"]["info"]["version"], API_VERSION);
}

#[test]
fn every_tool_sends_the_token_and_api_version() {
    let (manuals, _) = run_generate();
    for (name, tool) in tools_by_name(&manuals) {
        let t = &tool["tool_call_template"];
        assert_eq!(
            t["auth"]["api_key"],
            format!("Bearer ${{{PAT_VAR}}}"),
            "{name}"
        );
        assert_eq!(t["auth"]["var_name"], "Authorization", "{name}");
        assert_eq!(t["headers"]["X-GitHub-Api-Version"], API_VERSION, "{name}");
        assert_eq!(
            t["headers"]["Accept"], "application/vnd.github+json",
            "{name}"
        );
        assert!(t["url"].as_str().unwrap().starts_with("https://"), "{name}");
    }
}

#[test]
fn flat_bodies() {
    let tools = tools_by_name(&run_generate().0);
    let issue = &tools["create_issue"];
    assert_eq!(
        input_names(issue),
        ["owner", "repo", "title", "body", "labels", "parent"]
    );
    assert_eq!(required(issue), ["owner", "repo", "title"]);
    assert!(issue["tool_call_template"].get("body_field").is_none());
    // The read-only `id` is not an argument, and the self-referencing node stops at the cycle.
    let children = &issue["inputs"]["properties"]["parent"]["properties"]["children"];
    assert_eq!(children["items"]["description"], "A node of an issue tree.");
    // GitHub's own `body` field is an ordinary argument.
    let comment = &tools["create_issue_comment"];
    assert_eq!(comment["inputs"]["properties"]["body"]["type"], "string");
    assert_eq!(required(comment), ["owner", "repo", "issue_number", "body"]);
}

#[test]
fn body_field_when_flat_cannot_work() {
    let tools = tools_by_name(&run_generate().0);
    // Not an object: passed whole.
    let labels = &tools["add_issue_labels"];
    assert_eq!(labels["tool_call_template"]["body_field"], "body");
    assert!(labels["inputs"]["properties"]["body"]["oneOf"].is_array());
    assert_eq!(required(labels), ["owner", "repo", "issue_number"]);
    // A query parameter next to the body: utcp sends leftover arguments as query only with a body field.
    let attestation = &tools["create_attestation"];
    assert_eq!(attestation["tool_call_template"]["body_field"], "body");
    assert_eq!(input_names(attestation), ["owner", "repo", "dry", "body"]);
    assert_eq!(required(attestation), ["owner", "repo", "body"]);
}

#[test]
fn delete_and_text_bodies() {
    let tools = tools_by_name(&run_generate().0);
    // A DELETE sends its other arguments as query parameters, so its body is always one argument.
    let remove = &tools["remove_issue_assignees"];
    let t = &remove["tool_call_template"];
    assert_eq!(t["http_method"], "DELETE");
    assert_eq!(t["body_field"], "body");
    assert!(t.get("body_type").is_none());
    let body = &remove["inputs"]["properties"]["body"];
    assert_eq!(body["type"], "object");
    assert!(body["properties"]["assignees"].is_object());
    assert_eq!(body["description"], "The request body.");
    assert_eq!(required(remove), ["owner", "repo", "issue_number"]);
    // A text/plain body is sent as is.
    let raw = &tools["render_markdown_raw"];
    let t = &raw["tool_call_template"];
    assert_eq!(t["body_type"], "text");
    assert_eq!(t["body_field"], "body");
    assert_eq!(t["headers"]["Content-Type"], "text/plain");
    assert_eq!(raw["inputs"]["properties"]["body"]["type"], "string");
    assert!(tools["create_issue"]["tool_call_template"]["headers"]
        .get("Content-Type")
        .is_none());
}

#[test]
fn upload_and_file_answers() {
    let tools = tools_by_name(&run_generate().0);
    let upload = &tools["upload_release_asset"];
    let t = &upload["tool_call_template"];
    assert_eq!(
        t["url"],
        "https://uploads.github.com/repos/{owner}/{repo}/releases/{release_id}/assets"
    );
    assert_eq!(t["body_type"], "file");
    assert_eq!(t["body_field"], "body");
    assert_eq!(
        upload["inputs"]["properties"]["body"]["required"],
        json!(["file"])
    );
    assert_eq!(required(upload), ["owner", "repo", "release_id", "name"]);
    let logs = &tools["download_job_logs_for_workflow_run"];
    assert_eq!(logs["tool_call_template"]["response_type"], "file");
    assert_eq!(logs["outputs"]["description"], FILE_ANSWER);
    // 200 with JSON or a redirect: JSON, and the empty GET body is ignored.
    let content = &tools["get_content"];
    assert!(content["tool_call_template"].get("response_type").is_none());
    assert!(content["tool_call_template"].get("body_field").is_none());
    assert_eq!(input_names(content), ["owner", "repo", "path", "ref"]);
    assert_eq!(tools["get_zen"]["outputs"]["type"], "string");
    assert_eq!(tools["list_issue_assignees"]["outputs"]["type"], "array");
    assert_eq!(tools["delete_issue_comment"]["outputs"]["type"], "null");
}

#[test]
fn query_parameters() {
    let tools = tools_by_name(&run_generate().0);
    let reactions = &tools["list_reactions_for_issue"]["inputs"]["properties"];
    assert_eq!(reactions["content"]["enum"], json!(["+1", "-1", "heart"]));
    assert_eq!(reactions["per_page"]["default"], 30);
    // An array query parameter takes one value (utcp would send the array as one JSON string).
    let creator = &tools["list_agent_tasks_for_repo"]["inputs"]["properties"]["creator_id"];
    assert_eq!(creator["type"], "integer");
    assert_eq!(creator["description"], "Filter by creator. (one value)");
}

#[test]
fn descriptions() {
    let tools = tools_by_name(&run_generate().0);
    let repo = tools["get_repo"]["description"].as_str().unwrap();
    // "Gets a repository." restates the summary, and "See Repositories." lost its link.
    assert_eq!(repo, "Get a repository");
    let content = tools["get_content"]["description"].as_str().unwrap();
    assert_eq!(
        content,
        "Get repository content. Gets the contents of a file."
    );
    let issue = tools["create_issue"]["description"].as_str().unwrap();
    assert_eq!(
        issue,
        format!("{UNTESTED} Create an issue. Any user with pull access can create an issue.")
    );
    // The classic-token scope is shortened in place; "e.g." does not end a sentence.
    assert_eq!(
        tools["list_reactions_for_issue"]["description"],
        "List reactions for an issue. Classic tokens need the repo scope with a private repository. You can use \
         parameters to narrow the list of results. Each reaction has a type, e.g. heart. Anyone with read access \
         to the repository can use this endpoint."
    );
    assert_eq!(
        tools["list_reactions_for_issue"]["inputs"]["properties"]["per_page"]["description"],
        "The number of results per page (max 100)."
    );
    // A tested write has no marker; a DELETE starts with the note.
    assert_eq!(
        tools["create_issue_comment"]["description"],
        "Create an issue comment"
    );
    let delete = tools["delete_issue_comment"]["description"]
        .as_str()
        .unwrap();
    assert_eq!(
        delete,
        format!("{UNTESTED} {DELETE_NOTE} Delete an issue comment")
    );
    assert!(tools["delete_issue_comment"]["tags"]
        .as_array()
        .unwrap()
        .contains(&json!("untested")));
    assert_eq!(tools["get_repo"]["tags"], json!(["github", "repos"]));
    assert_eq!(
        tools["download_job_logs_for_workflow_run"]["tags"],
        json!(["github", "actions", "workflow-jobs"])
    );
}

#[test]
fn read_only_leaves_out_writes() {
    let (manuals, report) = run_generate_with(&test_config().read_only());
    let tools = tools_by_name(&manuals);
    assert!(tools.contains_key("get_repo"));
    assert!(!tools.contains_key("create_issue"));
    assert_eq!(
        reason_of(&report, "issues/create").as_deref(),
        Some("write (--read-only)")
    );
    assert!(tools
        .values()
        .all(|t| t["tool_call_template"]["http_method"] == "GET"));
}

#[test]
fn unknown_category_fails() {
    let mut spec = description();
    spec["paths"]["/zen"]["get"]["x-github"]["category"] = json!("brand-new-area");
    let e = generate(&spec, API_VERSION, &source_json(), &test_config()).unwrap_err();
    assert!(
        e.0.contains("category 'brand-new-area' has no manual"),
        "{}",
        e.0
    );
}

#[test]
fn long_hint_and_unknown_name_fail() {
    let mut conf = test_config();
    conf.manuals[0].hint = "x".repeat(UTCP_MAX_HINT_CHARS + 1);
    let e = generate(&description(), API_VERSION, &source_json(), &conf).unwrap_err();
    assert!(e.0.contains("hint longer"), "{}", e.0);
    let mut conf = test_config();
    conf.names.insert("repos/no-such-thing".into(), "x".into());
    let e = generate(&description(), API_VERSION, &source_json(), &conf).unwrap_err();
    assert!(e.0.contains("repos/no-such-thing"), "{}", e.0);
}

// Names

#[test]
fn names_follow_scheme_c() {
    let conf = Config::real();
    for (op, want) in [
        ("repos/get", "get_repo"),
        ("repos/list-for-org", "list_repos_for_org"),
        (
            "repos/create-or-update-file-contents",
            "create_or_update_file_contents",
        ),
        ("users/get-authenticated", "get_authenticated_user"),
        ("users/follow", "follow_user"),
        ("users/check-blocked", "check_user_blocked"),
        ("issues/create", "create_issue"),
        ("issues/list-for-repo", "list_issues_for_repo"),
        ("issues/list", "list_assigned_issues"),
        ("issues/add-labels", "add_issue_labels"),
        ("pulls/list-files", "list_pull_request_files"),
        ("pulls/list", "list_pull_requests"),
        ("reactions/create-for-issue", "create_reaction_for_issue"),
        ("reactions/list-for-issue", "list_reactions_for_issue"),
        ("git/create-blob", "create_git_blob"),
        ("dependabot/get-alert", "get_dependabot_alert"),
        ("code-scanning/get-analysis", "get_code_scanning_analysis"),
        ("secret-scanning/get-alert", "get_secret_scanning_alert"),
        (
            "search/issues-and-pull-requests",
            "search_issues_and_pull_requests",
        ),
        ("checks/create", "create_check_run"),
        ("checks/create-suite", "create_check_suite"),
        (
            "actions/create-workflow-dispatch",
            "create_workflow_dispatch",
        ),
        (
            "agents/list-repo-secrets",
            "list_copilot_agent_repo_secrets",
        ),
    ] {
        assert_eq!(names::base_name(op, &conf), want, "{op}");
    }
}

#[test]
fn clashes_get_the_area_and_unresolved_ones_fail() {
    let mut conf = Config::real();
    conf.tag_nouns.clear();
    let ids = ["repos/get-commit", "git/get-commit", "zoo/get-commit"];
    let named = names::assign("github_repos", &ids, &conf).unwrap();
    assert_eq!(named, ["get_commit", "get_git_commit", "get_zoo_commit"]);
    conf.names
        .insert("zoo/get-commit".into(), "get_commit".into());
    let e = names::assign("github_repos", &ids, &conf).unwrap_err();
    assert!(e.contains("'get_commit' is used by"), "{e}");
}

// Real configuration

#[test]
fn real_config_is_consistent() {
    let conf = Config::real();
    let manuals: HashSet<&str> = conf.manual_names().into_iter().collect();
    for m in conf.categories.values().chain(conf.subcategories.values()) {
        assert!(manuals.contains(m.as_str()), "{m}");
    }
    for m in &conf.manuals {
        assert!(m.hint.chars().count() <= UTCP_MAX_HINT_CHARS, "{}", m.name);
        assert!(m.name.starts_with("github_"), "{}", m.name);
    }
    let names: Vec<&String> = conf.names.values().collect();
    assert_eq!(
        names.len(),
        names.iter().collect::<HashSet<_>>().len(),
        "an override name used twice"
    );
    for n in names {
        assert_eq!(&snake(n), n, "{n} is not snake_case");
    }
    // The old hand-written manual's `list_issues` meant a repository's issues; no new tool takes that name.
    assert!(!conf.names.values().any(|n| n == "list_issues"));
}

#[test]
fn pins_match_the_api_version() {
    let pins = Pins::parse(PINS).unwrap();
    assert_eq!(pins.api_version, API_VERSION);
    assert!(pins.file.ends_with(&format!(".{API_VERSION}.json")));
    assert_eq!(pins.commit.len(), 40);
    assert_eq!(pins.sha256.len(), 64);
    assert!(pins
        .url()
        .starts_with("https://raw.githubusercontent.com/github/rest-api-description/"));
}

// Text

#[test]
fn text_helpers() {
    assert_eq!(snake("create-or-update"), "create_or_update");
    assert_eq!(one_line("a  b\n c", 100), "a b c");
    assert_eq!(one_line("alpha beta gamma, delta", 18), "alpha beta...");
    assert_eq!(
        plain("> [!NOTE]\n> See [the docs](https://docs.github.com/x) and `code` **now**."),
        "See the docs and code now."
    );
    assert_eq!(
        plain("Use <a href=\"x\">rate limits</a>.<br/>Then wait."),
        "Use rate limits. Then wait."
    );
    assert_eq!(
        plain("An _issue id_ (_not_ the pull_request key or `_links`)."),
        "An issue id (not the pull_request key or _links)."
    );
}

#[test]
fn sentences_split_at_ends_only() {
    assert_eq!(
        sentences("Use a name (e.g. GitHub). See \"Docs.\" Then v1.2 works, etc. Done? Yes"),
        [
            "Use a name (e.g. GitHub).",
            "See \"Docs.\"",
            "Then v1.2 works, etc. Done?",
            "Yes"
        ]
    );
    assert!(sentences("  ").is_empty());
}

#[test]
fn tidy_descriptions() {
    // A first sentence that restates the summary goes, whatever its verb form or filler words.
    assert!(restates(
        "List workflow runs for a repository",
        "Lists all workflow runs for a repository."
    ));
    assert!(restates("Get a gist comment", "Gets a comment on a gist."));
    assert!(!restates(
        "List concurrency groups for a repository",
        "Lists the active concurrency groups."
    ));
    assert!(!restates(
        "Delete a repository",
        "Deleting a repository requires admin access."
    ));
    assert_eq!(
        tidy("Gets a gist. It may be truncated.", "Get a gist"),
        "It may be truncated."
    );
    // Without a summary, nothing is taken as a restatement.
    assert_eq!(tidy("Gets a gist.", ""), "Gets a gist.");
    // Pointers to stripped links go.
    assert_eq!(
        tidy(
            "Page number. For more information, see \"Using pagination.\" Starts at 1.",
            ""
        ),
        "Page number. Starts at 1."
    );
    assert_eq!(tidy("See \"Searching topics\" for qualifiers.", ""), "");
    // Classic-token scope sentences are shortened where they stand.
    assert_eq!(
        tidy(
            "If the repository is private, OAuth tokens and personal access tokens (classic) need the repo scope to \
             use this endpoint. The workflow scope is also required.",
            ""
        ),
        "If the repository is private, classic tokens need the repo scope. The workflow scope is also required."
    );
    assert_eq!(
        tidy(
            "Lists alerts. OAuth app tokens and personal access tokens (classic) need the security_events scope to \
             use this endpoint with private or public repositories, or the public_repo scope to use this endpoint \
             with only public repositories.",
            ""
        ),
        "Lists alerts. Classic tokens need the security_events scope with private or public repositories, or the \
         public_repo scope with only public repositories."
    );
    assert_eq!(
        tidy(
            "OAuth apps and personal access tokens (classic) cannot use this endpoint.",
            ""
        ),
        "Classic tokens cannot use this endpoint."
    );
}

// Source

fn fake(body: Vec<u8>, calls: Rc<RefCell<Vec<String>>>) -> Transport {
    Box::new(move |req: &Request| {
        calls.borrow_mut().push(req.url.clone());
        assert!(req
            .headers
            .iter()
            .any(|(k, v)| k == "User-Agent" && v.starts_with("github-manuals/")));
        Ok(Response {
            status: 200,
            headers: Vec::new(),
            body: body.clone(),
        })
    })
}

fn pins_for(data: &[u8]) -> Pins {
    Pins {
        sha256: sha256_hex(data),
        length: data.len() as u64,
        ..Pins::parse(PINS).unwrap()
    }
}

#[test]
fn spec_download_is_verified_and_cached() {
    let data = br#"{"openapi": "3.0.3"}"#.to_vec();
    let pins = pins_for(&data);
    let cache = tempfile::tempdir().unwrap();
    let calls = Rc::new(RefCell::new(Vec::new()));
    let mut t = fake(data.clone(), calls.clone());
    let (got, downloaded) = source::fetch_spec(&mut t, &pins, cache.path()).unwrap();
    assert_eq!((got, downloaded), (data.clone(), true));
    assert_eq!(*calls.borrow(), [pins.url()]);
    let (got, downloaded) = source::fetch_spec(&mut t, &pins, cache.path()).unwrap();
    assert_eq!((got, downloaded), (data, false));
    assert_eq!(calls.borrow().len(), 1, "the second run uses the cache");
}

#[test]
fn spec_with_another_hash_is_refused_and_not_cached() {
    let pins = pins_for(b"the pinned file");
    let cache = tempfile::tempdir().unwrap();
    let mut t = fake(
        b"something else".to_vec(),
        Rc::new(RefCell::new(Vec::new())),
    );
    let e = source::fetch_spec(&mut t, &pins, cache.path()).unwrap_err();
    assert!(e.0.contains("does not match the pinned file"), "{}", e.0);
    assert_eq!(
        fs::read_dir(cache.path()).map(|d| d.count()).unwrap_or(0),
        0
    );
}

// Command line

#[test]
fn help_and_next_steps() {
    let args = |a: &[&str]| a.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
    assert_eq!(
        help_alias(args(&["gm", "generate", "help"])),
        args(&["gm", "help", "generate"])
    );
    assert_eq!(
        help_alias(args(&["gm", "nope", "help"])),
        args(&["gm", "nope", "help"])
    );
    let text = help(None).to_string();
    assert!(
        text.contains("generate") && text.contains("GITHUB_MANUALS_PAT"),
        "{text}"
    );
    assert!(help(Some("nope"))
        .to_string()
        .contains("unknown command 'nope'"));
    let steps = next_steps().join("\n");
    assert!(
        steps.contains(PAT_VAR)
            && steps.contains("utcp diff 'github_*'")
            && steps.contains("utcp promote 'github_*'"),
        "{steps}"
    );
}

// utcp load test

fn find_utcp() -> Option<PathBuf> {
    let path = std::env::var_os("UTCP_BIN")
        .map(PathBuf::from)
        .or_else(|| which("utcp"));
    let found = path.clone().filter(|p| p.is_file());
    let usable = match &found {
        None => Err(format!("no utcp binary (UTCP_BIN={path:?})")),
        Some(p) => crate::utcp::check_version(p),
    };
    match usable {
        Ok(()) => found,
        Err(e) if std::env::var_os("REQUIRE_UTCP").is_some() => {
            panic!("REQUIRE_UTCP is set but there is no usable utcp: {e}")
        }
        Err(e) => {
            eprintln!("skipped: {e} (set UTCP_BIN)");
            None
        }
    }
}

/// The expected manuals stage in a throwaway utcp, staged dry runs build the right requests, and they promote.
#[test]
fn utcp_stage_schema_dry_run_promote() {
    let Some(utcp) = find_utcp() else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let run = |args: &[&str], stdin: Option<&str>| -> String {
        use std::io::Write;
        use std::process::Stdio;
        let mut cmd = Command::new(&utcp);
        cmd.args(args)
            .arg("--direct")
            .env(PAT_VAR, "not-a-real-token");
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
    let (manuals, _) = run_generate();
    for name in manuals.keys() {
        let file = expected_dir().join(format!("{name}.utcp.json"));
        run(
            &["stage", name, file.to_str().unwrap(), "--hint", "test"],
            None,
        );
    }
    let listed = |staged: bool| {
        let list: Value = serde_json::from_str(&run(&["list", "--json"], None)).unwrap();
        let list = list.as_array().unwrap().clone();
        assert_eq!(list.len(), manuals.len(), "{list:?}");
        assert!(
            list.iter()
                .all(|m| m["staged"].as_bool().unwrap_or(false) == staged),
            "{list:?}"
        );
    };
    listed(true);
    for (name, manual) in &manuals {
        for tool in manual["tools"].as_array().unwrap() {
            let full = format!("{name}.{}", tool["name"].as_str().unwrap());
            let schema: Value =
                serde_json::from_str(&run(&["schema", "--staged", &full], None)).unwrap();
            assert_eq!(schema["inputs"], tool["inputs"], "{full}");
        }
    }
    let dry = |tool: &str, args: &str| -> Value {
        serde_json::from_str(&run(
            &["call", "--staged", tool, "--dry-run", "--args-stdin"],
            Some(args),
        ))
        .unwrap()
    };
    let out = dry(
        "github_issues_pulls.create_issue",
        r#"{"owner": "o", "repo": "r", "title": "T", "body": "B", "labels": ["x"]}"#,
    );
    assert_eq!(out["url"], "https://api.github.com/repos/o/r/issues");
    assert_eq!(out["method"], "POST");
    assert_eq!(
        out["args"],
        json!({"title": "T", "body": "B", "labels": ["x"]})
    );
    assert_eq!(out["headers"]["X-GitHub-Api-Version"], API_VERSION);
    assert!(!out.to_string().contains("not-a-real-token"));
    let out = dry(
        "github_repos.get_content",
        r#"{"owner": "o", "repo": "r", "path": "a/b.txt", "ref": "main"}"#,
    );
    assert_eq!(
        out["url"],
        "https://api.github.com/repos/o/r/contents/a%2Fb.txt"
    );
    assert_eq!(out["method"], "GET");
    let out = dry(
        "github_issues_pulls.remove_issue_assignees",
        r#"{"owner": "o", "repo": "r", "issue_number": 5, "body": {"assignees": ["a"]}}"#,
    );
    assert_eq!(out["method"], "DELETE");
    assert_eq!(
        out["url"],
        "https://api.github.com/repos/o/r/issues/5/assignees"
    );
    assert_eq!(out["body"], json!({"assignees": ["a"]}));
    assert_eq!(out["query"], json!({}));
    let out = dry("github_users.render_markdown_raw", r##"{"body": "# Hi"}"##);
    assert_eq!(out["method"], "POST");
    assert_eq!(out["body"], "# Hi");
    assert_eq!(out["body_type"], "text");
    assert_eq!(out["headers"]["Content-Type"], "text/plain");
    run(&["promote", "github_*"], None);
    listed(false);
    run(&["schema", "github_repos.get_repo"], None);
}
