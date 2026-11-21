"""Live route check: does every generated tool's URL exist on the real service, at a version it supports?

Azure DevOps lists its routes at `OPTIONS https://<host>/<org>/_apis` (needs a PAT). Each route has a template such
as `{project}/_apis/{area}/{resource}/{id}`, plus `minVersion`, `maxVersion` and `releasedVersion`. Routes carry no
HTTP method, so this checks paths and versions only. Read-only; prints tool names and counts, never the token.
"""

import base64
import json
import re
import urllib.error
import urllib.parse
import urllib.request

from . import __version__

USER_AGENT = f"azdo-manuals/{__version__} route-check"


def auth_header(pat: str) -> str:
    return "Basic " + base64.b64encode(f"pat:{pat}".encode()).decode()


def fetch_routes(base: str, pat: str, opener=urllib.request.urlopen) -> list:
    """`base` is `https://<host>/<org>` (or just `https://<host>` for org-less hosts)."""
    req = urllib.request.Request(base.rstrip("/") + "/_apis", method="OPTIONS",
                                 headers={"Authorization": auth_header(pat), "Accept": "application/json",
                                          "User-Agent": USER_AGENT})
    try:
        with opener(req, timeout=60) as resp:
            body = json.loads(resp.read())
    except urllib.error.HTTPError as e:
        e.close()
        raise RuntimeError(f"OPTIONS {base}/_apis failed: HTTP {e.code}") from None
    return body.get("value", body) if isinstance(body, dict) else body


def version_tuple(v: str) -> tuple:
    nums = re.findall(r"\d+", v.split("-", 1)[0])
    return tuple(int(n) for n in nums) or (0,)


def segment_token(seg: str) -> str:
    """Literal segments compare case-insensitively; any segment with a parameter becomes a wildcard."""
    seg = urllib.parse.unquote(seg)
    if "{" in seg:
        return seg.split("{", 1)[0] + "*"
    return seg.lower()


def route_segments(route: dict) -> list:
    template = route["routeTemplate"].replace("{area}", route.get("area", "")).replace(
        "{resource}", route.get("resourceName", ""))
    return [segment_token(s) for s in template.strip("/").split("/") if s]


def segments_match(route_segs: list, tool_segs: list) -> bool:
    """Route parameter segments match anything; trailing ones and leading ones (before `_apis`) may be absent."""
    lead = 0
    while lead < len(route_segs) and route_segs[lead].endswith("*") and route_segs[lead] != "_apis":
        lead += 1
    for skip in range(lead + 1):
        r = route_segs[skip:]
        if len(tool_segs) > len(r):
            continue
        if any(not seg.endswith("*") for seg in r[len(tool_segs):]):
            continue
        if all(_one(rs, ts) for rs, ts in zip(r, tool_segs)):
            return True
    return False


def _one(route_seg: str, tool_seg: str) -> bool:
    if route_seg.endswith("*"):
        return tool_seg.startswith(route_seg[:-1]) or (route_seg == "*")
    return route_seg == tool_seg


def split_tool_url(url: str, org: str) -> tuple:
    """Return (base for OPTIONS, path segments after the org, api-version)."""
    parsed = urllib.parse.urlsplit(url)
    version = urllib.parse.parse_qs(parsed.query).get("api-version", [""])[0]
    segs = [s for s in parsed.path.split("/") if s]
    if segs and segs[0].lower() == org.lower():
        return f"{parsed.scheme}://{parsed.netloc}/{segs[0]}", [segment_token(s) for s in segs[1:]], version
    return f"{parsed.scheme}://{parsed.netloc}", [segment_token(s) for s in segs], version


def version_problem(version: str, route: dict) -> str:
    """Empty when `version` is valid for the route, else why not."""
    v = version_tuple(version)
    if v < version_tuple(route.get("minVersion", "0")):
        return f"below minVersion {route.get('minVersion')}"
    if "-preview" in version:
        if v > version_tuple(route.get("maxVersion", "0")):
            return f"above maxVersion {route.get('maxVersion')}"
        n = re.search(r"preview\.(\d+)", version)
        if n and int(n.group(1)) > int(route.get("resourceVersion", 1)):
            return f"preview.{n.group(1)} above resourceVersion {route.get('resourceVersion')}"
        return ""
    if v > version_tuple(route.get("releasedVersion", "0")):
        return f"not released at {version} (releasedVersion {route.get('releasedVersion')}); needs -preview"
    return ""


def check(manuals: dict, org: str, pat: str, fetch=fetch_routes) -> dict:
    """manuals: name -> manual dict. Returns a report of matched, unmatched and wrong-version tools."""
    routes_by_base = {}
    results = {"matched": 0, "unmatched": [], "version": [], "unchecked": [], "bases": {}, "base_errors": {}}
    for manual_name, manual in manuals.items():
        for tool in manual["tools"]:
            name = f"{manual_name}.{tool['name']}"
            base, segs, version = split_tool_url(tool["tool_call_template"]["url"], org)
            shown = base.replace(org, "<org>")
            if base not in routes_by_base:
                try:
                    routes_by_base[base] = [(route_segments(r), r) for r in fetch(base, pat)]
                    results["bases"][shown] = len(routes_by_base[base])
                except RuntimeError as e:
                    routes_by_base[base] = None
                    results["base_errors"][shown] = str(e).replace(org, "<org>")
            if routes_by_base[base] is None:
                results["unchecked"].append(name)
                continue
            candidates = [r for segs_r, r in routes_by_base[base] if segments_match(segs_r, segs)]
            if not candidates:
                results["unmatched"].append(name)
                continue
            problems = [version_problem(version, r) for r in candidates]
            if all(problems):
                results["version"].append(f"{name} ({version}): {problems[0]}")
            else:
                results["matched"] += 1
    return results
