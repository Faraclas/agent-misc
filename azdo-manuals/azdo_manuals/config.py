"""Which spec areas go into which manual, and which non-GET operations are included.

Area names are the folder names under `specification/` in Microsoft's spec repository. Every GET of an included area
becomes a tool unless it is listed in SKIP_GETS or returns only binary content. Other methods are included only when
listed in POST_READS (POSTs that only read, such as queries and searches) or WRITES (phase B). DELETE and HEAD are
never included.
"""

API_VERSION = "7.1"
ARCHIVE_REVISION = "c7e66d986b369dce462d04c02288a6c26c155002"

MANUALS = {
    "azdo_git": {
        "areas": ["git", "policy"],
        "title": "Azure DevOps Git repositories, pull requests and branch policies",
        "hint": "Azure DevOps repos and PRs",
    },
    "azdo_work": {
        "areas": ["wit", "work"],
        "title": "Azure DevOps work items, queries, boards, iterations and backlogs",
        "hint": "Azure DevOps work items and boards",
    },
    "azdo_pipelines": {
        "areas": ["build", "pipelines", "release", "approvalsAndChecks", "distributedTask", "environments"],
        "title": "Azure DevOps builds, pipelines, releases, approvals, agent pools and environments",
        "hint": "Azure DevOps builds and pipelines",
    },
    "azdo_test": {
        "areas": ["testResults", "testPlan", "test"],
        "title": "Azure DevOps test plans, suites, cases, runs and results",
        "hint": "Azure DevOps test plans and results",
    },
    "azdo_wiki_search": {
        "areas": ["wiki", "search"],
        "title": "Azure DevOps wikis and code, wiki and work item search",
        "hint": "Azure DevOps wiki and search",
    },
    "azdo_core": {
        # No "profile": its only operation lives on app.vssps.visualstudio.com, outside the organization, and answered
        # 401 to an organization-scoped PAT (2026-11-20), the kind of PAT these manuals are meant for.
        "areas": ["core", "graph", "ims"],
        "title": "Azure DevOps projects, teams, processes, users, groups and identities",
        "hint": "Azure DevOps projects, teams, users",
    },
}

# operationId values, per area. POSTs whose only effect is to return data.
POST_READS = {
    "wit": [
        "Wiql_Query By Wiql",
        "Work Items_Get Work Items Batch",
        "Queries_Get Queries Batch",
        "Artifact Uri Query_Query",
    ],
    "search": [
        "Code Search Results_Fetch Code Search Results",
        "Wiki Search Results_Fetch Wiki Search Results",
        "Work Item Search Results_Fetch Work Item Search Results",
    ],
    "graph": [
        "Subject Lookup_Lookup Subjects",
        "Subject Query_Query",
    ],
}

# Phase B: chosen writes, each tested in the sandbox organization before it is listed here.
WRITES = {}

# Some reads accept only a write-level scope (Azure DevOps has no read scope for deployment groups or environments).
# Those reads are left out so the PAT can stay read-only, unless their scope is listed here.
ALLOW_WRITE_SCOPES = set()

# GETs left out on purpose, per area, with the reason next to each.
SKIP_GETS = {
    "build": {
        # Answers 404 "This API is being tested for whether it can be deprecated in the future" (2026-11-20).
        "History_Get": "Microsoft is testing its deprecation (404)",
    },
}

# Parameters the spec marks optional but the service requires (it answers 400 without them), per area and
# operationId. Found by the live read test.
REQUIRED_PARAMS = {
    "git": {
        "Refs Favorites_List": ["repositoryId"],
    },
    "build": {
        "Builds_Get Changes Between Builds": ["fromBuildId", "toBuildId"],
        "Leases_Get Retention Leases By Owner Id": ["ownerId"],
    },
    "approvalsAndChecks": {
        "Check Configurations_List": ["resourceType", "resourceId"],
    },
}

# Explicit tool names, per area and operationId, for names the automatic rule gets wrong or makes clash.
NAMES = {}

# Path parameters the spec marks `x-ms-required: false` that are left out of the URL (with their segment), per area.
# utcp cannot leave a segment out per call (an unfilled `{team}` stays in the URL), so each is either always required
# or always dropped. In `wit`, `team` only matters for team-relative WIQL macros such as @CurrentIteration; in `work`
# (boards, iterations, capacity) it selects the team, so it stays.
DROP_OPTIONAL_PATH_PARAMS = {"wit": {"team"}}


def all_areas():
    return [a for m in MANUALS.values() for a in m["areas"]]
