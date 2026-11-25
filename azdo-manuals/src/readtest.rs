//! Live read test: call every read tool of the generated manuals through the user's utcp, chaining IDs.
//!
//! Each call goes through `utcp call` exactly as agents make it (the daemon when it runs, with the secrets utcp
//! already has), with `--staged` for manuals that are staged, so the staged manuals can be checked before they are
//! promoted. The tool definitions come from the generated files: path parameters are filled from earlier responses,
//! so a tool `.../pullrequests/{pullRequestId}` takes the id of the first item the list tool `.../pullrequests`
//! returned. `project` (and `team`) come from the command line, `--seed` sets any other parameter by name. Tools whose
//! parameters cannot be filled are reported as not exercised.
//!
//! Only GETs run, plus the read-only POSTs that have a sample body below. Printed output holds tool names, HTTP
//! status codes and sizes only; the details file (written next to the manuals, which are private anyway) also holds
//! the values used, since they come from the organization.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::LazyLock;
use std::time::Duration;

use indexmap::IndexMap;
use regex::Regex;
use serde_json::{json, Map, Value};

use crate::text::py_str;
use crate::utcp::Utcp;

/// Sample bodies for the read-only POSTs, by tool name. Strings `@tool:path` are taken from an earlier response.
static POST_BODIES: LazyLock<IndexMap<&'static str, Value>> = LazyLock::new(|| {
    IndexMap::from([
        (
            "query_by_wiql",
            json!({
                "body": {"query": "Select [System.Id] From WorkItems Where [System.TeamProject] = @project \
                                   Order By [System.ChangedDate] Desc"},
                "$top": 5,
            }),
        ),
        (
            "fetch_code_search_results",
            json!({"body": {"searchText": "README", "$top": 5}}),
        ),
        (
            "fetch_wiki_search_results",
            json!({"body": {"searchText": "README", "$top": 5}}),
        ),
        (
            "fetch_work_item_search_results",
            json!({"body": {"searchText": "test", "$top": 5}}),
        ),
        (
            "query_subjects",
            json!({"body": {"query": "Project Collection Administrators", "subjectKind": ["Group"]}}),
        ),
        (
            "lookup_subjects",
            json!({"body": {"lookupKeys": [{"descriptor": "@list_users:value.0.descriptor"}]}}),
        ),
        (
            "get_work_items_batch",
            json!({"body": {"ids": ["@query_by_wiql:workItems.0.id"], "fields": ["System.Title"]}}),
        ),
        (
            "get_queries_batch",
            json!({"body": {"ids": ["@get_query:id"]}}),
        ),
        (
            "query_work_items_for_artifact_uris",
            json!({"body": {"artifactUris": ["@get_commit:_links.self.href"]}}),
        ),
    ])
});

