"""Offline tests: hand-made specs in, exact manuals out. No network.

Run from the azdo-manuals folder: `python3 -m unittest discover -s tests -v`.
Set UPDATE_EXPECTED=1 to rewrite tests/fixtures/expected/ after an intended change (then review the diff).
The utcp load test runs the `utcp` from UTCP_BIN, else from PATH, and is skipped when there is none (CI sets
REQUIRE_UTCP=1 on Linux to make it fail instead).
"""

import io
import json
import os
import shutil
import subprocess
import tempfile
import types
import unittest
import urllib.error
from pathlib import Path

from azdo_manuals import archive, cli, readtest, routes
from azdo_manuals.generate import (
    GenerateError,
    api_version_of,
    assign_names,
    base_name,
    generate,
    minimal_scopes,
)

HERE = Path(__file__).resolve().parent
FIXTURES = HERE / "fixtures"
EXPECTED = FIXTURES / "expected"
ORG = "contoso"
SOURCE = {"revision": "0" * 40, "revision_date": "2026-01-01T00:00:00+00:00", "revision_message": "test"}

TEST_CONFIG = types.SimpleNamespace(
    MANUALS={
        "t_main": {"areas": ["alpha", "beta", "gamma"], "title": "Test main", "hint": "test main"},
        "t_empty": {"areas": ["gamma"], "title": "Test empty", "hint": "test empty"},
    },
    POST_READS={"alpha": ["Query_Run Query"], "beta": ["Subject Query_Query"]},
    WRITES={"alpha": ["Widgets_Create"]},
    ALLOW_WRITE_SCOPES=set(),
    SKIP_GETS={"beta": {"Profiles_Get": "test skip"}},
    REQUIRED_PARAMS={"alpha": {"Widgets_List": ["kind"]}},
    NAMES={"beta": {"Subject Query_Query": "query_subjects"}},
    DROP_OPTIONAL_PATH_PARAMS={"alpha": {"team"}},
)


def load_fixture_specs():
    return {name: [json.loads((FIXTURES / f"{name}.json").read_text(encoding="utf-8"))] for name in ("alpha", "beta")}


def run_generate(conf=TEST_CONFIG):
    return generate(load_fixture_specs(), ORG, "7.1", SOURCE, conf)


