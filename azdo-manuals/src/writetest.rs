//! Live write test: call every write tool of the generated manuals through a real utcp, in a throwaway project.
//!
//! For a sandbox organization only. A run deletes any project left over by an earlier run (name starting with
//! `azdo-manuals-test-`), creates `azdo-manuals-test-<UTC time>` with a repository, two branches and a YAML pipeline
//! (direct REST calls, the fixtures), runs the writes through utcp as an agent would, and deletes the project again.
//! The pipeline uses the self-hosted `Default` pool, which has no agents in a new organization, so a queued build
//! waits and can be cancelled without hosted build minutes.
//!
//! Each write is checked against the service's answer, not only its status: a field the service ignores shows up as
//! a mismatch. Printed output holds tool names, statuses and the names of mismatched fields; the details file holds
//! the arguments and the values found.

use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::http::{Request, Transport};
use crate::readtest::is_read;
use crate::routes::auth_header;
use crate::utcp::Utcp;

pub const PROJECT_PREFIX: &str = "azdo-manuals-test-";
const API: &str = "api-version=7.1";
const USER_AGENT: &str = concat!("azdo-manuals/", env!("CARGO_PKG_VERSION"), " write-test");
const NO_COMMIT: &str = "0000000000000000000000000000000000000000";
const PIPELINE_YAML: &str =
    "trigger: none\npr: none\nparameters:\n- name: note\n  type: string\n  default: none\n\
                             pool: Default\nsteps:\n- script: echo ${{ parameters.note }}\n";

/// What a field of the answer must be.
#[derive(Clone, Debug)]
pub enum Want {
    Is(Value),
    OneOf(Vec<Value>),
    Absent,
}

fn is(v: impl Into<Value>) -> Want {
    Want::Is(v.into())
}

/// The value at a dotted path (`comments.0.content`), if any. Object keys may contain dots themselves, as work item
/// field names do (`fields.System.Title`); the longest matching key wins.
pub fn lookup<'a>(data: &'a Value, path: &str) -> Option<&'a Value> {
    fn walk<'a>(node: &'a Value, parts: &[&str]) -> Option<&'a Value> {
        if parts.is_empty() {
            return Some(node);
        }
        match node {
            Value::Array(a) => walk(a.get(parts[0].parse::<usize>().ok()?)?, &parts[1..]),
            Value::Object(o) => (1..=parts.len()).rev().find_map(|n| {
                let child = o.get(&parts[..n].join("."))?;
                walk(child, &parts[n..])
            }),
            _ => None,
        }
    }
    walk(data, &path.split('.').collect::<Vec<_>>())
}

/// The mismatches between an answer and the expectations, as `(path, found)`.
pub fn check(data: &Value, expect: &[(&str, Want)]) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for (path, want) in expect {
        let found = lookup(data, path);
        let ok = match (want, found) {
            (Want::Absent, None | Some(Value::Null)) => true,
            (Want::Is(v), Some(f)) => v == f,
            (Want::OneOf(vs), f) => vs.contains(f.unwrap_or(&Value::Null)),
            _ => false,
        };
        if !ok {
            out.push((path.to_string(), found.cloned().unwrap_or(Value::Null)));
        }
    }
    out
}

/// Project names a run may delete: its own, and leftovers of earlier runs.
pub fn is_test_project(name: &str) -> bool {
    name.strip_prefix(PROJECT_PREFIX)
        .is_some_and(|rest| !rest.is_empty())
}

