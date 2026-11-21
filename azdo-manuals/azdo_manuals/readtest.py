"""Live read test: call every read tool of the generated manuals through a real utcp, chaining IDs.

Each call goes through `utcp call` with the manuals registered in a throwaway config, so it tests exactly what agents
use. Path parameters are filled from earlier responses: a tool `.../pullrequests/{pullRequestId}` takes the id of the
first item the list tool `.../pullrequests` returned. `project` (and `team`) come from the command line, `--seed`
sets any other parameter by name. Tools whose parameters cannot be filled are reported as not exercised.

Only GETs run, plus the read-only POSTs that have a sample body below. Printed output holds tool names, HTTP status
codes and sizes only; the details file (written next to the manuals, which are private anyway) also holds the values
used, since they come from the organization.
"""

import json
import os
import re
import shutil
import subprocess
import tempfile
import time
from pathlib import Path

# Sample bodies for the read-only POSTs, by tool name. Strings `@tool:path` are taken from an earlier response.
POST_BODIES = {
    "query_by_wiql": {
        "body": {"query": "Select [System.Id] From WorkItems Where [System.TeamProject] = @project "
                          "Order By [System.ChangedDate] Desc"},
        "$top": 5,
    },
    "fetch_code_search_results": {"body": {"searchText": "README", "$top": 5}},
    "fetch_wiki_search_results": {"body": {"searchText": "README", "$top": 5}},
    "fetch_work_item_search_results": {"body": {"searchText": "test", "$top": 5}},
    "query_subjects": {"body": {"query": "Project Collection Administrators", "subjectKind": ["Group"]}},
    "lookup_subjects": {"body": {"lookupKeys": [{"descriptor": "@list_users:value.0.descriptor"}]}},
    "get_work_items_batch": {"body": {"ids": ["@query_by_wiql:workItems.0.id"], "fields": ["System.Title"]}},
    "get_queries_batch": {"body": {"ids": ["@get_query:id"]}},
    "query_work_items_for_artifact_uris": {"body": {"artifactUris": ["@get_commit:_links.self.href"]}},
}

# Where values that cannot be chained by URL come from: `tool.param` (one tool) or `param` (any tool). A value is a
# literal, `@tool:dotted.path` (from that tool's first successful response), `@seed:name` (a --project/--team/--seed
# value), or a string with `{@tool:path}` parts.
SOURCES = {
    "team": "@get_project:defaultTeam.name",
    "get_project.projectId": "@seed:project",
    "get_check_configurations_on_resource.resourceType": "queue",
    "get_check_configurations_on_resource.resourceId": "@get_agent_queues:value.0.id",
    "workItemId": "@query_by_wiql:workItems.0.id",
    "get_work_item.id": "@query_by_wiql:workItems.0.id",
    "get_revisions.id": "@query_by_wiql:workItems.0.id",
    "get_revision.id": "@query_by_wiql:workItems.0.id",
    "get_revision.revisionNumber": "@get_revisions:value.0.rev",
    "get_updates.id": "@query_by_wiql:workItems.0.id",
    "get_update.id": "@query_by_wiql:workItems.0.id",
    "get_update.updateNumber": "@get_updates:value.0.id",
    "get_work_items.ids": "@query_by_wiql:workItems.0.id",
    "get_work_item_next_states_on_checkin_action.ids": "@query_by_wiql:workItems.0.id",
    "query_by_id.id": "@get_queries:value.0.children.0.id",
    "get_query.query": "@get_queries:value.0.id",
    "search_queries.$filter": "bug",
    "get_work_item_template.type": "@get_work_item_types:value.0.name",
    "get_classification_nodes.ids": "@get_root_nodes:value.0.id",
    "get_total_iteration_capacities.iterationId": "@get_team_iterations:value.0.id",
    "subjectDescriptor": "@list_users:value.0.descriptor",
    "storageKey": "@get_storage_key:value",
    "environmentId": "@get_release:environments.0.id",
    "get_release_revision.definitionSnapshotRevision": "@get_release:definitionSnapshotRevision",
    "timelineId": "@get_build:orchestrationPlan.planId",
    "definition": "@get_definitions:value.0.id",
    "get_definition_tags.definitionId": "@get_definitions:value.0.id",
    "fromBuildId": "@get_builds:value.1.id",
    "toBuildId": "@get_builds:value.0.id",
    "build_get_folders.path": "\\",
    "release_get_folders.path": "\\",
    "get_tree.sha1": "@get_commit:treeId",
    "get_branch.name": "@get_branches:value.0.name",
    "get_merge_bases.otherCommitId": "@get_commits:value.1.commitId",
    "get_item.path": "/",
    "get_push_commits.pushId": "@get_pushes:value.0.pushId",
    "get_repository_with_parent.includeParent": True,
    "get_policy_evaluations.artifactId":
        "vstfs:///CodeReview/CodeReviewId/{@get_repository:project.id}/{@get_pull_requests:value.0.pullRequestId}",
    "get_policy_evaluation.evaluationId": "@get_policy_evaluations:value.0.evaluationId",
    "get_relation_type.relation": "@get_relation_types:value.0.referenceName",
    "get_policy_configuration_revision.revisionId": "@get_policy_configuration_revisions:value.0.revision",
    "read_identities.subjectDescriptors": "@list_users:value.0.descriptor",
    "buildId": "@get_builds:value.0.id",
    "build": "@get_builds:value.0.id",
    "pipelineId": "@list_pipelines:value.0.id",
    "poolIds": "@get_agent_pools:value.0.id",
    "queueIds": "@get_agent_queues:value.0.id",
    "queueNames": "@get_agent_queues:value.0.name",
    "releaseId": "@get_releases:value.0.id",
    "releaseEnvId": "@get_release:environments.0.id",
    "planId": "@get_test_plans:value.0.id",
    "get_work_item_columns.iterationId": "@get_team_iterations:value.0.id",
    "get_iteration_chart_image.iterationId": "@get_team_iterations:value.0.id",
}
LIMIT_PARAMS = {"$top": 5, "top": 5, "searchCriteria.$top": 5}
ID_KEYS = ("id", "name", "descriptor", "key", "objectId", "originId")
PLACEHOLDER = re.compile(r"\{([^}]+)\}")