class GoldenTest(unittest.TestCase):
    def test_manuals_match_expected(self):
        manuals, report = run_generate()
        outputs = {f"{name}.utcp.json": manual for name, manual in manuals.items()}
        outputs["report.json"] = report
        if os.environ.get("UPDATE_EXPECTED"):
            EXPECTED.mkdir(parents=True, exist_ok=True)
            for old in EXPECTED.glob("*.json"):
                old.unlink()
            for name, data in outputs.items():
                cli.write_json(EXPECTED / name, data)
        self.assertEqual(sorted(p.name for p in EXPECTED.glob("*.json")), sorted(outputs))
        for name, data in outputs.items():
            with self.subTest(file=name):
                expected = json.loads((EXPECTED / name).read_text(encoding="utf-8"))
                self.assertEqual(json.loads(json.dumps(data)), expected)

    def test_selection_and_urls(self):
        manuals, report = run_generate()
        tools = {t["name"]: t for t in manuals["t_main"]["tools"]}
        self.assertNotIn("t_empty", manuals, "a manual whose areas have no spec is not written")
        self.assertEqual(report["manuals"]["t_main"]["areas_without_spec"], ["gamma"])
        reasons = {(x["operation_id"], x["reason"]) for x in report["left_out"]}
        self.assertIn(("Widgets_Delete", "DELETE never included"), reasons)
        self.assertIn(("Widgets_Exists", "HEAD never included"), reasons)
        self.assertIn(("Archive_Get Zip", "binary response"), reasons)
        self.assertIn(("Profiles_Get", "skipped: test skip"), reasons)
        self.assertNotIn("get_profile", tools)
        self.assertEqual(tools["alpha_list_widgets"]["inputs"]["required"], ["kind", "project"])
        # Same operationId in two areas of one manual: both get the area prefix.
        self.assertIn("alpha_list_widgets", tools)
        self.assertIn("beta_list_widgets", tools)
        url = lambda n: tools[n]["tool_call_template"]["url"]  # noqa: E731
        self.assertEqual(url("get_widget_by_id"),
                         "https://dev.azure.com/contoso/{project}/_apis/alpha/widgets/{widgetId}"
                         "?api-version=7.1-preview.2")
        self.assertEqual(url("run_query"), "https://dev.azure.com/contoso/{project}/_apis/alpha/query?api-version=7.1")
        self.assertNotIn("team", tools["run_query"]["inputs"]["properties"])
        self.assertEqual(url("get_thing_template"),
                         "https://dev.azure.com/contoso/{project}/_apis/alpha/things/%24{type}?api-version=7.1")
        self.assertEqual(url("get_widgets_batch"), "https://dev.azure.com/contoso/_apis/alpha/widgets?api-version=7.1")
        self.assertEqual(tools["get_log"]["outputs"], {"type": "string"})
        self.assertEqual(tools["get_log"]["tool_call_template"]["headers"]["Accept"], "text/plain")
        self.assertEqual(tools["get_widget_by_id"]["tool_call_template"]["header_fields"], ["X-Trace"])
        auth = tools["get_widget_by_id"]["tool_call_template"]["auth"]
        self.assertEqual(auth, {"auth_type": "basic", "username": "pat", "password": "${AZURE_DEVOPS_EXT_PAT}"})

    def test_body_schema(self):
        manuals, _ = run_generate()
        tools = {t["name"]: t for t in manuals["t_main"]["tools"]}
        create = tools["create_widget"]
        self.assertEqual(create["tool_call_template"]["body_field"], "body")
        self.assertEqual(create["tool_call_template"]["headers"]["Content-Type"], "application/json")
        self.assertEqual(sorted(create["inputs"]["properties"]["body"]["properties"]), ["name"])
        body = tools["run_query"]["inputs"]["properties"]["body"]
        self.assertEqual(body["description"], "The query.")
        self.assertEqual(sorted(body["properties"]), ["$top", "options", "parent", "text"])
        self.assertEqual(body["required"], ["text"])
        self.assertEqual(body["properties"]["parent"], {"type": "object", "description": "QueryRequest"})
        level2 = body["properties"]["options"]["properties"]["level1"]["properties"]["level2"]
        self.assertEqual(level2, {"type": "object", "description": "Too deep to inline."})

    def test_scope_report(self):
        _, report = run_generate()
        self.assertEqual(report["scopes_all"], ["vso.alpha", "vso.alpha_write", "vso.beta", "vso.beta_full"])
        # Threads_List accepts vso.beta or vso.beta_full: the read scope covers it, no write scope is asked for.
        self.assertEqual([s["scope"] for s in report["scopes"]], ["vso.alpha_write", "vso.beta"])
        self.assertEqual(report["scopes"][0]["description"], "Read and write alpha things.")
        md = cli.scopes_markdown(report, ORG)
        self.assertIn("| `vso.alpha_write` | Read and write alpha things. |", md)

    def test_read_needing_write_scope(self):
        manuals, report = run_generate()
        names = {t["name"] for t in manuals["t_main"]["tools"]}
        self.assertNotIn("list_pools", names)
        self.assertIn(("Pools_List", "read needs a write scope: vso.beta_manage"),
                      {(x["operation_id"], x["reason"]) for x in report["left_out"]})
        conf = types.SimpleNamespace(**{**vars(TEST_CONFIG), "ALLOW_WRITE_SCOPES": {"vso.beta_manage"}})
        manuals, report = run_generate(conf)
        self.assertIn("list_pools", {t["name"] for t in manuals["t_main"]["tools"]})
        self.assertIn("vso.beta_manage", [s["scope"] for s in report["scopes"]])

    def test_covering_scopes(self):
        from azdo_manuals.generate import covering_scopes
        self.assertEqual(covering_scopes([["vso.code", "vso.threads_full"], ["vso.code"], []]), ["vso.code"])
        self.assertEqual(covering_scopes([["vso.profile", "vso.project"]]), ["vso.profile"])
        self.assertEqual(covering_scopes([["vso.project"], ["vso.profile", "vso.project"]]), ["vso.project"])

    def test_output_has_no_other_org(self):
        manuals, report = run_generate()
        text = json.dumps([manuals, report])
        self.assertNotIn("{organization}", text)


