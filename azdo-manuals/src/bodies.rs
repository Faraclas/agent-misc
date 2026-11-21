//! Hand-written request bodies for the writes in `Config::writes`.
//!
//! The specs describe each write body with the full response type: 35 properties for a pull request, 45 for a build,
//! most of them set by the service (dates, authors, links). An agent reading that schema cannot tell which fields it
//! may send, and the manual grows by about 100 KB. The work item body is a JSON Patch array that the spec describes as
//! an object. So each write gets a short schema here, with only the fields the service accepts. The idea, and the
//! field lists for pull requests and work items, follow Microsoft's `vsts-api-patcher` (MIT, see NOTICE); the text is
/// ours. The `write-test` command sends every field listed here and checks that the response shows it, so a field the
/// service ignores is caught there.
use std::collections::HashMap;

use serde_json::{json, Value};

fn identity_list(what: &str) -> Value {
    json!({
        "type": "array",
        "description": what,
        "items": {
            "type": "object",
            "required": ["id"],
            "properties": {"id": {"type": "string", "description": "Identity id (GUID) of a user or group."}},
        },
    })
}

fn completion_options() -> Value {
    json!({
        "type": "object",
        "description": "How the pull request is merged when it completes.",
        "properties": {
            "mergeStrategy": {
                "type": "string",
                "enum": ["noFastForward", "squash", "rebase", "rebaseMerge"],
                "description": "Default noFastForward (a merge commit).",
            },
            "deleteSourceBranch": {"type": "boolean"},
            "mergeCommitMessage": {"type": "string"},
            "transitionWorkItems": {
                "type": "boolean",
                "description": "Move linked work items to their next state (for example Active to Resolved).",
            },
        },
    })
}

fn pull_request_create() -> Value {
    json!({
        "type": "object",
        "description": "The pull request to open.",
        "required": ["sourceRefName", "targetRefName", "title"],
        "properties": {
            "sourceRefName": {
                "type": "string",
                "description": "Branch with the changes, as a full ref: refs/heads/<branch>.",
            },
            "targetRefName": {"type": "string", "description": "Branch to merge into, for example refs/heads/main."},
            "title": {"type": "string"},
            "description": {"type": "string", "description": "Markdown, up to 4000 characters."},
            "isDraft": {"type": "boolean", "description": "Open as a draft."},
            "reviewers": identity_list("Reviewers to add."),
            "workItemRefs": {
                "type": "array",
                "description": "Work items to link.",
                "items": {
                    "type": "object",
                    "required": ["id"],
                    "properties": {"id": {"type": "string", "description": "Work item id."}},
                },
            },
            "labels": {
                "type": "array",
                "description": "Labels (tags) to add.",
                "items": {"type": "object", "required": ["name"], "properties": {"name": {"type": "string"}}},
            },
        },
    })
}

fn pull_request_update() -> Value {
    json!({
        "type": "object",
        "description": "Only the fields to change.",
        "properties": {
            "title": {"type": "string"},
            "description": {"type": "string", "description": "Markdown, up to 4000 characters."},
            "isDraft": {"type": "boolean", "description": "false publishes a draft."},
            "status": {
                "type": "string",
                "enum": ["active", "abandoned", "completed"],
                "description": "abandoned closes the pull request, active reopens it. completed merges it now and \
                                needs lastMergeSourceCommit.",
            },
            "lastMergeSourceCommit": {
                "type": "object",
                "description": "With status completed: the source branch commit being merged, from \
                                get_pull_request (lastMergeSourceCommit.commitId).",
                "required": ["commitId"],
                "properties": {"commitId": {"type": "string"}},
            },
            "autoCompleteSetBy": {
                "type": "object",
                "description": "Turns on auto-complete: the pull request merges by itself once all policies pass. \
                                id: the caller's identity id (get_pull_request createdBy.id when the caller \
                                opened it).",
                "required": ["id"],
                "properties": {"id": {"type": "string"}},
            },
            "completionOptions": completion_options(),
        },
    })
}

fn comment(required_content: bool) -> Value {
    let mut c = json!({
        "type": "object",
        "properties": {
            "content": {"type": "string", "description": "Markdown, up to 150,000 characters."},
        },
    });
    if required_content {
        c["required"] = json!(["content"]);
    }
    c
}

fn file_position() -> Value {
    json!({
        "type": "object",
        "required": ["line", "offset"],
        "properties": {
            "line": {"type": "integer", "description": "1-based line."},
            "offset": {"type": "integer", "description": "1-based character in the line."},
        },
    })
}

fn thread_status() -> Value {
    json!({
        "type": "string",
        "enum": ["active", "pending", "fixed", "wontFix", "closed", "byDesign"],
        "description": "active and pending are open; fixed, wontFix, closed and byDesign resolve the thread.",
    })
}