fn str_at(data: &Value, path: &str) -> Option<String> {
    match lookup(data, path)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn int_at(data: &Value, path: &str) -> Option<i64> {
    lookup(data, path).and_then(Value::as_i64)
}

/// Ids made by the fixtures.
struct Fixture {
    project: String,
    repo: String,
    me: String,
    main_commit: String,
    definition: i64,
}

pub struct WriteTest {
    utcp: Utcp,
    manuals: Vec<(String, Value)>,
    transport: Transport,
    base: String,
    auth: String,
    results: Map<String, Value>,
    project_id: Option<String>,
    pub log: Box<dyn FnMut(&str)>,
    pub poll_timeout: Duration,
}

impl WriteTest {
    pub fn new(
        utcp: std::path::PathBuf,
        manuals: Vec<(String, Value)>,
        transport: Transport,
        org: &str,
        pat: &str,
    ) -> std::io::Result<Self> {
        Ok(WriteTest {
            utcp: Utcp::new(utcp, "azdo-writetest-")?,
            manuals,
            transport,
            base: format!("https://dev.azure.com/{org}"),
            auth: auth_header(pat),
            results: Map::new(),
            project_id: None,
            log: Box::new(|line| println!("{line}")),
            poll_timeout: Duration::from_secs(180),
        })
    }

    pub fn register(&self, folder: &std::path::Path) -> Result<(), String> {
        self.utcp
            .register(folder, self.manuals.iter().map(|(n, _)| n))
    }

    /// A fixture call. `path` is relative to the organization; api-version 7.1 is added unless it names its own.
    fn rest(
        &mut self,
        method: &'static str,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, String> {
        let sep = if path.contains('?') { '&' } else { '?' };
        let version = if path.contains("api-version=") {
            String::new()
        } else {
            format!("{sep}{API}")
        };
        let mut headers = vec![
            ("Authorization".to_string(), self.auth.clone()),
            ("Accept".to_string(), "application/json".to_string()),
            ("User-Agent".to_string(), USER_AGENT.to_string()),
        ];
        if body.is_some() {
            headers.push(("Content-Type".into(), "application/json".into()));
        }
        let req = Request {
            method,
            url: format!("{}/{path}{version}", self.base),
            headers,
            body: body.map(|b| b.to_string().into_bytes()),
        };
        let what = format!("{method} {}", path.split('?').next().unwrap_or(""));
        let resp = (self.transport)(&req).map_err(|e| format!("{what}: {e}"))?;
        let data: Value = if resp.body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&resp.body).unwrap_or(Value::Null)
        };
        if !(200..300).contains(&resp.status) {
            let msg: String = str_at(&data, "message")
                .unwrap_or_default()
                .chars()
                .take(300)
                .collect();
            return Err(format!("{what}: HTTP {} {msg}", resp.status));
        }
        Ok(data)
    }

    /// Call `path` until `done` holds for its answer.
    fn poll(&mut self, path: &str, done: impl Fn(&Value) -> bool) -> Result<Value, String> {
        let start = Instant::now();
        loop {
            let data = self.rest("GET", path, None)?;
            if done(&data) {
                return Ok(data);
            }
            if start.elapsed() > self.poll_timeout {
                return Err(format!(
                    "GET {path}: still not ready after {}s",
                    self.poll_timeout.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    fn record(&mut self, label: String, result: Value) {
        let status = result["status"].as_str().unwrap_or("").to_string();
        let fields: Vec<&str> = result["mismatches"]
            .as_object()
            .map(|m| m.keys().map(String::as_str).collect())
            .unwrap_or_default();
        let detail = if fields.is_empty() {
            String::new()
        } else {
            format!(" ({})", fields.join(", "))
        };
        (self.log)(&format!("  {status:8} {label}{detail}"));
        let key = format!("{:02} {label}", self.results.len() + 1);
        self.results.insert(key, result);
    }

    /// One write through utcp. The answer when the call succeeded and every expectation held.
    fn write(
        &mut self,
        manual: &str,
        tool: &str,
        note: &str,
        args: Value,
        expect: &[(&str, Want)],
    ) -> Option<Value> {
        let args = match args {
            Value::Object(o) => o,
            _ => Map::new(),
        };
        let called = self.utcp.call(manual, tool, &args);
        let mut result = called.result.clone();
        result.insert("tool".into(), json!(format!("{manual}.{tool}")));
        result.insert("args".into(), Value::Object(args));
        let label = if note.is_empty() {
            format!("{manual}.{tool}")
        } else {
            format!("{manual}.{tool} [{note}]")
        };
        let ok = called.status() == "ok";
        let answer = called.data.filter(|_| ok);
        let mut passed = None;
        if let Some(data) = answer {
            let mismatches = check(&data, expect);
            if mismatches.is_empty() {
                passed = Some(data);
            } else {
                result.insert("status".into(), json!("mismatch"));
                let found: Map<String, Value> = mismatches.into_iter().collect();
                result.insert("mismatches".into(), Value::Object(found));
            }
        }
        self.record(label, Value::Object(result));
        passed
    }

    /// A write that the service must refuse with a 4xx status.
    fn refused(&mut self, manual: &str, tool: &str, note: &str, args: Value) {
        let args = match args {
            Value::Object(o) => o,
            _ => Map::new(),
        };
        let called = self.utcp.call(manual, tool, &args);
        let got = called.status().to_string();
        let mut result = called.result;
        result.insert("tool".into(), json!(format!("{manual}.{tool}")));
        result.insert("args".into(), Value::Object(args));
        result.insert("answer".into(), json!(got));
        let refused = got.starts_with("http 4");
        result.insert(
            "status".into(),
            json!(if refused { "ok" } else { "mismatch" }),
        );
        if !refused {
            result.insert("mismatches".into(), json!({"refused": got}));
        }
        self.record(
            format!("{manual}.{tool} [{note}, must be refused]"),
            Value::Object(result),
        );
    }

    /// A check made with a fixture read after a write (for effects the write's own answer does not show).
    fn verify(&mut self, label: &str, path: &str, expect: &[(&str, Want)]) {
        let result = match self.rest("GET", path, None) {
            Err(e) => json!({"status": "error", "error": e}),
            Ok(data) => {
                let mismatches = check(&data, expect);
                if mismatches.is_empty() {
                    json!({"status": "ok"})
                } else {
                    let found: Map<String, Value> = mismatches.into_iter().collect();
                    json!({"status": "mismatch", "mismatches": found})
                }
            }
        };
        self.record(format!("check: {label}"), result);
    }

    /// Record the outcome of a `poll` whose answer must have `field` equal to `want`.
    fn record_wait(&mut self, label: &str, polled: Result<Value, String>, field: &str, want: &str) {
        let result = match polled {
            Ok(d) if d[field] == want => json!({"status": "ok"}),
            Ok(d) => json!({"status": "mismatch", "mismatches": {field: d[field]}}),
            Err(e) => json!({"status": "error", "error": e}),
        };
        self.record(format!("check: {label}"), result);
    }

    fn delete_leftovers(&mut self) -> Result<(), String> {
        let list = self.rest("GET", "_apis/projects?stateFilter=all&$top=500", None)?;
        let leftovers: Vec<(String, String)> = list["value"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| Some((str_at(p, "name")?, str_at(p, "id")?)))
            .filter(|(name, _)| is_test_project(name))
            .collect();
        for (name, id) in leftovers {
            match self.rest("DELETE", &format!("_apis/projects/{id}"), None) {
                Ok(_) => (self.log)(&format!("Deleted leftover project {name}.")),
                Err(e) => (self.log)(&format!("Could not delete leftover project {name}: {e}")),
            }
        }
        Ok(())
    }

    fn push(
        &mut self,
        f: &Fixture,
        branch: &str,
        parent: Option<&str>,
        files: &[(&str, &str)],
    ) -> Result<String, String> {
        let changes: Vec<Value> = files
            .iter()
            .map(|(path, content)| {
                json!({"changeType": "add", "item": {"path": path},
                       "newContent": {"content": content, "contentType": "rawText"}})
            })
            .collect();
        let mut commit = json!({"comment": format!("Add {}", files[0].0), "changes": changes});
        if let Some(p) = parent {
            commit["parents"] = json!([p]);
        }
        let body = json!({
            "refUpdates": [{"name": format!("refs/heads/{branch}"), "oldObjectId": NO_COMMIT}],
            "commits": [commit],
        });
        let data = self.rest(
            "POST",
            &format!("{}/_apis/git/repositories/{}/pushes", f.project, f.repo),
            Some(body),
        )?;
        str_at(&data, "commits.0.commitId")
            .ok_or_else(|| format!("push to {branch}: no commit id in the answer"))
    }

    fn setup(&mut self, name: &str) -> Result<Fixture, String> {
        // Only offered as a preview.
        let me = self.rest("GET", "_apis/connectionData?api-version=7.1-preview", None)?;
        let me =
            str_at(&me, "authenticatedUser.id").ok_or("connectionData: no authenticatedUser.id")?;
        let processes = self.rest("GET", "_apis/process/processes", None)?;
        let process = processes["value"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|p| p["isDefault"] == true)
            .and_then(|p| str_at(p, "id"))
            .ok_or("no default process")?;
        let body = json!({
            "name": name,
            "description": "Created and deleted by azdo-manuals write-test.",
            "visibility": "private",
            "capabilities": {
                "versioncontrol": {"sourceControlType": "Git"},
                "processTemplate": {"templateTypeId": process},
            },
        });
        let queued = self.rest("POST", "_apis/projects", Some(body))?;
        let operation = str_at(&queued, "id").ok_or("project create: no operation id")?;
        self.poll(&format!("_apis/operations/{operation}"), |o| {
            matches!(
                o["status"].as_str(),
                Some("succeeded" | "failed" | "cancelled")
            )
        })
        .and_then(|o| match o["status"].as_str() {
            Some("succeeded") => Ok(()),
            other => Err(format!(
                "project create: operation {}",
                other.unwrap_or("?")
            )),
        })?;
        let project = self.rest("GET", &format!("_apis/projects/{name}"), None)?;
        let project = str_at(&project, "id").ok_or("project: no id")?;
        self.project_id = Some(project.clone());
        (self.log)(&format!("Created project {name}."));
        let repos = self.rest("GET", &format!("{project}/_apis/git/repositories"), None)?;
        let repo = str_at(&repos, "value.0.id").ok_or("no repository in the new project")?;
        let mut f = Fixture {
            project: project.clone(),
            repo,
            me,
            main_commit: String::new(),
            definition: 0,
        };
        f.main_commit = self.push(
            &f,
            "main",
            None,
            &[
                ("/README.md", "# azdo-manuals write test\n"),
                ("/azure-pipelines.yml", PIPELINE_YAML),
            ],
        )?;
        let main = f.main_commit.clone();
        self.push(
            &f,
            "feature-a",
            Some(&main),
            &[("/a.txt", "one\ntwo\nthree\n")],
        )?;
        self.push(&f, "feature-b", Some(&main), &[("/b.txt", "b\n")])?;
        let queues = self.rest(
            "GET",
            &format!("{project}/_apis/distributedtask/queues?queueName=Default"),
            None,
        )?;
        let queue =
            int_at(&queues, "value.0.id").ok_or("no Default agent queue in the new project")?;
        let body = json!({
            "name": "azdo-manuals-test",
            "path": "\\",
            "type": "build",
            "queue": {"id": queue},
            "repository": {"id": f.repo, "type": "TfsGit", "defaultBranch": "refs/heads/main"},
            "process": {"type": 2, "yamlFilename": "/azure-pipelines.yml"},
        });
        let definition = self.rest(
            "POST",
            &format!("{project}/_apis/build/definitions"),
            Some(body),
        )?;
        f.definition = int_at(&definition, "id").ok_or("pipeline create: no id")?;
        (self.log)(
            "Fixtures ready: repository with main, feature-a and feature-b; one YAML pipeline.",
        );
        Ok(f)
    }

    fn work_items(&mut self, f: &Fixture) -> Option<i64> {
        let m = "azdo_work";
        let other = self.write(
            m,
            "create_work_item",
            "title only",
            json!({"project": f.project, "type": "Task",
                   "body": [{"op": "add", "path": "/fields/System.Title", "value": "Related item"}]}),
            &[("fields.System.Title", is("Related item"))],
        )?;
        let item = self.write(
            m,
            "create_work_item",
            "title and description",
            json!({"project": f.project, "type": "Task", "body": [
                {"op": "add", "path": "/fields/System.Title", "value": "Write test item"},
                {"op": "add", "path": "/fields/System.Description", "value": "Made by write-test."},
            ]}),
            &[
                ("fields.System.Title", is("Write test item")),
                ("fields.System.Description", is("Made by write-test.")),
            ],
        )?;
        let (id, rev) = (int_at(&item, "id")?, int_at(&item, "rev")?);
        self.write(
            m,
            "update_work_item",
            "test rev, replace, remove, link",
            json!({"project": f.project, "id": id, "body": [
                {"op": "test", "path": "/rev", "value": rev},
                {"op": "replace", "path": "/fields/System.Title", "value": "Write test item, renamed"},
                {"op": "remove", "path": "/fields/System.Description"},
                {"op": "add", "path": "/relations/-",
                 "value": {"rel": "System.LinkTypes.Related", "url": other["url"]}},
            ]}),
            &[
                ("fields.System.Title", is("Write test item, renamed")),
                // A removed HTML field comes back empty rather than absent.
                (
                    "fields.System.Description",
                    Want::OneOf(vec![Value::Null, json!("")]),
                ),
                ("relations.0.rel", is("System.LinkTypes.Related")),
                ("relations.0.url", Want::Is(other["url"].clone())),
            ],
        );
        self.refused(
            m,
            "update_work_item",
            "stale rev",
            json!({"project": f.project, "id": id, "body": [
                {"op": "test", "path": "/rev", "value": rev},
                {"op": "replace", "path": "/fields/System.Title", "value": "Must not be written"},
            ]}),
        );
        Some(id)
    }

    fn pull_requests(&mut self, f: &Fixture, work_item: Option<i64>) -> Option<()> {
        let m = "azdo_git";
        let pr_args = |id: i64, body: Value| {
            let mut a = json!({"project": f.project, "repositoryId": f.repo, "pullRequestId": id});
            a["body"] = body;
            a
        };
        let mut body = json!({
            "sourceRefName": "refs/heads/feature-a",
            "targetRefName": "refs/heads/main",
            "title": "Write test PR",
            "description": "Opened by write-test.",
            "isDraft": true,
            "reviewers": [{"id": f.me}],
            "labels": [{"name": "azdo-manuals-test"}],
        });
        if let Some(w) = work_item {
            body["workItemRefs"] = json!([{"id": w.to_string()}]);
        }
        let pr = self.write(
            m,
            "create_pull_request",
            "draft, reviewer, label, work item",
            json!({"project": f.project, "repositoryId": f.repo, "body": body}),
            &[
                ("isDraft", is(true)),
                ("title", is("Write test PR")),
                ("description", is("Opened by write-test.")),
                ("reviewers.0.id", is(f.me.clone())),
                ("labels.0.name", is("azdo-manuals-test")),
            ],
        )?;
        let id = int_at(&pr, "pullRequestId")?;
        let pr_path = format!(
            "{}/_apis/git/repositories/{}/pullRequests/{id}",
            f.project, f.repo
        );
        if let Some(w) = work_item {
            self.verify(
                "pull request links the work item",
                &format!("{pr_path}/workitems"),
                &[("value.0.id", is(w.to_string()))],
            );
        }
        self.write(
            m,
            "update_pull_request",
            "title, description, publish draft",
            pr_args(id, json!({"title": "Write test PR, renamed", "description": "Edited.", "isDraft": false})),
            &[
                ("title", is("Write test PR, renamed")),
                ("description", is("Edited.")),
                ("isDraft", is(false)),
            ],
        );
        self.threads(f, id);
        for status in ["abandoned", "active"] {
            self.write(
                m,
                "update_pull_request",
                status,
                pr_args(id, json!({"status": status})),
                &[("status", is(status))],
            );
        }
        self.write(
            m,
            "update_pull_request",
            "auto-complete with completion options",
            pr_args(
                id,
                json!({
                    "autoCompleteSetBy": {"id": f.me},
                    "completionOptions": {
                        "mergeStrategy": "squash",
                        "deleteSourceBranch": true,
                        "mergeCommitMessage": "Squashed by write-test",
                        "transitionWorkItems": false,
                    },
                }),
            ),
            &[
                ("autoCompleteSetBy.id", is(f.me.clone())),
                ("completionOptions.mergeStrategy", is("squash")),
                ("completionOptions.deleteSourceBranch", is(true)),
                (
                    "completionOptions.mergeCommitMessage",
                    is("Squashed by write-test"),
                ),
            ],
        );
        let done = self.poll(&pr_path, |p| {
            p["status"] == "completed" || p["status"] == "abandoned"
        });
        self.record_wait(
            "auto-complete merges the pull request",
            done,
            "status",
            "completed",
        );
        let second = self.write(
            m,
            "create_pull_request",
            "minimal",
            json!({"project": f.project, "repositoryId": f.repo, "body": {
                "sourceRefName": "refs/heads/feature-b", "targetRefName": "refs/heads/main", "title": "Second PR",
            }}),
            &[("status", is("active")), ("isDraft", is(false))],
        )?;
        let id2 = int_at(&second, "pullRequestId")?;
        let path2 = format!(
            "{}/_apis/git/repositories/{}/pullRequests/{id2}",
            f.project, f.repo
        );
        let merged = self
            .poll(&path2, |p| {
                p["mergeStatus"] != "queued" && p["mergeStatus"] != "notSet"
            })
            .ok()?;
        let source = str_at(&merged, "lastMergeSourceCommit.commitId")?;
        // Completing is asynchronous: the answer may still say active.
        let completed = self.write(
            m,
            "update_pull_request",
            "complete now",
            pr_args(
                id2,
                json!({"status": "completed", "lastMergeSourceCommit": {"commitId": source},
                       "completionOptions": {"mergeStrategy": "noFastForward"}}),
            ),
            &[(
                "status",
                Want::OneOf(vec![json!("active"), json!("completed")]),
            )],
        );
        if completed.is_some() {
            let done = self.poll(&path2, |p| p["status"] != "active");
            self.record_wait(
                "complete now merges the pull request",
                done,
                "status",
                "completed",
            );
        }
        Some(())
    }

    fn threads(&mut self, f: &Fixture, pr: i64) -> Option<()> {
        let m = "azdo_git";
        let args = |extra: Value| {
            let mut a = json!({"project": f.project, "repositoryId": f.repo, "pullRequestId": pr});
            for (k, v) in extra.as_object().into_iter().flatten() {
                a[k] = v.clone();
            }
            a
        };
        let general = self.write(
            m,
            "create_thread",
            "general comment",
            args(
                json!({"body": {"comments": [{"content": "General remark.", "commentType": "text"}],
                                 "status": "active"}}),
            ),
            &[
                ("status", is("active")),
                ("comments.0.content", is("General remark.")),
            ],
        )?;
        let on_file = self.write(
            m,
            "create_thread",
            "on lines of a file",
            args(json!({"body": {
                "comments": [{"content": "About line 2.", "commentType": "text"}],
                "status": "pending",
                "threadContext": {"filePath": "/a.txt",
                                  "rightFileStart": {"line": 2, "offset": 1}, "rightFileEnd": {"line": 2, "offset": 4}},
            }})),
            &[
                ("status", is("pending")),
                ("threadContext.filePath", is("/a.txt")),
                ("threadContext.rightFileStart.line", is(2)),
                ("threadContext.rightFileEnd.offset", is(4)),
            ],
        );
        let thread = int_at(&general, "id")?;
        let reply = self.write(
            m,
            "create_comment",
            "reply",
            args(json!({"threadId": thread,
                        "body": {"content": "A reply.", "parentCommentId": 1, "commentType": "text"}})),
            &[("content", is("A reply.")), ("parentCommentId", is(1))],
        );
        if let Some(comment) = reply.as_ref().and_then(|r| int_at(r, "id")) {
            self.write(
                m,
                "update_comment",
                "",
                args(json!({"threadId": thread, "commentId": comment, "body": {"content": "A reply, edited."}})),
                &[("content", is("A reply, edited."))],
            );
        }
        for status in ["fixed", "active"] {
            self.write(
                m,
                "update_thread",
                status,
                args(json!({"threadId": thread, "body": {"status": status}})),
                &[("status", is(status))],
            );
        }
        if let Some(t) = on_file.as_ref().and_then(|t| int_at(t, "id")) {
            self.write(
                m,
                "update_thread",
                "closed",
                args(json!({"threadId": t, "body": {"status": "closed"}})),
                &[("status", is("closed"))],
            );
        }
        Some(())
    }

    fn builds(&mut self, f: &Fixture) -> Option<()> {
        let m = "azdo_pipelines";
        let build = self.write(
            m,
            "queue_build",
            "branch, commit, runtime parameter",
            json!({"project": f.project, "body": {
                "definition": {"id": f.definition},
                "sourceBranch": "refs/heads/main",
                "sourceVersion": f.main_commit,
                "templateParameters": {"note": "from-write-test"},
            }}),
            &[
                ("definition.id", is(f.definition)),
                ("sourceBranch", is("refs/heads/main")),
                ("sourceVersion", is(f.main_commit.clone())),
                ("templateParameters.note", is("from-write-test")),
            ],
        )?;
        let id = int_at(&build, "id")?;
        let path = format!("{}/_apis/build/builds/{id}", f.project);
        let args = |body: Value| json!({"project": f.project, "buildId": id, "body": body});
        let cancel_expect = [
            (
                "status",
                Want::OneOf(vec![json!("cancelling"), json!("completed")]),
            ),
            // A build that had failed by itself before the cancel would show result "failed".
            ("result", Want::OneOf(vec![Value::Null, json!("canceled")])),
        ];
        self.write(
            m,
            "update_build",
            "cancel",
            args(json!({"status": "cancelling"})),
            &cancel_expect,
        )?;
        let done = self.poll(&path, |b| b["status"] == "completed");
        self.record_wait("cancel stops the build", done, "result", "canceled");
        let mut retry = args(json!({}));
        retry["retry"] = json!(true);
        self.write(
            m,
            "update_build",
            "retry",
            retry,
            &[(
                "status",
                Want::OneOf(vec![json!("notStarted"), json!("inProgress")]),
            )],
        )?;
        self.write(
            m,
            "update_build",
            "cancel the retry",
            args(json!({"status": "cancelling"})),
            &cancel_expect,
        );
        Some(())
    }

    fn cleanup(&mut self) {
        if let Some(id) = self.project_id.take() {
            match self.rest("DELETE", &format!("_apis/projects/{id}"), None) {
                Ok(_) => (self.log)("Deleted the test project."),
                Err(e) => (self.log)(&format!(
                    "Could not delete the test project ({e}); the next run deletes it."
                )),
            }
        }
    }

    /// Runs the whole test and returns the results; `Err` when the fixtures could not be set up.
    pub fn run(&mut self, name: &str) -> Result<Map<String, Value>, String> {
        self.delete_leftovers()?;
        let setup = self.setup(name);
        let fixture = match setup {
            Ok(f) => f,
            Err(e) => {
                self.cleanup();
                return Err(e);
            }
        };
        self.builds(&fixture);
        let work_item = self.work_items(&fixture);
        self.pull_requests(&fixture, work_item);
        self.cleanup();
        let called: Vec<String> = self
            .results
            .values()
            .filter_map(|r| r["tool"].as_str().map(String::from))
            .collect();
        let mut missing = Vec::new();
        for (manual, data) in &self.manuals {
            for t in data["tools"].as_array().into_iter().flatten() {
                let full = format!("{manual}.{}", t["name"].as_str().unwrap_or(""));
                if !is_read(t) && !called.contains(&full) {
                    missing.push(full);
                }
            }
        }
        for full in missing {
            self.record(
                full,
                json!({"status": "not run", "reason": "an earlier step failed, or the test has no step for it"}),
            );
        }
        Ok(std::mem::take(&mut self.results))
    }
}
