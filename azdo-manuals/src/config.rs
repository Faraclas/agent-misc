//! Which spec areas go into which manual, and what is left out of them.
//!
//! Area names are the folder names under `specification/` in Microsoft's spec repository. Every area goes into a
//! manual unless `excluded_areas` gives the reason it does not. Every operation of an included area becomes a tool,
//! except: GETs listed in `skip_gets`, DELETEs not listed in `deletes`, and HEAD. A file answer or a file upload is
//! included too: utcp saves the file or reads the file to send. The PAT's permissions, not this list, decide what an
//! agent can do; leaving a tool out only spares agents an operation that cannot be undone or cannot work.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

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
    /// Areas of the spec folder that go into no manual, with the reason.
    pub excluded_areas: Vec<(String, String)>,
    /// operationId values, per area. POSTs whose only effect is to return data.
    pub post_reads: HashMap<String, Vec<String>>,
    /// Writes tested in a sandbox organization by `write-test`, each with a hand-written body in `bodies.rs`. The
    /// other writes are included too, marked "(untested)" in their description and tagged `untested`.
    pub tested_writes: HashMap<String, Vec<String>>,
    /// The DELETEs that are included; the others are left out because deleting cannot be undone.
    pub deletes: HashMap<String, Vec<String>>,
    /// Leave out every write, and the reads that only a write-level scope allows, for a read-only PAT.
    pub read_only: bool,
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
    /// Request body schemas that replace the spec's, per area and operationId (see `bodies.rs`).
    pub body_overrides: HashMap<String, HashMap<String, Value>>,
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

    /// Leave out every write, for a read-only PAT.
    pub fn read_only(mut self) -> Config {
        self.read_only = true;
        self
    }

    /// Every operationId the configuration names, as (setting, area, operationId).
    pub fn named_operations(&self) -> Vec<(&'static str, &str, &str)> {
        let mut out = Vec::new();
        for (setting, map) in [
            ("post_reads", &self.post_reads),
            ("tested_writes", &self.tested_writes),
            ("deletes", &self.deletes),
        ] {
            for (area, ops) in map {
                out.extend(ops.iter().map(|op| (setting, area.as_str(), op.as_str())));
            }
        }
        let keyed = [
            (
                "skip_gets",
                self.skip_gets
                    .iter()
                    .map(|(a, m)| (a, m.keys().collect::<Vec<_>>()))
                    .collect::<Vec<_>>(),
            ),
            (
                "required_params",
                self.required_params
                    .iter()
                    .map(|(a, m)| (a, m.keys().collect()))
                    .collect(),
            ),
            (
                "names",
                self.names
                    .iter()
                    .map(|(a, m)| (a, m.keys().collect()))
                    .collect(),
            ),
            (
                "body_overrides",
                self.body_overrides
                    .iter()
                    .map(|(a, m)| (a, m.keys().collect()))
                    .collect(),
            ),
        ];
        for (setting, areas) in keyed {
            for (area, ops) in areas {
                out.extend(
                    ops.into_iter()
                        .map(|op| (setting, area.as_str(), op.as_str())),
                );
            }
        }
        out.sort();
        out
    }

    pub fn real() -> Config {
        let no_scope = "no scope in the spec; 401 without a full-access PAT".to_string();
        Config {
            manuals: vec![
                manual(
                    "azdo_git",
                    &["git", "policy", "tfvc", "advancedSecurity"],
                    "Azure DevOps Git repositories, pull requests, branch policies and TFVC",
                    "Azure DevOps repos and PRs",
                ),
                manual(
                    "azdo_work",
                    &["wit", "work", "dashboard", "processes", "processadmin", "processDefinitions"],
                    "Azure DevOps work items, queries, boards, iterations, backlogs, dashboards and processes",
                    "Azure DevOps work items, boards, dashboards",
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
                    "Azure DevOps wikis and code, wiki, work item and package search",
                    "Azure DevOps wiki and search",
                ),
                manual(
                    "azdo_core",
                    &["core", "graph", "ims", "operations", "favorite", "status", "resourceUsage"],
                    "Azure DevOps projects, teams, processes, users, groups, identities, favorites and service status",
                    "Azure DevOps projects, teams, users",
                ),
                manual(
                    "azdo_admin",
                    &[
                        "security",
                        "securityRoles",
                        "permissionsReport",
                        "serviceEndpoint",
                        "hooks",
                        "notification",
                        "extensionManagement",
                        "memberEntitlementManagement",
                        "audit",
                        "delegatedAuth",
                    ],
                    "Azure DevOps permissions, security roles, service connections, service hooks, notifications, \
                     extensions, user entitlements and the audit log",
                    "Azure DevOps permissions, connections, hooks",
                ),
                manual(
                    "azdo_artifacts",
                    &["artifacts", "artifactsPackageTypes", "symbol"],
                    "Azure DevOps Artifacts feeds, packages (npm, NuGet, Maven, Python, Universal) and symbols",
                    "Azure DevOps feeds, packages, symbols",
                ),
            ],
            excluded_areas: [
                (
                    "profile",
                    "hosted outside the organization; refused (401) an organization's PAT (2026-11-20)",
                ),
                ("account", "hosted outside the organization (app.vssps.visualstudio.com)"),
                ("tokens", "creates and revokes PATs"),
                ("tokenAdmin", "revokes PATs of other users"),
                ("tokenAdministration", "revokes PATs of other users; replaced by tokenAdmin after 5.2"),
                ("governance", "its 7.1 folder has examples only, no API description"),
            ]
            .iter()
            .map(|(a, r)| (a.to_string(), r.to_string()))
            .collect(),
            post_reads: list_map(&[
                ("approvalsAndChecks", &["Check Configurations_Query"]),
                (
                    "artifacts",
                    &[
                        "Artifact  Details_Query Package Metrics",
                        "Artifact  Details_Query Package Version Metrics",
                    ],
                ),
                ("build", &["Builds_Get Build Work Items Refs From Commits"]),
                (
                    "git",
                    &["Commits_Get Commits Batch", "Items_Get Items Batch", "Pull Request Query_Get"],
                ),
                ("graph", &["Subject Lookup_Lookup Subjects", "Subject Query_Query"]),
                (
                    "hooks",
                    &[
                        "Notifications_Query",
                        "Publishers_Query Input Values",
                        "Publishers_Query Publishers",
                        "Subscriptions_Create Subscriptions Query",
                    ],
                ),
                ("notification", &["Subscriptions_Query"]),
                ("pipelines", &["Preview_Preview"]),
                (
                    "search",
                    &[
                        "Code Search Results_Fetch Code Search Results",
                        "Package Search Results_Fetch Package Search Results",
                        "Wiki Search Results_Fetch Wiki Search Results",
                        "Work Item Search Results_Fetch Work Item Search Results",
                    ],
                ),
                ("security", &["Permissions_Has Permissions Batch"]),
                ("test", &["Points_Get Points By Query", "Test History_Query"]),
                (
                    "testResults",
                    &[
                        "Filecoverage_Get",
                        "History_Query",
                        "Result Meta Data_Query",
                        "Result Trend By Build_Query",
                        "Result Trend By Release_Query",
                        "Results_Get Test Results By Query",
                        "Results_Get Test Results By Query Wiql",
                        "Resultsummarybyrelease_Query Test Results Summary For Releases",
                        "Resultsummarybyrequirement_Query",
                        "Test History_Query",
                        "Workitems_Query Test Method Linked Work Items",
                    ],
                ),
                ("tfvc", &["Changesets_Get Batched Changesets", "Items_Get Items Batch"]),
                ("wiki", &["Pages Batch_Get"]),
                (
                    "wit",
                    &[
                        "Wiql_Query By Wiql",
                        "Work Items_Get Work Items Batch",
                        "Queries_Get Queries Batch",
                        "Artifact Uri Query_Query",
                        "Reporting Work Item Revisions_Read Reporting Revisions Post",
                    ],
                ),
            ]),
            // What an agent needs most for code review and tracking: open and update PRs, comment on, reply to and
            // resolve their threads, create and update work items, queue and cancel builds. Pushing code goes
            // through `git push`, not the REST API.
            tested_writes: list_map(&[
                (
                    "git",
                    &[
                        "Pull Requests_Create",
                        "Pull Requests_Update",
                        "Pull Request Threads_Create",
                        "Pull Request Threads_Update",
                        "Pull Request Thread Comments_Create",
                        "Pull Request Thread Comments_Update",
                    ],
                ),
                (
                    "wit",
                    &["Work Items_Create", "Work Items_Update", "Attachments_Create"],
                ),
                ("build", &["Builds_Queue", "Builds_Update Build"]),
            ]),
            // Deletes of small things an agent adds and may need to take back: comments, reactions, labels,
            // reviewers, statuses, tags, wiki pages, widgets, favorites, and test cases in a suite. Work items go to
            // the recycle bin. Projects, repositories, branches, pipelines, feeds, packages, users and permissions are
            // not deleted by agents.
            deletes: list_map(&[
                (
                    "git",
                    &[
                        "Pull Request Attachments_Delete",
                        "Pull Request Comment Likes_Delete",
                        "Pull Request Labels_Delete",
                        "Pull Request Reviewers_Delete",
                        "Pull Request Statuses_Delete",
                        "Pull Request Iteration Statuses_Delete",
                        "Pull Request Thread Comments_Delete",
                        "Refs Favorites_Delete",
                    ],
                ),
                ("build", &["Tags_Delete Build Tag", "Tags_Delete Definition Tag"]),
                ("wit", &["Comments Reactions_Delete", "Comments_Delete", "Work Items_Delete"]),
                ("test", &["Test  Suites_Remove Test Cases From Suite Url"]),
                ("testPlan", &["Suite Test Case_Remove Test Cases List From Suite"]),
                ("wiki", &["Pages_Delete Page", "Pages_Delete Page By Id"]),
                ("dashboard", &["Widgets_Delete"]),
                ("favorite", &["Favorites_Delete Favorite By Id"]),
            ]),
            read_only: false,
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
            // File answers and uploads whose client method name alone says too little.
            names: HashMap::from([
                (
                    "permissionsReport".to_string(),
                    HashMap::from([(
                        "Permissions Report Download_Download".to_string(),
                        "download_permissions_report".to_string(),
                    )]),
                ),
                (
                    "processadmin".to_string(),
                    HashMap::from([
                        (
                            "Processes_Export Process Template".to_string(),
                            "export_process_template".to_string(),
                        ),
                        (
                            "Processes_Import Process Template".to_string(),
                            "import_process_template".to_string(),
                        ),
                    ]),
                ),
                (
                    "release".to_string(),
                    HashMap::from([("Releases_Get Logs".to_string(), "get_release_logs".to_string())]),
                ),
            ]),
            // In `wit`, `team` only matters for team-relative WIQL macros such as @CurrentIteration; in `work`
            // (boards, iterations, capacity) it selects the team, so it stays.
            drop_optional_path_params: HashMap::from([("wit".to_string(), HashSet::from(["team".to_string()]))]),
            body_overrides: crate::bodies::real(),
        }
    }
}