class UnitTest(unittest.TestCase):
    def test_base_name(self):
        self.assertEqual(base_name("Teams_List"), "list_teams")
        self.assertEqual(base_name("Teams_Get"), "get_team")
        self.assertEqual(base_name("Widgets_Create"), "create_widget")
        self.assertEqual(base_name("Classification Nodes_Get"), "get_classification_node")
        self.assertEqual(base_name("Policies_Get"), "get_policy")
        self.assertEqual(base_name("Work Items_Get Work Item"), "get_work_item")
        self.assertEqual(base_name("Wiql_Query By Wiql"), "query_by_wiql")
        self.assertEqual(base_name("Pull Requests_Get Pull Request By Id"), "get_pull_request_by_id")

    def test_api_version(self):
        spec = {}
        op = {"operationId": "x", "parameters": [{"$ref": "#/parameters/api-Version-preview.3"}]}
        self.assertEqual(api_version_of(spec, op, "7.1"), "7.1-preview.3")
        with self.assertRaises(GenerateError):
            api_version_of(spec, {"operationId": "x", "parameters": []}, "7.1")
        with self.assertRaises(GenerateError):
            api_version_of(spec, {"operationId": "x", "parameters": [{"$ref": "#/parameters/api-Version-beta"}]}, "7.1")

    def test_org_name_checked(self):
        for bad in ("", "a/b", "my org", "x" * 60, "-lead"):
            with self.subTest(org=bad), self.assertRaises(GenerateError):
                generate(load_fixture_specs(), bad, "7.1", SOURCE, TEST_CONFIG)

    def test_required_override_must_exist(self):
        conf = types.SimpleNamespace(**{**vars(TEST_CONFIG), "REQUIRED_PARAMS": {"alpha": {"Widgets_List": ["nope"]}}})
        with self.assertRaisesRegex(GenerateError, "does not have"):
            run_generate(conf)

    def test_name_clash_needs_override(self):
        entries = [
            {"area": "a", "operation_id": "X_Get Thing", "tool": {}},
            {"area": "a", "operation_id": "Y_Get Thing", "tool": {}},
        ]
        conf = types.SimpleNamespace(NAMES={})
        with self.assertRaisesRegex(GenerateError, "add one of them"):
            assign_names("m", entries, conf)

    def test_minimal_scopes(self):
        self.assertEqual(minimal_scopes({"vso.code", "vso.code_write", "vso.work", "vso.build_execute"}),
                         ["vso.build_execute", "vso.code_write", "vso.work"])


class FakeResponse(io.BytesIO):
    def __init__(self, data, headers=None):
        super().__init__(data)
        self.headers = headers or {}

    def __enter__(self):
        return self

    def __exit__(self, *a):
        self.close()


class ArchiveTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())

    def tearDown(self):
        shutil.rmtree(self.tmp)

    def test_blob_hash(self):
        self.assertEqual(archive.git_blob_sha1(b"hello\n"), "ce013625030ba8dba906f756967f9e9ca394464a")

    def test_fetch_verifies_and_caches(self):
        data = b'\xef\xbb\xbf{"swagger": "2.0"}'
        pin = {"file": "x.json", "sha1_git": archive.git_blob_sha1(data)}
        calls = []

        def opener(req, timeout):
            calls.append(req)
            self.assertIn("azdo-manuals/", req.get_header("User-agent"))
            return FakeResponse(data, {"X-RateLimit-Remaining": "7"})

        a = archive.Archive(opener=opener)
        self.assertEqual(archive.fetch_file(a, pin, self.tmp), data)
        self.assertEqual(archive.fetch_file(a, pin, self.tmp), data)
        self.assertEqual(len(calls), 1, "second read comes from the cache")
        self.assertEqual(a.remaining, "7")
        self.assertEqual(archive.load_spec(data), {"swagger": "2.0"})

    def test_fetch_refuses_wrong_content(self):
        pin = {"file": "x.json", "sha1_git": archive.git_blob_sha1(b"expected")}
        a = archive.Archive(opener=lambda req, timeout: FakeResponse(b"tampered"))
        with self.assertRaisesRegex(archive.ArchiveError, "pinned"):
            archive.fetch_file(a, pin, self.tmp)
        self.assertEqual(list(self.tmp.iterdir()), [])

    def test_corrupt_cache_is_replaced(self):
        data = b"{}"
        pin = {"file": "x.json", "sha1_git": archive.git_blob_sha1(data)}
        (self.tmp / f"{pin['sha1_git']}.json").write_bytes(b"corrupt")
        a = archive.Archive(opener=lambda req, timeout: FakeResponse(data))
        self.assertEqual(archive.fetch_file(a, pin, self.tmp), data)

    def test_rate_limit_message(self):
        def opener(req, timeout):
            raise urllib.error.HTTPError(req.full_url, 429, "Too Many", {"X-RateLimit-Reset": "x"}, None)

        with self.assertRaisesRegex(archive.ArchiveError, "rate limit"):
            archive.Archive(opener=opener).get_content("0" * 40)

    def test_discover_pins_resumes_and_records_missing(self):
        listings = {
            "/revision/r1/": {"directory": "root", "date": "d", "message": "m\nmore"},
            "/directory/root/specification/": {"type": "dir", "target": "spec"},
            "/directory/spec/": [{"name": "a1", "type": "dir", "target": "A1"},
                                 {"name": "a2", "type": "dir", "target": "A2"}],
            "/directory/A1/": [{"name": "7.1", "type": "dir", "target": "A1v"}],
            "/directory/A1v/": [{"name": "a1.json", "type": "file", "target": "h1", "length": 3},
                                {"name": "httpExamples", "type": "dir", "target": "ex"}],
            "/directory/A2/": [{"name": "7.2", "type": "dir", "target": "A2v"}],
        }
        asked = []

        def opener(req, timeout):
            path = req.full_url.split("/api/1", 1)[1]
            asked.append(path)
            return FakeResponse(json.dumps(listings[path]).encode())

        saved = []
        result = archive.discover_pins(archive.Archive(opener=opener), "r1", "7.1", ["a1", "a2"],
                                       log=lambda m: None, save=lambda r: saved.append(json.dumps(r)))
        self.assertEqual(result["areas"], {"a1": [{"file": "a1.json", "sha1_git": "h1", "length": 3}]})
        self.assertEqual(result["missing"], {"a2": ["7.2"]})
        self.assertEqual(result["revision_message"], "m")
        self.assertEqual(len(saved), 2)
        asked.clear()
        archive.discover_pins(archive.Archive(opener=opener), "r1", "7.1", ["a1", "a2"], existing=result,
                              log=lambda m: None)
        self.assertEqual(asked, [], "a complete earlier result needs no requests")