def template_of(tool: dict) -> str:
    url = tool["tool_call_template"]["url"].split("?", 1)[0]
    return url.replace("%24{", "${")


def normalize(template: str) -> str:
    return PLACEHOLDER.sub("{}", template).lower()


def first_item(response):
    if isinstance(response, dict):
        for key in ("value", "items", "results", "comments"):
            if isinstance(response.get(key), list):
                return response[key][0] if response[key] else None
        return response
    if isinstance(response, list):
        return response[0] if response else None
    return None


def value_for(param: str, item):
    if not isinstance(item, dict):
        return None
    lowered = {k.lower(): k for k in item}
    for key in (param,) + ID_KEYS:
        if key.lower() in lowered:
            v = item[lowered[key.lower()]]
            if isinstance(v, (str, int)) and v != "":
                return v
    return None


class Unresolved(Exception):
    pass


def dig(data, path: str):
    for part in path.split(".") if path else []:
        if isinstance(data, list) and part.isdigit() and int(part) < len(data):
            data = data[int(part)]
        elif isinstance(data, dict) and part in data:
            data = data[part]
        else:
            raise Unresolved(path)
    if data is None or data == "":
        raise Unresolved(path)
    return data


REF = re.compile(r"\{@([a-z0-9_]+):([^}]*)\}")


def resolve(value, responses: dict):
    """Replace `@tool:path` strings (and `{@tool:path}` parts) with values from earlier responses."""
    if isinstance(value, dict):
        return {k: resolve(v, responses) for k, v in value.items()}
    if isinstance(value, list):
        return [resolve(v, responses) for v in value]
    if not isinstance(value, str):
        return value
    m = re.fullmatch(r"@([a-z0-9_]+):(.*)", value)
    if m:
        if m.group(1) not in responses:
            raise Unresolved(m.group(1))
        return dig(responses[m.group(1)], m.group(2))

    def part(match):
        if match.group(1) not in responses:
            raise Unresolved(match.group(1))
        return str(dig(responses[match.group(1)], match.group(2)))

    return REF.sub(part, value)