/// Where values that cannot be chained by URL come from: `tool.param` (one tool) or `param` (any tool). A value is
/// a literal, `@tool:dotted.path` (from that tool's first successful response; a `[key=value]` part picks the first
/// list item whose `key` is `value`), `@seed:name` (a --project/--team/--seed value), or a string with
/// `{@tool:path}` parts.
static SOURCES: LazyLock<IndexMap<&'static str, Value>> = LazyLock::new(|| {
    let strings: &[(&str, &str)] = &[
        ("team", "@get_project:defaultTeam.name"),
        ("get_project.projectId", "@seed:project"),
        ("get_check_configurations_on_resource.resourceType", "queue"),
        ("get_check_configurations_on_resource.resourceId", "@get_agent_queues:value.0.id"),
        ("workItemId", "@query_by_wiql:workItems.0.id"),
        ("get_work_item.id", "@query_by_wiql:workItems.0.id"),
        ("get_revisions.id", "@query_by_wiql:workItems.0.id"),
        ("get_revision.id", "@query_by_wiql:workItems.0.id"),
        ("get_revision.revisionNumber", "@get_revisions:value.0.rev"),
        ("get_updates.id", "@query_by_wiql:workItems.0.id"),
        ("get_update.id", "@query_by_wiql:workItems.0.id"),
        ("get_update.updateNumber", "@get_updates:value.0.id"),
        ("get_work_items.ids", "@query_by_wiql:workItems.0.id"),
        ("get_work_item_next_states_on_checkin_action.ids", "@query_by_wiql:workItems.0.id"),
        ("query_by_id.id", "@get_queries:value.0.children.0.id"),
        ("get_query.query", "@get_queries:value.0.id"),
        ("search_queries.$filter", "bug"),
        ("get_work_item_template.type", "@get_work_item_types:value.0.name"),
        ("get_classification_nodes.ids", "@get_root_nodes:value.0.id"),
        ("get_total_iteration_capacities.iterationId", "@get_team_iterations:value.0.id"),
        ("subjectDescriptor", "@list_users:value.0.descriptor"),
        ("storageKey", "@get_storage_key:value"),
        ("environmentId", "@get_release:environments.0.id"),
        ("get_release_revision.definitionSnapshotRevision", "@get_release:definitionSnapshotRevision"),
        ("timelineId", "@get_build:orchestrationPlan.planId"),
        ("definition", "@get_definitions:value.0.id"),
        ("get_definition_tags.definitionId", "@get_definitions:value.0.id"),
        // Both builds must be of the same pipeline; the newest build twice always is.
        ("fromBuildId", "@get_builds:value.0.id"),
        ("toBuildId", "@get_builds:value.0.id"),
        ("build_get_folders.path", "\\"),
        ("release_get_folders.path", "\\"),
        ("get_tree.sha1", "@get_commit:treeId"),
        ("get_branch.name", "@get_branches:value.0.name"),
        ("get_merge_bases.otherCommitId", "@get_commits:value.1.commitId"),
        ("get_item.path", "/"),
        ("get_push_commits.pushId", "@get_pushes:value.0.pushId"),
        ("get_repository_with_parent.includeParent", ""),
        (
            "get_policy_evaluations.artifactId",
            "vstfs:///CodeReview/CodeReviewId/{@get_repository:project.id}/{@get_pull_requests:value.0.pullRequestId}",
        ),
        ("get_policy_evaluation.evaluationId", "@get_policy_evaluations:value.0.evaluationId"),
        ("get_relation_type.relation", "@get_relation_types:value.0.referenceName"),
        ("get_policy_configuration_revision.revisionId", "@get_policy_configuration_revisions:value.0.revision"),
        ("read_identities.subjectDescriptors", "@list_users:value.0.descriptor"),
        ("buildId", "@get_builds:value.0.id"),
        ("build", "@get_builds:value.0.id"),
        ("pipelineId", "@list_pipelines:value.0.id"),
        ("poolIds", "@get_agent_pools:value.0.id"),
        ("queueIds", "@get_agent_queues:value.0.id"),
        ("queueNames", "@get_agent_queues:value.0.name"),
        ("releaseId", "@get_releases:value.0.id"),
        ("releaseEnvId", "@get_release:environments.0.id"),
        ("planId", "@get_test_plans:value.0.id"),
        ("get_work_item_columns.iterationId", "@get_team_iterations:value.0.id"),
        // The built-in processes refuse layout reads ("work item types are locked"); inherited ones answer.
        ("get_form_layout.processId", "@get_list_of_processes:value.[customizationType=inherited].typeId"),
        ("get_system_controls.processId", "@get_list_of_processes:value.[customizationType=inherited].typeId"),
        ("get_iteration_chart_image.iterationId", "@get_team_iterations:value.0.id"),
        // Analytics snapshot sets time out unless filtered to a few days; the board snapshot answers only aggregations.
        ("query_work_item_snapshot.$filter", "DateSK ge {@date:-3}"),
        (
            "query_work_item_board_snapshot.$apply",
            "filter(DateSK eq {@date:-1})/aggregate($count as Count)",
        ),
        ("query_test_point_history_snapshot.$filter", "DateSK ge {@date:-3}"),
        ("query_test_results_daily.$filter", "DateSK ge {@date:-3}"),
        ("query_task_agent_request_snapshots.$filter", "SamplingDateSK ge {@date:-3}"),
        ("query_pipeline_run_activity_results.$filter", "PipelineRunCompletedDateSK ge {@date:-3}"),
        ("repositoryType", "TfsGit"),
        ("get_configurations.branch", "main"),
        ("get_detected_build_frameworks.branch", "main"),
        ("get_containers.artifactUris", "vstfs:///Build/Build/{@get_builds:value.0.id}"),
    ];
    strings
        .iter()
        .map(|(k, v)| {
            // The one non-string source: a boolean flag.
            let value = if *k == "get_repository_with_parent.includeParent" {
                json!(true)
            } else {
                json!(v)
            };
            (*k, value)
        })
        .collect()
});

const LIMIT_PARAMS: [(&str, i64); 3] = [("$top", 5), ("top", 5), ("searchCriteria.$top", 5)];
const ID_KEYS: [&str; 6] = ["id", "name", "descriptor", "key", "objectId", "originId"];
static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{([^}]+)\}").unwrap());
static REF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{@([a-z0-9_]+):([^}]*)\}").unwrap());
static WHOLE_REF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^@([a-z0-9_]+):(.*)$").unwrap());
static DATE_REF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{@date:(-?[0-9]+)\}").unwrap());

