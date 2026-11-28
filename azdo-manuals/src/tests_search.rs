//! Offline tests of `search-test`: the case format, and pass/fail/skip on canned search results. One end-to-end test
//! searches the fixture manual in a throwaway utcp; like the load test it is skipped without a utcp binary.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::fs;

use crate::cli::help;
use crate::searchtest::{
    check, check_all, failed, parse_cases, report, run, Case, Status, DEFAULT_CASES, SEARCH_LIMIT,
};
use crate::tests::{expected_dir, find_utcp};
use crate::utcp::Utcp;

fn case(query: &str, tools: &[&str], known_miss: Option<&str>) -> Case {
    Case {
        line: 1,
        query: query.into(),
        tools: tools.iter().map(|t| t.to_string()).collect(),
        known_miss: known_miss.map(String::from),
    }
}

fn names(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn built_in_cases_parse() {
    let cases = parse_cases(DEFAULT_CASES).unwrap();
    assert_eq!(cases.len(), 20);
    assert_eq!(
        cases[0],
        Case {
            line: 2,
            query: "who am i".into(),
            tools: names(&[
                "azdo_core.get_connection_data",
                "github_users.get_authenticated_user"
            ]),
            known_miss: None,
        }
    );
    let known: Vec<&Case> = cases.iter().filter(|c| c.known_miss.is_some()).collect();
    let known_queries: Vec<&str> = known.iter().map(|c| c.query.as_str()).collect();
    assert_eq!(known_queries, ["repository details", "list projects"]);
    assert_eq!(known[1].tools, names(&["azdo_core.get_projects"]));
    assert_eq!(
        known[1].known_miss.as_deref(),
        Some("Azure DevOps calls it \"Get all projects\", and search has no synonyms")
    );
    // The file is public: it names tools only, never an organization.
    assert!(!DEFAULT_CASES.contains("dev.azure.com"));
}

#[test]
fn cases_take_comments_alternatives_and_known_misses() {
    let text = "\
# a comment line
   # an indented one

who am i -> a.b   # a trailing comment
c# sdk -> a.c|a.d |  e-f.g_h
slow -> a.b [known miss: not found # yet]
";
    let cases = parse_cases(text).unwrap();
    assert_eq!(cases.len(), 3);
    assert_eq!((cases[0].line, cases[0].query.as_str()), (4, "who am i"));
    assert_eq!(cases[0].tools, names(&["a.b"]));
    assert_eq!(
        cases[1].query, "c# sdk",
        "a # inside a word is not a comment"
    );
    assert_eq!(cases[1].tools, names(&["a.c", "a.d", "e-f.g_h"]));
    assert_eq!(cases[1].known_miss, None);
    assert_eq!(cases[2].tools, names(&["a.b"]));
    assert_eq!(cases[2].known_miss.as_deref(), Some("not found # yet"));
    assert!(parse_cases("# only comments\n\n").unwrap().is_empty());
}

#[test]
fn malformed_cases_name_their_line() {
    for (line, why) in [
        (
            "who am i a.b",
            "expected `query -> manual.tool | manual.tool`",
        ),
        (" -> a.b", "no query"),
        ("--help -> a.b", "cannot start with `-`"),
        ("q ->", "a tool name is missing"),
        ("q -> a.b |", "a tool name is missing"),
        ("q -> a.b || a.c", "a tool name is missing"),
        ("q -> get_profile", "`get_profile` is not a tool name"),
        ("q -> a.b.c", "`a.b.c` is not a tool name"),
        ("q -> a.b c", "`a.b c` is not a tool name"),
        ("q -> a.b [known miss]", "expected `[known miss: reason]`"),
        ("q -> a.b [known miss: ]", "expected `[known miss: reason]`"),
        ("q -> a.b [maybe: x]", "expected `[known miss: reason]`"),
        (
            "q -> a.b [known miss: x] more",
            "expected `[known miss: reason]`",
        ),
        ("q -> a.b [known miss: x", "expected `[known miss: reason]`"),
    ] {
        let text = format!("# header\nok -> a.b\n{line}\n");
        let err = parse_cases(&text).unwrap_err();
        assert!(err.starts_with("line 3: "), "{line}: {err}");
        assert!(err.contains(why), "{line}: {err}");
        assert!(err.ends_with(line.trim()), "{line}: {err}");
    }
}

#[test]
fn a_case_passes_in_the_top_n_and_fails_below() {
    let registered = set(&["azdo_core"]);
    let c = case("me", &["azdo_core.get_connection_data"], None);
    let found = names(&["x.y", "azdo_core.get_connection_data", "x.z"]);
    let got = check(&c, &registered, &found, 3);
    assert_eq!(
        (got.rank, got.tool.as_str(), got.status),
        (Some(2), "azdo_core.get_connection_data", Status::Pass)
    );
    let got = check(&c, &registered, &found, 1);
    assert_eq!(
        (got.rank, got.status),
        (Some(2), Status::Fail),
        "found, but below the top N"
    );
    let got = check(&c, &registered, &names(&["x.y"]), 3);
    assert_eq!(
        (got.rank, got.tool.as_str(), got.status),
        (None, "azdo_core.get_connection_data", Status::Fail)
    );
    // Only the first SEARCH_LIMIT results count.
    let mut long = vec!["x.y".to_string(); SEARCH_LIMIT];
    long.push("azdo_core.get_connection_data".into());
    assert_eq!(check(&c, &registered, &long, 10).rank, None);
}

#[test]
fn the_best_ranked_registered_tool_counts() {
    let c = case(
        "who am i",
        &[
            "azdo_core.get_connection_data",
            "azdo_core.get_profile",
            "github_users.get_authenticated_user",
        ],
        None,
    );
    let found = names(&[
        "x.y",
        "azdo_core.get_profile",
        "azdo_core.get_connection_data",
    ]);
    let got = check(&c, &set(&["azdo_core"]), &found, 3);
    assert_eq!(
        (got.rank, got.tool.as_str(), got.status),
        (Some(2), "azdo_core.get_profile", Status::Pass)
    );
    // Not found: the line names the tools that could have been found, not those of an unregistered manual.
    let got = check(&c, &set(&["azdo_core"]), &[], 3);
    assert_eq!(
        got.tool,
        "azdo_core.get_connection_data | azdo_core.get_profile"
    );
    // A tool of an unregistered manual does not count even when it shows up.
    let found = names(&["github_users.get_authenticated_user"]);
    assert_eq!(
        check(&c, &set(&["azdo_core"]), &found, 3).status,
        Status::Fail
    );
}

#[test]
fn known_misses_are_reported_but_do_not_fail() {
    let c = case(
        "list projects",
        &["azdo_core.get_projects"],
        Some("no synonyms"),
    );
    let registered = set(&["azdo_core"]);
    let miss = check(&c, &registered, &names(&["x.y"]), 3);
    assert_eq!(miss.status, Status::Known);
    let low = check(
        &c,
        &registered,
        &names(&["x.a", "x.b", "x.c", "azdo_core.get_projects"]),
        3,
    );
    assert_eq!((low.rank, low.status), (Some(4), Status::Known));
    let hit = check(&c, &registered, &names(&["azdo_core.get_projects"]), 3);
    assert_eq!(hit.status, Status::Pass);
    assert!(!failed(&[miss.clone(), low, hit.clone()]));
    let lines = report(&[miss, hit], 3).join("\n");
    assert!(lines.contains("KNOWN (no synonyms)"), "{lines}");
    assert!(
        lines.contains("Known misses found now, remove their marker: line 1 (list projects)."),
        "{lines}"
    );
}

#[test]
fn cases_without_a_registered_manual_are_skipped_without_searching() {
    let cases = vec![
        case("me", &["github_users.get_authenticated_user"], None),
        case("work item", &["azdo_work.get_work_item"], None),
    ];
    let searches = Cell::new(0);
    let checked = check_all(&cases, &set(&["azdo_work"]), 3, |query| {
        searches.set(searches.get() + 1);
        assert_eq!(query, "work item");
        Ok(names(&["azdo_work.get_work_item"]))
    })
    .unwrap();
    assert_eq!(searches.get(), 1);
    assert_eq!(checked[0].status, Status::Skip);
    assert_eq!(checked[0].rank, None);
    assert_eq!(checked[0].tool, "github_users.get_authenticated_user");
    assert_eq!(checked[1].status, Status::Pass);
    assert!(!failed(&checked));
    // A search that fails stops the test.
    let err = check_all(&cases, &set(&["azdo_work"]), 3, |_| {
        Err("daemon down".into())
    });
    assert_eq!(err.unwrap_err(), "daemon down");
}

#[test]
fn the_report_has_one_line_per_case_and_the_totals() {
    let registered = set(&["a"]);
    let cases = [
        case("first", &["a.one"], None),
        case("third", &["a.three"], None),
        case("fifth", &["a.five"], None),
        case("lost", &["a.lost"], None),
        case("hard", &["a.hard"], Some("odd wording")),
        case("other", &["b.x"], None),
    ];
    let found = names(&["a.one", "x.2", "a.three", "x.4", "a.five"]);
    let checked: Vec<_> = cases
        .iter()
        .map(|c| check(c, &registered, &found, 3))
        .collect();
    assert!(failed(&checked));
    let lines = report(&checked, 3);
    assert_eq!(
        lines,
        [
            "query  rank  tool     result",
            "first     1  a.one    PASS",
            "third     3  a.three  PASS",
            "fifth     5  a.five   FAIL",
            "lost      -  a.lost   FAIL",
            "hard      -  a.hard   KNOWN (odd wording)",
            "other     -  b.x      SKIP (no manual of these tools is registered)",
            "",
            "6 cases: 5 searched, 1 skipped.",
            "Found: top-1 1 of 5, top-3 2 of 5, top-5 3 of 5.",
            "1 known miss; 2 failures (not in the top 3).",
        ]
    );
    let none = report(&checked[5..], 3);
    assert_eq!(
        none.last().unwrap(),
        "Nothing was searched: no case names a registered manual (staged manuals need --staged)."
    );
}

#[test]
fn search_test_has_help() {
    let text = help(Some("search-test")).render().to_string();
    for word in [
        "--staged",
        "--cases",
        "--top",
        "--utcp",
        "[default: 3]",
        "known miss",
    ] {
        assert!(text.contains(word), "{word}: {text}");
    }
}

/// The fixture manual, active in a throwaway utcp and staged under a second name, searched with a small case file.
#[test]
fn search_test_finds_the_fixture_tools_in_utcp() {
    let Some(bin) = find_utcp() else {
        eprintln!("skipped: no utcp binary (set UTCP_BIN)");
        return;
    };
    let utcp = Utcp::isolated(bin, "azdo-search-test-").unwrap();
    utcp.register(&expected_dir(), [&"t_main".to_string()])
        .unwrap();
    let file = expected_dir().join("t_main.utcp.json");
    utcp.run_for_test(&["stage", "t_staged", file.to_str().unwrap()])
        .unwrap();
    assert_eq!(utcp.registered(false).unwrap(), set(&["t_main"]));
    assert_eq!(utcp.registered(true).unwrap(), set(&["t_main", "t_staged"]));

    let dir = tempfile::tempdir().unwrap();
    let cases_file = dir.path().join("cases.txt");
    fs::write(
        &cases_file,
        "\
# The fixture manual's tools
discussion threads -> t_main.list_threads
pools -> t_main.list_pools
xyzzy -> t_main.get_zip [known miss: no tool says xyzzy]
staged pools -> t_staged.list_pools
someone else -> github_users.get_authenticated_user
",
    )
    .unwrap();
    let cases = parse_cases(&fs::read_to_string(&cases_file).unwrap()).unwrap();

    let active = run(&utcp, &cases, false, 3).unwrap();
    let status: Vec<Status> = active.iter().map(|c| c.status).collect();
    assert_eq!(
        status,
        [
            Status::Pass,
            Status::Pass,
            Status::Known,
            Status::Skip,
            Status::Skip
        ],
        "{:#?}",
        report(&active, 3)
    );
    assert_eq!(active[0].rank, Some(1), "{:#?}", report(&active, 3));
    assert!(!failed(&active));

    let staged = run(&utcp, &cases, true, 3).unwrap();
    assert_eq!(staged[3].status, Status::Pass, "{:#?}", report(&staged, 3));
    assert_eq!(staged[3].tool, "t_staged.list_pools");
    assert_eq!(staged[4].status, Status::Skip);
}
