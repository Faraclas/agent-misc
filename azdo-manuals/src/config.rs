//! Which spec areas go into which manual, and which non-GET operations are included.
//!
//! Area names are the folder names under `specification/` in Microsoft's spec repository. Every GET of an included
//! area becomes a tool unless it is listed in `skip_gets` or returns only binary content. Other methods are included
//! only when listed in `post_reads` (POSTs that only read, such as queries and searches) or `writes` (phase B).
//! DELETE and HEAD are never included.

use std::collections::{HashMap, HashSet};

pub const API_VERSION: &str = "7.1";
pub const ARCHIVE_REVISION: &str = "c7e66d986b369dce462d04c02288a6c26c155002";

#[derive(Clone, Debug)]
pub struct ManualConf {
    pub name: String,
    pub areas: Vec<String>,
    pub title: String,
    pub hint: String,
}

/// Everything the generator decides by configuration. `Config::default()` is the real one; tests build their own.
#[derive(Clone, Debug, Default)]
pub struct Config {
    pub manuals: Vec<ManualConf>,
    /// operationId values, per area. POSTs whose only effect is to return data.
    pub post_reads: HashMap<String, Vec<String>>,
    /// Phase B: chosen writes, each tested in the sandbox organization before it is listed here.
    pub writes: HashMap<String, Vec<String>>,
    /// Some reads accept only a write-level scope (Azure DevOps has no read scope for deployment groups or
    /// environments). Those reads are left out so the PAT can stay read-only, unless their scope is listed here.
    pub allow_write_scopes: HashSet<String>,
    /// GETs left out on purpose, per area and operationId, with the reason.
    pub skip_gets: HashMap<String, HashMap<String, String>>,
    /// Parameters the spec marks optional but the service requires (it answers 400 without them), per area and
    /// operationId. Found by the live read test.
    pub required_params: HashMap<String, HashMap<String, Vec<String>>>,
    /// Explicit tool names, per area and operationId, for names the automatic rule gets wrong or makes clash.
    pub names: HashMap<String, HashMap<String, String>>,
    /// Path parameters the spec marks `x-ms-required: false` that are left out of the URL (with their segment), per
    /// area. utcp cannot leave a segment out per call (an unfilled `{team}` stays in the URL), so each is either
    /// always required or always dropped.
    pub drop_optional_path_params: HashMap<String, HashSet<String>>,
}

pub fn manual(name: &str, areas: &[&str], title: &str, hint: &str) -> ManualConf {
    ManualConf {
        name: name.into(),
        areas: areas.iter().map(|a| a.to_string()).collect(),
        title: title.into(),
        hint: hint.into(),
    }
}

pub fn list_map(items: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
    items
        .iter()
        .map(|(k, v)| (k.to_string(), v.iter().map(|s| s.to_string()).collect()))
        .collect()
}

pub fn nested<T: Clone>(items: &[(&str, &[(&str, T)])]) -> HashMap<String, HashMap<String, T>> {
    items
        .iter()
        .map(|(area, ops)| {
            (
                area.to_string(),
                ops.iter()
                    .map(|(op, v)| (op.to_string(), v.clone()))
                    .collect(),
            )
        })
        .collect()
}

impl Config {
    pub fn all_areas(&self) -> Vec<String> {
        self.manuals
            .iter()
            .flat_map(|m| m.areas.iter().cloned())
            .collect()
    }

    pub fn real() -> Config {
        let no_scope =
            "no scope in the spec; refused (401) by a PAT with every read scope".to_string();
        Config {
            manuals: vec![
                manual(
                    "azdo_git",
                    &["git", "policy"],
                    "Azure DevOps Git repositories, pull requests and branch policies",
                    "Azure DevOps repos and PRs",
                ),
                manual(
                    "azdo_work",
                    &["wit", "work"],
                    "Azure DevOps work items, queries, boards, iterations and backlogs",
                    "Azure DevOps work items and boards",
                ),
                manual(
                    "azdo_pipelines",
                    &["build", "pipelines", "release", "approvalsAndChecks", "distributedTask", "environments"],
                    "Azure DevOps builds, pipelines, releases, approvals, agent pools and environments",
                    "Azure DevOps builds and pipelines",
                ),
                manual(
                    "azdo_test",
                    &["testResults", "testPlan", "test"],
                    "Azure DevOps test plans, suites, cases, runs and results",
                    "Azure DevOps test plans and results",
                ),
                manual(
                    "azdo_wiki_search",
                    &["wiki", "search"],
                    "Azure DevOps wikis and code, wiki and work item search",
                    "Azure DevOps wiki and search",
                ),
                // No "profile": its only operation lives on app.vssps.visualstudio.com, outside the organization,
                // and answered 401 to an organization-scoped PAT (2026-11-20), the kind of PAT these manuals are
                // meant for.
                manual(
                    "azdo_core",
                    &["core", "graph", "ims"],
                    "Azure DevOps projects, teams, processes, users, groups and identities",
                    "Azure DevOps projects, teams, users",
                ),
            ],
            post_reads: list_map(&[
                (
                    "wit",
                    &[
                        "Wiql_Query By Wiql",
                        "Work Items_Get Work Items Batch",
                        "Queries_Get Queries Batch",
                        "Artifact Uri Query_Query",
                    ],
                ),
                (
                    "search",
                    &[
                        "Code Search Results_Fetch Code Search Results",
                        "Wiki Search Results_Fetch Wiki Search Results",
                        "Work Item Search Results_Fetch Work Item Search Results",
                    ],
                ),
                ("graph", &["Subject Lookup_Lookup Subjects", "Subject Query_Query"]),
            ]),
            writes: HashMap::new(),
            allow_write_scopes: HashSet::new(),
            skip_gets: nested(&[
                (
                    "build",
                    // Answers 404 "This API is being tested for whether it can be deprecated in the future"
                    // (2026-11-20).
                    &[("History_Get", "Microsoft is testing its deprecation (404)".to_string())],
                ),
                (
                    "testResults",
                    // The spec lists no scope for these. A PAT with all the read scopes the report lists answers
                    // 401, a full-access PAT answers 200 (2026-11-20), so a read-only PAT cannot use them.
                    &[
                        ("Settings_Get", no_scope.clone()),
                        ("Resultsummarybypipeline_Query", no_scope.clone()),
                        ("Resultsgroup Details_Test Results Group Details", no_scope),
                    ],
                ),
            ]),
            required_params: nested(&[
                ("git", &[("Refs Favorites_List", vec!["repositoryId".to_string()])]),
                (
                    "build",
                    &[
                        ("Builds_Get Changes Between Builds", vec!["fromBuildId".into(), "toBuildId".into()]),
                        ("Leases_Get Retention Leases By Owner Id", vec!["ownerId".into()]),
                    ],
                ),
                (
                    "approvalsAndChecks",
                    &[("Check Configurations_List", vec!["resourceType".into(), "resourceId".into()])],
                ),
            ]),
            names: HashMap::new(),
            // In `wit`, `team` only matters for team-relative WIQL macros such as @CurrentIteration; in `work`
            // (boards, iterations, capacity) it selects the team, so it stays.
            drop_optional_path_params: HashMap::from([("wit".to_string(), HashSet::from(["team".to_string()]))]),
        }
    }
}