pub fn template_of(tool: &Value) -> String {
    let url = tool["tool_call_template"]["url"].as_str().unwrap_or("");
    url.split('?').next().unwrap_or("").replace("%24{", "${")
}

pub fn normalize(template: &str) -> String {
    PLACEHOLDER.replace_all(template, "{}").to_lowercase()
}

pub fn first_item(response: &Value) -> Option<&Value> {
    match response {
        Value::Object(o) => {
            for key in ["value", "items", "results", "comments"] {
                if let Some(Value::Array(a)) = o.get(key) {
                    return a.first();
                }
            }
            Some(response)
        }
        Value::Array(a) => a.first(),
        _ => None,
    }
}

/// A string, integer or boolean (Python's `isinstance(v, (str, int))`), not empty.
fn usable_id(v: &Value) -> bool {
    match v {
        Value::String(s) => !s.is_empty(),
        Value::Number(n) => n.is_i64() || n.is_u64(),
        Value::Bool(_) => true,
        _ => false,
    }
}

pub fn value_for(param: &str, item: Option<&Value>) -> Option<Value> {
    let item = item?.as_object()?;
    let mut lowered: HashMap<String, &String> = HashMap::new();
    for k in item.keys() {
        lowered.insert(k.to_lowercase(), k);
    }
    // Some resources are addressed by another key than their id or name: a process by its `typeId`, a work item
    // type, behavior or field by its `referenceName` (`witRefName`, `behaviorRefName`).
    let lower = param.to_lowercase();
    let preferred = if lower.ends_with("refname") {
        Some("referenceName")
    } else if lower.starts_with("process") {
        Some("typeId")
    } else {
        None
    };
    for key in std::iter::once(param).chain(preferred).chain(ID_KEYS) {
        if let Some(original) = lowered.get(&key.to_lowercase()) {
            let v = &item[original.as_str()];
            if usable_id(v) {
                return Some(v.clone());
            }
        }
    }
    None
}

/// A reference that cannot be resolved yet: the tool has not answered, or its answer lacks the path.
#[derive(Debug, PartialEq)]
pub struct Unresolved(pub String);

fn dig(data: &Value, path: &str) -> Result<Value, Unresolved> {
    let mut node = data;
    if !path.is_empty() {
        for part in path.split('.') {
            let filter = part
                .strip_prefix('[')
                .and_then(|p| p.strip_suffix(']'))
                .and_then(|p| p.split_once('='));
            let next = match node {
                // `[key=value]`: the first item whose `key` is the string `value`.
                Value::Array(a) if filter.is_some() => {
                    let (key, want) = filter.expect("checked");
                    a.iter().find(|item| item[key].as_str() == Some(want))
                }
                Value::Array(a) if !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()) => {
                    part.parse::<usize>().ok().and_then(|i| a.get(i))
                }
                Value::Object(o) => o.get(part),
                _ => None,
            };
            node = next.ok_or_else(|| Unresolved(path.to_string()))?;
        }
    }
    if node.is_null() || node.as_str() == Some("") {
        return Err(Unresolved(path.to_string()));
    }
    Ok(node.clone())
}