fn thread_create() -> Value {
    let mut first = comment(true);
    first["properties"]["commentType"] = json!({"type": "string", "enum": ["text", "codeChange"]});
    json!({
        "type": "object",
        "description": "A new comment thread: one first comment, optionally anchored to lines of a file.",
        "required": ["comments"],
        "properties": {
            "comments": {"type": "array", "description": "The first comment (one item).", "items": first},
            "status": thread_status(),
            "threadContext": {
                "type": "object",
                "description": "Omit for a general comment. To comment on lines of a file: filePath plus \
                                rightFileStart and rightFileEnd (lines of the new version) or leftFileStart and \
                                leftFileEnd (lines of the old version).",
                "required": ["filePath"],
                "properties": {
                    "filePath": {"type": "string", "description": "Path from the repository root: /src/main.rs."},
                    "rightFileStart": file_position(),
                    "rightFileEnd": file_position(),
                    "leftFileStart": file_position(),
                    "leftFileEnd": file_position(),
                },
            },
        },
    })
}

fn thread_update() -> Value {
    json!({
        "type": "object",
        "description": "Resolve or reopen the thread. Add replies with create_comment.",
        "required": ["status"],
        "properties": {"status": thread_status()},
    })
}

fn comment_create() -> Value {
    let mut c = comment(true);
    c["description"] = json!("A reply in the thread.");
    c["properties"]["parentCommentId"] = json!({
        "type": "integer",
        "description": "Comment replied to; 1 is the thread's first comment.",
    });
    c["properties"]["commentType"] = json!({"type": "string", "enum": ["text", "codeChange"]});
    c
}

fn comment_update() -> Value {
    let mut c = comment(true);
    c["description"] = json!("The new text of the comment.");
    c
}

fn json_patch(create: bool) -> Value {
    let example = if create {
        r#"[{"op":"add","path":"/fields/System.Title","value":"Fix login"}]"#
    } else {
        r#"[{"op":"add","path":"/fields/System.State","value":"Active"}]"#
    };
    json!({
        "type": "array",
        "description": format!(
            "JSON Patch operations, for example {example}. Fields: path /fields/<reference name>; add sets a value, \
             replace changes it, remove clears it. Links: add to /relations/- a value {{\"rel\":\"<link type>\",\
             \"url\":\"<work item url>\"}}, for example rel System.LinkTypes.Hierarchy-Reverse for the parent or \
             System.LinkTypes.Related. Optimistic locking: start with {{\"op\":\"test\",\"path\":\"/rev\",\
             \"value\":<rev>}}."
        ),
        "items": {
            "type": "object",
            "required": ["op", "path"],
            "properties": {
                "op": {"type": "string", "enum": ["add", "replace", "remove", "test"]},
                "path": {"type": "string"},
                "value": {"description": "Field value, or the link object for /relations/-."},
            },
        },
    })
}

fn build_queue() -> Value {
    json!({
        "type": "object",
        "description": "The run to queue.",
        "required": ["definition"],
        "properties": {
            "definition": {
                "type": "object",
                "required": ["id"],
                "properties": {"id": {"type": "integer", "description": "Pipeline (build definition) id."}},
            },
            "sourceBranch": {
                "type": "string",
                "description": "Default: the pipeline's default branch. A full ref: refs/heads/<branch>, or \
                                refs/pull/<id>/merge for a pull request.",
            },
            "sourceVersion": {"type": "string", "description": "Commit to build. Default: the branch head."},
            "templateParameters": {
                "type": "object",
                "description": "Runtime parameters of a YAML pipeline, name to value.",
                "additionalProperties": {"type": "string"},
            },
        },
    })
}

fn build_update() -> Value {
    // The spec also lists keepForever; the service answers 400 ("deprecated in favor of retention leases").
    json!({
        "type": "object",
        "description": "{\"status\":\"cancelling\"} cancels a queued or running build. To rerun the failed jobs of a \
                        finished build, pass retry=true with an empty body {}.",
        "properties": {
            "status": {"type": "string", "enum": ["cancelling"]},
        },
    })
}

/// area -> operationId -> body schema.
pub fn real() -> HashMap<String, HashMap<String, Value>> {
    let area = |ops: Vec<(&str, Value)>| {
        ops.into_iter()
            .map(|(op, schema)| (op.to_string(), schema))
            .collect::<HashMap<_, _>>()
    };
    HashMap::from([
        (
            "git".to_string(),
            area(vec![
                ("Pull Requests_Create", pull_request_create()),
                ("Pull Requests_Update", pull_request_update()),
                ("Pull Request Threads_Create", thread_create()),
                ("Pull Request Threads_Update", thread_update()),
                ("Pull Request Thread Comments_Create", comment_create()),
                ("Pull Request Thread Comments_Update", comment_update()),
            ]),
        ),
        (
            "wit".to_string(),
            area(vec![
                ("Work Items_Create", json_patch(true)),
                ("Work Items_Update", json_patch(false)),
            ]),
        ),
        (
            "build".to_string(),
            area(vec![
                ("Builds_Queue", build_queue()),
                ("Builds_Update Build", build_update()),
            ]),
        ),
    ])
}
