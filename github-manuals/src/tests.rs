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
use crate::text::{
    end_sentence, fit, one_line, plain, restates, sentences, snake, tidy, tidy_sentences,
    write_json, Sentence,
};

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
    conf.input_descriptions
        .retain(|(op, _), _| ids.contains(op));
    conf.optional_path.retain(|(op, ..)| ids.contains(op));
    conf.notes.retain(|op, _| ids.contains(op));
    conf.variants.retain(|v| ids.contains(&v.op_id));
    // The real fixes match GitHub's real text, which the hand-made description does not have.
    conf.sentence_fixes.clear();
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
    // A variant is a second tool for an operation that already has one.
    let total = crate::generate::operations(&description()).len();
    assert_eq!(
        tools.len() + report["left_out"].as_array().unwrap().len(),
        total + test_config().variants.len()
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
    let (manuals, report) = run_generate();
    let variants: HashMap<String, String> = test_config()
        .variants
        .iter()
        .map(|v| (v.name.to_string(), v.accept.to_string()))
        .collect();
    assert!(variants.contains_key("get_raw_content"));
    for (name, tool) in tools_by_name(&manuals) {
        let t = &tool["tool_call_template"];
        assert_eq!(
            t["auth"]["api_key"],
            format!("Bearer ${{{PAT_VAR}}}"),
            "{name}"
        );
        assert_eq!(t["auth"]["var_name"], "Authorization", "{name}");
        assert_eq!(t["headers"]["X-GitHub-Api-Version"], API_VERSION, "{name}");
        let accept = variants
            .get(&name)
            .map_or("application/vnd.github+json", String::as_str);
        assert_eq!(t["headers"]["Accept"], accept, "{name}");
        let listed = report["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["name"] == name.as_str())
            .unwrap();
        assert_eq!(listed["accept"], accept, "{name}");
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
    // An array query parameter takes one value: GitHub reads repeated parameters, utcp sends a list comma-separated.
    let creator = &tools["list_agent_tasks_for_repo"]["inputs"]["properties"]["creator_id"];
    assert_eq!(creator["type"], "integer");
    assert_eq!(
        creator["description"],
        "Filter by creator. Takes one value."
    );
}

#[test]
fn placeholder_descriptions_and_optional_path() {
    let tools = tools_by_name(&run_generate().0);
    let content = &tools["get_content"];
    assert_eq!(required(content), ["owner", "repo"]);
    let path = content["inputs"]["properties"]["path"]["description"]
        .as_str()
        .unwrap();
    assert!(
        path.starts_with("The path of a file or directory")
            && path.ends_with("Leave it out for the repository root."),
        "{path}"
    );
    let mut conf = test_config();
    conf.param_descriptions.remove("path");
    let e = generate(&description(), API_VERSION, &source_json(), &conf)
        .unwrap_err()
        .to_string();
    assert!(e.contains("'path' has only the placeholder"), "{e}");
    let mut conf = test_config();
    conf.optional_path
        .push(("repos/no-such".into(), "path".into(), "x".into()));
    let e = generate(&description(), API_VERSION, &source_json(), &conf)
        .unwrap_err()
        .to_string();
    assert!(e.contains("repos/no-such"), "{e}");
}

#[test]
fn descriptions() {
    let tools = tools_by_name(&run_generate().0);
    let repo = tools["get_repo"]["description"].as_str().unwrap();
    // "Gets a repository." restates the summary, and "See Repositories." lost its link; the note points to files.
    assert_eq!(
        repo,
        "Get a repository. To read a file, use github_repos.get_content."
    );
    let content = tools["get_content"]["description"].as_str().unwrap();
    assert_eq!(
        content,
        "Get repository content. Gets the contents of a file. For a file's content as it is, instead of base64, \
         use github_repos.get_raw_content."
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
         to the repository can use this tool."
    );
    assert_eq!(
        tools["list_reactions_for_issue"]["inputs"]["properties"]["per_page"]["description"],
        "The number of results per page (max 100)."
    );
    // A tested write has no marker; a DELETE has the note after its summary, so search still shows what it deletes.
    assert_eq!(
        tools["create_issue_comment"]["description"],
        "Create an issue comment."
    );
    let delete = tools["delete_issue_comment"]["description"]
        .as_str()
        .unwrap();
    assert_eq!(
        delete,
        format!("{UNTESTED} Delete an issue comment. {DELETE_NOTE}")
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
        "Classic tokens cannot use this tool."
    );
}

#[test]
fn markdown_blocks() {
    let text = |md: &str| tidy(&plain(md), "");
    // Each list item is a sentence; a wrapped line (indented, or starting lowercase) joins its item.
    assert_eq!(
        text("Lists runs:\n\n- `Queued` runs\n  that wait.\n- Completed runs\nthat ended\n\nDone."),
        "Lists runs: Queued runs that wait. Completed runs that ended. Done."
    );
    // A table row is `first cell: other cells`, without the header and rule rows.
    assert_eq!(
        text("States:\n\n| Name | Meaning |\n| --- | :---: |\n| `open` | Not done | Yet |\n| `closed` | Done |"),
        "States: open: Not done, Yet. closed: Done."
    );
    // A bold heading goes, unless it leads in to what follows.
    assert_eq!(
        text("**Signature verification object**\n\nThe object has fields."),
        "The object has fields."
    );
    assert_eq!(
        text("**For a team:**\n\nUse the slug."),
        "For a team: Use the slug."
    );
    // Code keeps its text, without the fences.
    assert_eq!(
        text("Example:\n\n```\ncurl -X POST\n```\n\nThen wait."),
        "Example: curl -X POST. Then wait."
    );
    // An image becomes its alt text.
    assert_eq!(
        text("Shows the flow: ![Flow chart](https://x.test/y.png \"t\")"),
        "Shows the flow: Flow chart."
    );
    assert_eq!(plain("  - One\n"), "One.");
}

#[test]
fn tidy_sentence_list() {
    let texts = |t: &str| {
        tidy_sentences(t, "")
            .into_iter()
            .map(|s| (s.text, s.essential))
            .collect::<Vec<_>>()
    };
    // A paragraph break ends a sentence, even after an example; the last one ends with a period only after a word.
    assert_eq!(
        tidy("Use base64 -w0\n\nUploads work. Default: main", ""),
        "Use base64 -w0. Uploads work. Default: main."
    );
    assert_eq!(
        tidy("Example: https://example.com/a", ""),
        "Example: https://example.com/a"
    );
    assert_eq!(
        tidy("(see below)\n\nHe said \"done.\"\n\nNext", ""),
        "(see below). He said \"done.\" Next."
    );
    // A lead-in ending with `:` takes the next sentence with it, essential if that one is.
    assert_eq!(
        texts(
            "Lists items. For private repositories:\n\nOAuth app tokens and personal access tokens (classic) need \
             the repo scope to use this endpoint."
        ),
        [
            ("Lists items.".to_string(), false),
            (
                "For private repositories: Classic tokens need the repo scope.".to_string(),
                true
            )
        ]
    );
    // A clause that only points to a stripped link goes.
    assert_eq!(
        tidy(
            "Returns runs, see \"Workflow runs\" for more information.",
            ""
        ),
        "Returns runs."
    );
    assert_eq!(
        tidy("Lists items; for more information, see \"Items.\"", ""),
        "Lists items."
    );
    // Doubled small words are written once, and a repeated sentence goes.
    assert_eq!(
        tidy(
            "Needs the the repo scope to to work. Needs the repo scope to work.",
            ""
        ),
        "Needs the repo scope to work."
    );
    assert!(tidy_sentences("\n\n  \n", "").is_empty());
}

#[test]
fn http_details_become_what_an_agent_does() {
    // Words that say only that the tool is part of the API go.
    assert_eq!(
        tidy(
            "You can use the REST API to list the sub-issues on an issue.",
            ""
        ),
        "List the sub-issues on an issue."
    );
    // Paging is next_page, and headers are utcp's business.
    assert_eq!(
        tidy(
            "Use the Link header to get the URL for the next page of results. Set the Accept header to raw.",
            ""
        ),
        "Call again with next_page for the next page of results."
    );
    assert_eq!(
        tidy(
            "The Checks API only looks for pushes in the repository where the check suite or check run were created.",
            ""
        ),
        "Checks only look for pushes in the repository where the check suite or check run was created."
    );
    // utcp prints the answer, not its status code.
    assert_eq!(
        tidy(
            "A response with an HTTP 200 status means that you already added the reaction type to this issue.",
            ""
        ),
        "If you already added this reaction type to this issue, the answer is that reaction and nothing is added."
    );
    // The tool itself, and tools in general, are tools; a Rails placeholder and a query parameter are arguments.
    assert_eq!(
        tidy(
            "That endpoint reports runs. The endpoint will return 404. Use \"Issues\" endpoints. If you omit :ref, \
             the default branch is used.",
            ""
        ),
        "That tool reports runs. The tool will return 404. Use \"Issues\" tools. If you omit ref, the default \
         branch is used."
    );
    assert_eq!(
        tidy("To list all, pass the query parameter all=false.", ""),
        "To list all, with all set to false."
    );
    // A list in the URL query is the JSON list the argument takes.
    assert_eq!(
        tidy(
            "Example: cwes=79,284,22 or cwes[]=79&cwes[]=284&cwes[]=22",
            ""
        ),
        "Example: [\"79\", \"284\", \"22\"]"
    );
    assert_eq!(
        tidy("Example: cwes[]=79&cwes[]=284 or cwes=79,284", ""),
        "Example: [\"79\", \"284\"]"
    );
}

#[test]
fn sentences_end_before_lowercase_words_only_after_words() {
    // "created_at means" starts a sentence after a word, but a lowercase word after "v2." or
    // "No." does not; a capital after "tags." does.
    assert_eq!(
        sentences("Sorts by the cache. created_at means age. Use v2. tags. No. items."),
        [
            "Sorts by the cache.",
            "created_at means age.",
            "Use v2. tags.",
            "No. items."
        ]
    );
    // JSON arguments end with a period; a URL or a quoted value does not.
    assert_eq!(
        end_sentence("Use {\"q\": \"css\"}"),
        "Use {\"q\": \"css\"}."
    );
    // A search's URL query in a code span is the JSON arguments the tool takes.
    assert_eq!(
        plain("Try `q=tetris+language:assembly&sort=stars&per_page=5`."),
        "Try {\"q\": \"tetris language:assembly\", \"sort\": \"stars\", \"per_page\": 5}."
    );
}

#[test]
fn links_name_tools() {
    let spec = description();
    let ops = crate::generate::operations(&spec);
    let tools: HashMap<String, String> = [
        ("repos/get", "github_repos.get_repo"),
        (
            "issues/list-assignees",
            "github_issues_pulls.list_issue_assignees",
        ),
        ("checks/create", "github_actions.create_check_run"),
        (
            "code-scanning/get-alert",
            "github_security.get_code_scanning_alert",
        ),
    ]
    .into_iter()
    .map(|(op, tool)| (op.to_string(), tool.to_string()))
    .collect();
    let t = crate::links::Targets::new(&ops, &tools);
    let page = "https://docs.github.com/en/rest/repos/repos#get-a-repository";
    // A page title in quotes, or after "use the", is the tool.
    assert_eq!(
        t.rewrite(&format!("See \"[Get a repository]({page}).\"")),
        "See github_repos.get_repo."
    );
    assert_eq!(
        t.rewrite(&format!("Use the [Get a repository]({page}) endpoint.")),
        "Use github_repos.get_repo."
    );
    // Link text that names an endpoint is the tool, with the words around it gone.
    assert_eq!(
        t.rewrite(
            "Use the [assignees endpoint](/rest/issues/assignees#list-assignees-of-an-issue)."
        ),
        "Use github_issues_pulls.list_issue_assignees."
    );
    // A page title inside a sentence reads as words, with the tool after them.
    assert_eq!(
        t.rewrite("You can manually [Create a check run](/rest/checks/runs#create-a-check-run)."),
        "You can manually create a check run (github_actions.create_check_run)."
    );
    // Text that says what to do gets the tool; a thing after an article does not.
    assert_eq!(
        t.rewrite(&format!(
            "You can [read the repository]({page}) or the [repository]({page})."
        )),
        "You can read the repository (github_repos.get_repo) or the repository."
    );
    // A left-out target, or a guide, keeps its text for plain().
    assert_eq!(
        t.rewrite("See \"[Update a repository](/rest/repos/repos#update-a-repository).\""),
        "See \"Update a repository.\""
    );
    let guide = "See [About repositories](https://docs.github.com/repositories/about).";
    assert_eq!(t.rewrite(guide), guide);
    // A method and path, also with Rails placeholders or other names, is the tool.
    assert_eq!(
        t.rewrite("Call `GET /repos/{owner}/{repo}/code-scanning/alerts/{number}`."),
        "Call github_security.get_code_scanning_alert."
    );
    assert_eq!(
        t.rewrite("Use GET /repos/:owner/:repo."),
        "Use github_repos.get_repo."
    );
    // The organization half of a pointer goes when organizations are left out.
    assert_eq!(
        t.rewrite(
            "Use `GET /repos/{owner}/{repo}` and `GET /orgs/{org}/members` for organizations."
        ),
        "Use github_repos.get_repo."
    );
    // "the ... endpoint" goes around a tool name, also across a line break.
    assert_eq!(
        t.rewrite("Use the `GET /repos/{owner}/{repo}`\nendpoint."),
        "Use github_repos.get_repo."
    );
}

#[test]
fn delete_notes_and_outputs() {
    use crate::generate::{
        answer_description, delete_note, null_when, outputs, NO_CONTENT, NULL_TOO, YES_NO_ANSWER,
    };
    assert_eq!(
        delete_note("Delete a release").as_deref(),
        Some(DELETE_NOTE)
    );
    assert_eq!(
        delete_note("Remove a label").as_deref(),
        Some("Remove only what the user asked for or what you added.")
    );
    assert_eq!(
        delete_note("Disable a workflow").as_deref(),
        Some("Disable only what the user asked for or what you enabled.")
    );
    // Undoing the user's own action needs no caution.
    assert_eq!(delete_note("Unstar a repository"), None);
    // Only what an answer's description says about its content is kept.
    assert_eq!(answer_description("Response", ""), "");
    assert_eq!(answer_description("Task created successfully", ""), "");
    assert_eq!(
        answer_description("Response if merge was successful", ""),
        ""
    );
    assert_eq!(
        answer_description("Response including the release's assets", ""),
        "Includes the release's assets."
    );
    assert_eq!(
        answer_description(
            "The repository",
            "Get a repository /repository/{owner}/{repo}"
        ),
        ""
    );
    assert_eq!(
        null_when("Response if repository is empty"),
        "Null if the repository is empty."
    );
    assert_eq!(
        null_when("Response when already merged"),
        "Null when already merged."
    );
    assert_eq!(null_when("A header with no content is returned."), NULL_TOO);

    let schema = json!({"application/json": {"schema": {"type": "object"}}});
    let list = json!({"application/json": {"schema": {"type": "array", "items": {}}}});
    let out = |method: &str, responses: Value| {
        let spec = json!({"paths": {"/x": {method: {"operationId": "x/y", "summary": "Get an x",
                                                     "responses": responses}}}});
        let ops = crate::generate::operations(&spec);
        outputs(&spec, &ops[0]).unwrap()
    };
    // An outcome-only description says nothing; a redirect is followed, so it is not an answer.
    assert_eq!(
        out(
            "get",
            json!({"200": {"description": "OK", "content": schema}, "301": {"description": "Moved"}})
        ),
        json!({"type": "object"})
    );
    // Two answers described differently: utcp does not say which came, so no description.
    assert_eq!(
        out(
            "post",
            json!({"200": {"description": "Reaction exists", "content": schema},
                            "201": {"description": "Reaction created", "content": schema}})
        ),
        json!({"type": "object"})
    );
    // A 204 next to a 200 says when the answer is null.
    assert_eq!(
        out(
            "get",
            json!({"200": {"description": "Response", "content": list},
                           "204": {"description": "Response if repository is empty"}})
        ),
        json!({"type": "array", "description": "Null if the repository is empty."})
    );
    // Only a 204: a write answers null when it worked; a GET is a yes/no check.
    assert_eq!(
        out("put", json!({"204": {"description": "Response"}})),
        json!({"type": "null", "description": NO_CONTENT})
    );
    assert_eq!(
        out(
            "get",
            json!({"204": {"description": "If the user is blocked"},
                           "404": {"description": "If the user is not blocked"}})
        ),
        json!({"type": "null", "description": YES_NO_ANSWER})
    );
}

#[test]
fn config_mistakes_fail() {
    let fails = |conf: &Config| {
        generate(&description(), API_VERSION, &source_json(), conf)
            .unwrap_err()
            .0
    };
    // A sentence fix whose text GitHub no longer has.
    let mut conf = test_config();
    conf.sentence_fixes
        .push(("repos/get".into(), "No such text.".into(), "x".into()));
    let e = fails(&conf);
    assert!(e.contains("repos/get: config sentence_fixes text"), "{e}");
    // A note naming a tool that does not exist.
    let mut conf = test_config();
    conf.notes
        .insert("repos/get".into(), "Use github_repos.no_such_tool.".into());
    let e = fails(&conf);
    assert!(
        e.contains("names github_repos.no_such_tool, which is not a tool"),
        "{e}"
    );
    // An operation the description does not have.
    let mut conf = test_config();
    conf.notes.insert("repos/no-such".into(), "x".into());
    let e = fails(&conf);
    assert!(e.contains("config names repos/no-such"), "{e}");
    // A variant of a left-out operation.
    let mut conf = test_config();
    let mut v = conf.variants[0].clone();
    v.op_id = "repos/update".into();
    v.name = "update_raw".into();
    conf.variants.push(v);
    let e = fails(&conf);
    assert!(
        e.contains("config variant update_raw is of repos/update, which is left out"),
        "{e}"
    );
    // Two tools with one name.
    let mut conf = test_config();
    conf.names.insert("repos/get".into(), "get_content".into());
    let e = fails(&conf);
    assert!(
        e.contains("tool name 'get_content' is used by repos/get and repos/get-content"),
        "{e}"
    );
}

#[test]
fn fit_whole_sentences() {
    let s = |text: &str, essential| Sentence {
        text: text.into(),
        essential,
    };
    // The essential sentence is kept even though an earlier one did not fit.
    let list = [
        s("One two.", false),
        s("Three four five.", false),
        s("Classic tokens need the repo scope.", true),
    ];
    assert_eq!(
        fit(&list, 50),
        "One two. Classic tokens need the repo scope."
    );
    assert_eq!(
        fit(&list, 100),
        "One two. Three four five. Classic tokens need the repo scope."
    );
    // A lead-in left last goes.
    let list = [
        s("Lists runs.", false),
        s("For example:", false),
        s("A long example sentence that does not fit.", false),
    ];
    assert_eq!(fit(&list, 30), "Lists runs.");
    // Only a first sentence that alone is too long is cut, at a word.
    let list = [s(
        "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu",
        false,
    )];
    assert_eq!(
        fit(&list, 50),
        "alpha beta gamma delta epsilon zeta eta theta..."
    );
    assert_eq!(fit(&[], 50), "");
}

#[test]
fn end_sentence_after_words_only() {
    for text in [
        "Default: false",
        "a branch or tag",
        "branch/PR",
        "off-topic",
        "the owner's",
        "(an existing option)",
        "Use `main`",
    ] {
        assert_eq!(end_sentence(text), format!("{text}."), "{text}");
    }
    for text in [
        "https://example.com",
        "docs/README.md",
        "max 100",
        "\"open\"",
        "Done.",
        "Why?",
        "-x",
        "",
    ] {
        assert_eq!(end_sentence(text), text, "{text}");
    }
}

#[test]
fn one_of_descriptions_move_up() {
    let spec = json!({});
    let convert = |schema: Value| crate::generate::convert(&spec, &schema, 0, &[]).unwrap();
    // The sentences all branches start with move up.
    let out = convert(json!({"oneOf": [
        {"type": "integer", "description": "The milestone. Its number."},
        {"type": "string", "description": "The milestone. Its title."}
    ]}));
    assert_eq!(out["description"], "The milestone.");
    assert_eq!(out["oneOf"][0]["description"], "Its number.");
    assert_eq!(out["oneOf"][1]["description"], "Its title.");
    // The only described branch's description moves up whole.
    let out = convert(json!({"anyOf": [
        {"type": "integer", "description": "The milestone number."},
        {"type": "null"}
    ]}));
    assert_eq!(out["description"], "The milestone number.");
    assert!(out["anyOf"][0].get("description").is_none());
    // Nothing shared, or a description of its own: nothing moves.
    let out = convert(json!({"oneOf": [
        {"type": "integer", "description": "A number."},
        {"type": "string", "description": "A title."}
    ]}));
    assert!(out.get("description").is_none());
    let out = convert(json!({"description": "Own.", "oneOf": [
        {"type": "integer", "description": "Shared."},
        {"type": "string", "description": "Shared."}
    ]}));
    assert_eq!(out["description"], "Own.");
    assert_eq!(out["oneOf"][0]["description"], "Shared.");
}

#[test]
fn shared_summaries_are_qualified() {
    let mut spec = description();
    spec["paths"]["/zen"]["get"]["summary"] = json!("Get the authenticated user");
    let e = generate(&spec, API_VERSION, &source_json(), &test_config()).unwrap_err();
    assert!(
        e.0.contains("meta/get-zen: other tools have the same summary")
            && e.0.contains("add category 'meta'"),
        "{}",
        e.0
    );
    let mut conf = test_config();
    conf.summary_qualifiers
        .insert("meta".into(), "for the meta API".into());
    let (manuals, _) = generate(&spec, API_VERSION, &source_json(), &conf).unwrap();
    let tools = tools_by_name(&manuals);
    assert_eq!(
        tools["get_zen"]["description"],
        "Get the authenticated user for the meta API."
    );
    assert_eq!(
        tools["get_authenticated_user"]["description"],
        "Get the authenticated user for a user."
    );
    // Two tools of one category still collide.
    let mut spec = description();
    spec["paths"]["/repos/{owner}/{repo}/contents/{path}"]["get"]["summary"] =
        json!("Get a repository");
    let e = generate(&spec, API_VERSION, &source_json(), &test_config()).unwrap_err();
    assert!(e.0.contains("still have the same summary"), "{}", e.0);
}

#[test]
fn every_argument_is_described() {
    let key = |op: &str, name: &str| (op.to_string(), name.to_string());
    let mut spec = description();
    spec["paths"]["/search/code"]["get"]["parameters"][0]
        .as_object_mut()
        .unwrap()
        .remove("description");
    let e = generate(&spec, API_VERSION, &source_json(), &test_config()).unwrap_err();
    assert!(
        e.0.contains("search/code: argument 'q' has no description"),
        "{}",
        e.0
    );
    let mut conf = test_config();
    conf.input_descriptions
        .insert(key("search/code", "q"), "The query.".into());
    let (manuals, _) = generate(&spec, API_VERSION, &source_json(), &conf).unwrap();
    assert_eq!(
        tools_by_name(&manuals)["search_code"]["inputs"]["properties"]["q"]["description"],
        "The query."
    );
    // An entry for an argument GitHub describes, or one the tool does not have, or for an unknown operation, fails.
    let e = generate(&description(), API_VERSION, &source_json(), &conf).unwrap_err();
    assert!(e.0.contains("GitHub now describes argument 'q'"), "{}", e.0);
    let mut conf = test_config();
    conf.input_descriptions
        .insert(key("search/code", "nope"), "x".into());
    let e = generate(&description(), API_VERSION, &source_json(), &conf).unwrap_err();
    assert!(e.0.contains("which the tool does not have"), "{}", e.0);
    let mut conf = test_config();
    conf.input_descriptions
        .insert(key("search/no-such-thing", "q"), "x".into());
    let e = generate(&description(), API_VERSION, &source_json(), &conf).unwrap_err();
    assert!(e.0.contains("search/no-such-thing"), "{}", e.0);
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
    let steps = next_steps(false).join("\n");
    assert!(
        steps.contains(PAT_VAR)
            && steps.contains("utcp diff 'github_*'")
            && steps.contains("utcp promote 'github_*'"),
        "{steps}"
    );
    let steps = next_steps(true).join("\n");
    assert!(
        !steps.contains(PAT_VAR) && steps.contains("utcp promote 'github_*'"),
        "{steps}"
    );
    let doctor = |creds: Value| json!({"doctor": {"credentials": creds}});
    let cred = |var: &str, set: bool| json!({"env_var": var, "is_set": set});
    let set_in = crate::utcp::secret_is_set_in;
    assert_eq!(
        set_in(
            &doctor(json!([cred(PAT_VAR, false), cred(PAT_VAR, true)])),
            PAT_VAR
        ),
        Some(true)
    );
    assert_eq!(
        set_in(
            &doctor(json!([cred(PAT_VAR, false), cred("X", true)])),
            PAT_VAR
        ),
        Some(false)
    );
    assert_eq!(set_in(&doctor(json!([cred("X", true)])), PAT_VAR), None);
    assert_eq!(set_in(&json!({}), PAT_VAR), None);
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
    // Without the optional path: the repository root.
    let out = dry("github_repos.get_content", r#"{"owner": "o", "repo": "r"}"#);
    assert_eq!(out["url"], "https://api.github.com/repos/o/r/contents");
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