/// Replace `@tool:path` strings (and `{@tool:path}` parts) with values from earlier responses.
pub fn resolve(value: &Value, responses: &HashMap<String, Value>) -> Result<Value, Unresolved> {
    match value {
        Value::Object(o) => {
            let mut out = Map::new();
            for (k, v) in o {
                out.insert(k.clone(), resolve(v, responses)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(a) => a
            .iter()
            .map(|v| resolve(v, responses))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::String(s) => {
            // `{@date:-7}`: the local date that many days from today, as Analytics date keys write it (yyyyMMdd).
            let replaced = DATE_REF.replace_all(s, |c: &regex::Captures| {
                let days: i64 = c[1].parse().unwrap_or(0);
                (chrono::Local::now().date_naive() + chrono::TimeDelta::days(days))
                    .format("%Y%m%d")
                    .to_string()
            });
            let s: &str = &replaced;
            if let Some(m) = WHOLE_REF.captures(s) {
                let data = responses
                    .get(&m[1])
                    .ok_or_else(|| Unresolved(m[1].to_string()))?;
                return dig(data, &m[2]);
            }
            let mut out = String::new();
            let mut last = 0;
            for m in REF.captures_iter(s) {
                let whole = m.get(0).expect("group 0");
                let data = responses
                    .get(&m[1])
                    .ok_or_else(|| Unresolved(m[1].to_string()))?;
                out.push_str(&s[last..whole.start()]);
                out.push_str(&py_str(&dig(data, &m[2])?));
                last = whole.end();
            }
            out.push_str(&s[last..]);
            Ok(Value::String(out))
        }
        other => Ok(other.clone()),
    }
}

fn properties(tool: &Value) -> &Map<String, Value> {
    static EMPTY: LazyLock<Map<String, Value>> = LazyLock::new(Map::new);
    tool["inputs"]["properties"].as_object().unwrap_or(&EMPTY)
}

/// Whether the read test calls this tool: every GET, and the read-only POSTs that have a sample body.
pub fn is_read(tool: &Value) -> bool {
    tool["tool_call_template"]["http_method"] == "GET"
        || POST_BODIES.contains_key(tool["name"].as_str().unwrap_or(""))
}

pub struct ReadTest {
    utcp: Utcp,
    manuals: IndexMap<String, Value>,
    /// The manuals called with `--staged`.
    staged: BTreeSet<String>,
    /// normalized list-URL template -> first item of its response
    pub items: HashMap<String, Value>,
    /// tool name -> first successful response; "seed" -> the seeds
    responses: HashMap<String, Value>,
    results: Map<String, Value>,
    pub delay: Duration,
    pub log: Box<dyn FnMut(&str)>,
}

impl ReadTest {
    pub fn new(
        utcp: Utcp,
        manuals: IndexMap<String, Value>,
        staged: BTreeSet<String>,
        seeds: Map<String, Value>,
    ) -> Self {
        ReadTest {
            utcp,
            manuals,
            staged,
            items: HashMap::new(),
            responses: HashMap::from([("seed".to_string(), Value::Object(seeds))]),
            results: Map::new(),
            delay: Duration::from_millis(100),
            log: Box::new(|line| println!("{line}")),
        }
    }

    fn seeds(&self) -> &Map<String, Value> {
        self.responses["seed"]
            .as_object()
            .expect("seeds are an object")
    }

    pub fn set_seed(&mut self, name: &str, value: Value) {
        if let Some(Value::Object(seeds)) = self.responses.get_mut("seed") {
            seeds.insert(name.to_string(), value);
        }
    }

    /// A value for `param` from the seeds or SOURCES; `Ok(None)` when there is no rule. With `general = false` only
    /// seeds and rules for this tool apply.
    fn source(
        &self,
        tool_name: &str,
        param: &str,
        general: bool,
    ) -> Result<Option<Value>, Unresolved> {
        if let Some(v) = self.seeds().get(param) {
            return Ok(Some(v.clone()));
        }
        let own = format!("{tool_name}.{param}");
        let keys: Vec<&str> = if general {
            vec![&own, param]
        } else {
            vec![&own]
        };
        for key in keys {
            if let Some(v) = SOURCES.get(key) {
                return resolve(v, &self.responses).map(Some);
            }
        }
        Ok(None)
    }

    /// The arguments for a call, or why the tool cannot be called (yet).
    pub fn fill(&self, tool: &Value) -> Result<Map<String, Value>, String> {
        self.try_fill(tool)
            .unwrap_or_else(|e| Err(format!("waiting for {}", e.0)))
    }

    fn try_fill(&self, tool: &Value) -> Result<Result<Map<String, Value>, String>, Unresolved> {
        let mut args = Map::new();
        let name = tool["name"].as_str().unwrap_or("");
        let required: BTreeSet<String> = tool["inputs"]["required"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let template = template_of(tool);
        for m in PLACEHOLDER.captures_iter(&template) {
            let param = &m[1];
            let start = m.get(0).expect("group 0").start();
            // Seeds and this tool's own rule first, then the URL chain, then general rules.
            let mut value = self.source(name, param, false)?;
            if value.is_none() {
                let prefix = normalize(
                    template[..start]
                        .trim_end_matches('$')
                        .trim_end_matches('/'),
                );
                value = value_for(param, self.items.get(&prefix));
            }
            if value.is_none() {
                value = self.source(name, param, true)?;
            }
            let Some(value) = value else {
                // An optional path parameter is left out: utcp leaves its segment out (the default timeline, team...).
                if !required.contains(param) {
                    continue;
                }
                return Ok(Err(format!("no value for {{{param}}}")));
            };
            args.insert(param.to_string(), value);
        }
        for param in &required {
            if param == "body" || args.contains_key(param) {
                continue;
            }
            match self.source(name, param, true)? {
                Some(v) => {
                    args.insert(param.clone(), v);
                }
                None => return Ok(Err(format!("required parameter {param}"))),
            }
        }
        let body = POST_BODIES.get(name);
        if required.contains("body") && body.is_none() {
            return Ok(Err("no sample body".into()));
        }
        if let Some(body) = body {
            if let Value::Object(o) = resolve(body, &self.responses)? {
                for (k, v) in o {
                    args.insert(k, v);
                }
            }
        }
        // Tool-specific sources also fill optional parameters (some tools need one of several).
        let props = properties(tool);
        for (key, value) in SOURCES.iter() {
            let Some((tool_name, param)) = key.split_once('.') else {
                continue;
            };
            if !param.is_empty()
                && tool_name == name
                && !args.contains_key(param)
                && props.contains_key(param)
            {
                args.insert(param.to_string(), resolve(value, &self.responses)?);
            }
        }
        for (param, value) in LIMIT_PARAMS {
            if props.contains_key(param) && !args.contains_key(param) {
                args.insert(param.to_string(), json!(value));
            }
        }
        Ok(Ok(args))
    }

    fn call(
        &self,
        manual: &str,
        tool: &Value,
        args: &Map<String, Value>,
    ) -> (Map<String, Value>, Option<Value>) {
        let name = tool["name"].as_str().unwrap_or("");
        let staged = self.staged.contains(manual);
        if tool["tool_call_template"]["response_type"] != "file" {
            let called = self.utcp.call(manual, name, args, staged);
            return (called.result, called.data);
        }
        // A file answer goes to a temporary file (not the user's Downloads folder) and is deleted once it is
        // counted; it has no ids for other tools.
        let out = std::env::temp_dir().join(format!(
            "azdo-manuals-readtest-{}-{manual}.{name}",
            std::process::id()
        ));
        let called = self.utcp.call_to(manual, name, args, staged, Some(&out));
        let _ = std::fs::remove_file(&out);
        (called.result, None)
    }

    pub fn run(&mut self) -> Map<String, Value> {
        let mut pending: Vec<(String, Value)> = Vec::new();
        for (m, manual) in &self.manuals {
            for t in manual["tools"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
                let name = t["name"].as_str().unwrap_or("");
                if is_read(t) {
                    pending.push((m.clone(), t.clone()));
                } else {
                    self.results.insert(
                        format!("{m}.{name}"),
                        json!({"status": "not exercised", "reason": "not a read"}),
                    );
                }
            }
        }
        let mut progress = true;
        while !pending.is_empty() && progress {
            progress = false;
            let mut still = Vec::new();
            // Shorter URLs first, so list tools run before the tools that need their ids.
            pending.sort_by_cached_key(|(_, t)| {
                (
                    template_of(t).matches('/').count(),
                    t["name"].as_str().unwrap_or("").to_string(),
                )
            });
            for (manual, tool) in pending {
                let args = match self.fill(&tool) {
                    Ok(args) => args,
                    Err(_) => {
                        still.push((manual, tool));
                        continue;
                    }
                };
                let name = tool["name"].as_str().unwrap_or("").to_string();
                let (mut result, data) = self.call(&manual, &tool, &args);
                result.insert("args".into(), Value::Object(args));
                let status = result["status"].as_str().unwrap_or("").to_string();
                let bytes = result
                    .get("bytes")
                    .map(|b| b.to_string())
                    .unwrap_or_default();
                self.results
                    .insert(format!("{manual}.{name}"), Value::Object(result));
                (self.log)(&format!("  {status:8} {bytes:>9} {manual}.{name}"));
                if let Some(data) = data.filter(|d| !d.is_null()) {
                    if tool["tool_call_template"]["http_method"] == "GET" {
                        if let Some(item) = first_item(&data) {
                            self.items
                                .entry(normalize(&template_of(&tool)))
                                .or_insert_with(|| item.clone());
                        }
                    }
                    self.responses.entry(name).or_insert(data);
                }
                progress = true;
                std::thread::sleep(self.delay);
            }
            pending = still;
        }
        for (manual, tool) in pending {
            let reason = self.fill(&tool).err().unwrap_or_default();
            let name = tool["name"].as_str().unwrap_or("");
            self.results.insert(
                format!("{manual}.{name}"),
                json!({"status": "not exercised", "reason": reason}),
            );
        }
        std::mem::take(&mut self.results)
    }
}

pub fn summary(results: &Map<String, Value>) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for r in results.values() {
        *out.entry(r["status"].as_str().unwrap_or("").to_string())
            .or_default() += 1;
    }
    out
}