class ReadTest:
    def __init__(self, utcp: str, manuals: dict, seeds: dict, log=print, delay: float = 0.1):
        self.utcp, self.manuals, self.seeds, self.log, self.delay = utcp, manuals, seeds, log, delay
        self.items = {}  # normalized list-URL template -> first item of its response
        self.responses = {"seed": seeds}  # tool name -> first successful response; "seed" -> the seeds
        self.results = {}
        self.tmp = Path(tempfile.mkdtemp(prefix="azdo-readtest-"))
        self.env = dict(os.environ)
        for name in ("CONFIG", "CACHE", "STATE"):
            self.env[f"UTCP_{name}_DIR"] = str(self.tmp / name.lower())

    def close(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def run_utcp(self, *args, stdin=None):
        return subprocess.run([self.utcp, *args, "--direct"], env=self.env, capture_output=True, text=True,
                              input=stdin, timeout=120)

    def register(self, folder: Path):
        for name in self.manuals:
            r = self.run_utcp("register", name, str(folder / f"{name}.utcp.json"))
            if r.returncode != 0:
                raise RuntimeError(f"utcp register {name} failed: {r.stderr.strip()[:300]}")

    def source(self, tool_name: str, param: str, general: bool = True):
        """A value for `param` from the seeds or SOURCES; None when there is no rule; Unresolved when not yet known.
        With `general=False` only seeds and rules for this tool apply."""
        if param in self.seeds:
            return self.seeds[param]
        for key in (f"{tool_name}.{param}", param) if general else (f"{tool_name}.{param}",):
            if key in SOURCES:
                return resolve(SOURCES[key], self.responses)
        return None

    def fill(self, tool: dict):
        """Return (args, None) or (None, reason)."""
        args = {}
        name = tool["name"]
        required = set(tool["inputs"].get("required", []))
        template = template_of(tool)
        try:
            for match in PLACEHOLDER.finditer(template):
                param = match.group(1)
                # Seeds and this tool's own rule first, then the URL chain, then general rules.
                value = self.source(name, param, general=False)
                if value is None:
                    prefix = normalize(template[: match.start()].rstrip("$").rstrip("/"))
                    value = value_for(param, self.items.get(prefix))
                if value is None:
                    value = self.source(name, param)
                if value is None:
                    return None, f"no value for {{{param}}}"
                args[param] = value
            for param in sorted(required - set(args) - {"body"}):
                value = self.source(name, param)
                if value is None:
                    return None, f"required parameter {param}"
                args[param] = value
            if "body" in required and name not in POST_BODIES:
                return None, "no sample body"
            if name in POST_BODIES:
                args.update(resolve(POST_BODIES[name], self.responses))
            # Tool-specific sources also fill optional parameters (some tools need one of several).
            for key in SOURCES:
                tool_name, _, param = key.partition(".")
                if param and tool_name == name and param not in args and param in tool["inputs"]["properties"]:
                    args[param] = resolve(SOURCES[key], self.responses)
        except Unresolved as e:
            return None, f"waiting for {e}"
        for param, value in LIMIT_PARAMS.items():
            if param in tool["inputs"]["properties"]:
                args.setdefault(param, value)
        return args, None

    def call(self, manual: str, tool: dict, args: dict):
        start = time.monotonic()
        r = self.run_utcp("call", f"{manual}.{tool['name']}", "--args-stdin", stdin=json.dumps(args))
        elapsed = round(time.monotonic() - start, 2)
        if r.returncode == 0:
            text = r.stdout.strip()
            try:
                data = json.loads(text)
            except ValueError:
                data = text
            return {"status": "ok", "bytes": len(text.encode()), "seconds": elapsed}, data
        m = re.search(r"status:? (\d{3})", r.stderr)
        return {"status": f"http {m.group(1)}" if m else "error", "seconds": elapsed,
                "error": r.stderr.strip()[:500]}, None

    def run(self):
        pending = [(m, t) for m, man in self.manuals.items() for t in man["tools"]
                   if t["tool_call_template"]["http_method"] == "GET" or t["name"] in POST_BODIES]
        skipped = [(m, t) for m, man in self.manuals.items() for t in man["tools"] if (m, t) not in pending]
        for m, t in skipped:
            self.results[f"{m}.{t['name']}"] = {"status": "not exercised", "reason": "not a read"}
        progress = True
        while pending and progress:
            progress, still = False, []
            # Shorter URLs first, so list tools run before the tools that need their ids.
            for manual, tool in sorted(pending, key=lambda mt: (template_of(mt[1]).count("/"), mt[1]["name"])):
                args, reason = self.fill(tool)
                if args is None:
                    still.append((manual, tool))
                    continue
                result, data = self.call(manual, tool, args)
                result["args"] = args
                self.results[f"{manual}.{tool['name']}"] = result
                self.log(f"  {result['status']:8} {result.get('bytes', ''):>9} {manual}.{tool['name']}")
                if data is not None:
                    self.responses.setdefault(tool["name"], data)
                if data is not None and tool["tool_call_template"]["http_method"] == "GET":
                    item = first_item(data)
                    if item is not None:
                        self.items.setdefault(normalize(template_of(tool)), item)
                progress = True
                time.sleep(self.delay)
            pending = still
        for manual, tool in pending:
            _, reason = self.fill(tool)
            self.results[f"{manual}.{tool['name']}"] = {"status": "not exercised", "reason": reason}
        return self.results


def summary(results: dict) -> dict:
    out = {}
    for r in results.values():
        key = r["status"]
        out[key] = out.get(key, 0) + 1
    return out