class RouteCheckTest(unittest.TestCase):
    ROUTES = {
        "https://dev.azure.com/contoso": [
            {"area": "alpha", "resourceName": "widgets",
             "routeTemplate": "{project}/_apis/{area}/{resource}/{widgetId}",
             "minVersion": "1.0", "maxVersion": "7.2", "releasedVersion": "7.1", "resourceVersion": 2},
            {"area": "alpha", "resourceName": "query", "routeTemplate": "{project}/{team}/_apis/{area}/{resource}",
             "minVersion": "1.0", "maxVersion": "7.2", "releasedVersion": "7.1", "resourceVersion": 1},
            {"area": "alpha", "resourceName": "things", "routeTemplate": "{project}/_apis/{area}/{resource}/${type}",
             "minVersion": "1.0", "maxVersion": "7.2", "releasedVersion": "7.1", "resourceVersion": 1},
            {"area": "alpha", "resourceName": "logs", "routeTemplate": "_apis/{area}/{resource}/{logId}",
             "minVersion": "1.0", "maxVersion": "7.2", "releasedVersion": "7.0", "resourceVersion": 1},
        ],
        "https://vsbeta.dev.azure.com/contoso": [
            {"area": "beta", "resourceName": "widgets", "routeTemplate": "_apis/{area}/{resource}",
             "minVersion": "1.0", "maxVersion": "7.2", "releasedVersion": "7.1", "resourceVersion": 1},
        ],
        "https://vsbeta.dev.azure.com": [],
    }

    def test_check(self):
        manuals, _ = run_generate()
        result = routes.check(manuals, ORG, "x", fetch=lambda base, pat: self.ROUTES[base])
        self.assertEqual(result["bases"], {"https://dev.azure.com/<org>": 4, "https://vsbeta.dev.azure.com/<org>": 1})
        # Widgets list/create/get/batch (project is optional), query, things, beta widgets match; logs is asked at
        # 7.1 but released only to 7.0.
        self.assertEqual(result["matched"], 7)
        self.assertEqual(sorted(result["unmatched"]), ["t_main.list_threads", "t_main.query_subjects"])

    def test_unreadable_route_list(self):
        manuals, _ = run_generate()

        def fetch(base, pat):
            if "vsbeta" in base:
                raise RuntimeError(f"OPTIONS {base}/_apis failed: HTTP 401")
            return self.ROUTES[base]

        result = routes.check(manuals, ORG, "x", fetch=fetch)
        self.assertEqual(sorted(result["unchecked"]),
                         ["t_main.beta_list_widgets", "t_main.list_threads", "t_main.query_subjects"])
        self.assertEqual(result["base_errors"],
                         {"https://vsbeta.dev.azure.com/<org>": "OPTIONS https://vsbeta.dev.azure.com/<org>/_apis "
                                                                "failed: HTTP 401"})
        self.assertEqual(len(result["version"]), 1)
        self.assertIn("t_main.get_log (7.1): not released at 7.1", result["version"][0])

    def test_versions(self):
        route = {"minVersion": "4.0", "maxVersion": "7.2", "releasedVersion": "7.1", "resourceVersion": 2}
        self.assertEqual(routes.version_problem("7.1", route), "")
        self.assertEqual(routes.version_problem("7.1-preview.2", route), "")
        self.assertIn("resourceVersion", routes.version_problem("7.1-preview.3", route))
        self.assertIn("needs -preview", routes.version_problem("7.2", route))
        self.assertIn("minVersion", routes.version_problem("3.0", route))

    def test_literal_does_not_match_parameter(self):
        route = routes.route_segments({"area": "a", "resourceName": "r", "routeTemplate": "_apis/{area}/{resource}"})
        self.assertTrue(routes.segments_match(route, ["_apis", "a", "r"]))
        self.assertFalse(routes.segments_match(route, ["_apis", "a", "*"]))
        self.assertFalse(routes.segments_match(route, ["_apis", "a", "r", "extra"]))


class ReadTestHelpersTest(unittest.TestCase):
    def test_resolve(self):
        responses = {"list_things": {"count": 2, "value": [{"id": 7, "name": "a"}, {"id": 8}]}}
        self.assertEqual(readtest.resolve("@list_things:value.1.id", responses), 8)
        self.assertEqual(readtest.resolve({"ids": ["@list_things:value.0.id"]}, responses), {"ids": [7]})
        self.assertEqual(readtest.resolve("x/{@list_things:value.0.name}/y", responses), "x/a/y")
        self.assertEqual(readtest.resolve(True, responses), True)
        for missing in ("@other:id", "@list_things:value.5.id", "@list_things:value.1.name"):
            with self.subTest(ref=missing), self.assertRaises(readtest.Unresolved):
                readtest.resolve(missing, responses)

    def test_first_item_and_value(self):
        self.assertEqual(readtest.first_item({"count": 1, "value": [{"id": 1}]}), {"id": 1})
        self.assertIsNone(readtest.first_item({"count": 0, "value": []}))
        self.assertEqual(readtest.first_item({"id": 3}), {"id": 3})
        self.assertEqual(readtest.value_for("pullRequestId", {"pullRequestId": 5, "id": 9}), 5)
        self.assertEqual(readtest.value_for("repositoryId", {"id": "r", "name": "n"}), "r")

    def test_fill_chains_ids(self):
        manuals, _ = run_generate()
        tools = {t["name"]: t for t in manuals["t_main"]["tools"]}
        test = readtest.ReadTest("utcp", {}, {"project": "P"})
        try:
            args, reason = test.fill(tools["get_widget_by_id"])
            self.assertEqual((args, reason), (None, "no value for {widgetId}"))
            test.items[readtest.normalize(readtest.template_of(tools["alpha_list_widgets"]))] = {"id": "w1"}
            args, reason = test.fill(tools["get_widget_by_id"])
            self.assertEqual(args, {"project": "P", "widgetId": "w1"})
            args, reason = test.fill(tools["alpha_list_widgets"])
            self.assertEqual(reason, "required parameter kind")
            test.seeds["kind"] = "round"
            args, _ = test.fill(tools["alpha_list_widgets"])
            self.assertEqual(args, {"project": "P", "kind": "round", "$top": 5})
            self.assertEqual(test.fill(tools["run_query"]), (None, "no sample body"))
        finally:
            test.close()


def find_utcp():
    path = os.environ.get("UTCP_BIN") or shutil.which("utcp")
    found = path if path and os.access(path, os.X_OK) else None
    if not found and os.environ.get("REQUIRE_UTCP"):
        raise RuntimeError(f"REQUIRE_UTCP is set but no usable utcp binary was found (UTCP_BIN={path!r})")
    return found


@unittest.skipUnless(find_utcp(), "no utcp binary (set UTCP_BIN)")
class UtcpLoadTest(unittest.TestCase):
    """The expected manuals register in a throwaway utcp, and dry runs build the right requests."""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.env = dict(os.environ)
        for name in ("CONFIG", "CACHE", "STATE"):
            self.env[f"UTCP_{name}_DIR"] = str(self.tmp / name.lower())
        self.env["AZURE_DEVOPS_EXT_PAT"] = "not-a-real-token"

    def tearDown(self):
        shutil.rmtree(self.tmp)

    def utcp(self, *args, stdin=None):
        result = subprocess.run([find_utcp(), *args, "--direct"], env=self.env, capture_output=True, text=True,
                                input=stdin, timeout=60)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout

    def test_register_schema_dry_run(self):
        self.utcp("register", "t_main", str(EXPECTED / "t_main.utcp.json"))
        manual = json.loads((EXPECTED / "t_main.utcp.json").read_text(encoding="utf-8"))
        for tool in manual["tools"]:
            schema = json.loads(self.utcp("schema", f"t_main.{tool['name']}"))
            self.assertEqual(schema["inputs"], tool["inputs"])
        out = json.loads(self.utcp("call", "t_main.run_query", "--dry-run", "--args-stdin",
                                   stdin='{"project": "My Project", "body": {"text": "x"}, "$top": 2}'))
        self.assertEqual(out["url"], "https://dev.azure.com/contoso/My%20Project/_apis/alpha/query?api-version=7.1")
        self.assertEqual(out["method"], "POST")
        self.assertNotIn("not-a-real-token", json.dumps(out))
        out = json.loads(self.utcp("call", "t_main.get_thing_template", "--dry-run", "--args-stdin",
                                   stdin='{"project": "P", "type": "Bug"}'))
        self.assertEqual(out["url"], "https://dev.azure.com/contoso/P/_apis/alpha/things/%24Bug?api-version=7.1")


if __name__ == "__main__":
    unittest.main()
