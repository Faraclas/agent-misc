"""Turn Azure DevOps Swagger 2.0 specs into utcp HTTP manuals.

Input: the parsed spec files of each area (see config.MANUALS). Output: one utcp manual per group, plus a report
(tools per manual, size, required PAT scopes, operations left out and why). Nothing here touches the network.
"""

import json
import re
import urllib.parse

from . import config

PAT_VAR = "AZURE_DEVOPS_EXT_PAT"
# utcp rejects basic auth with an empty user name; Azure DevOps ignores the name and checks only the token.
AUTH = {"auth_type": "basic", "username": "pat", "password": "${" + PAT_VAR + "}"}
BINARY_TYPES = ("application/zip", "application/octet-stream", "image/")
ORG_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9-]{0,48}[A-Za-z0-9]?$")
MAX_DESCRIPTION = 400
BODY_DEPTH = 3
DROP_BODY_PROPERTIES = {"_links", "url"}
HTTP_METHODS = ("get", "put", "post", "delete", "options", "head", "patch")
# Formats JSON Schema knows; the specs also use free text such as "array (string)".
SCHEMA_FORMATS = {"int32", "int64", "date-time", "uuid", "date", "double", "float", "byte", "binary"}


class GenerateError(Exception):
    pass


def snake(text: str) -> str:
    text = re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", text)
    return re.sub(r"[^a-z0-9]+", "_", text.lower()).strip("_")


def one_line(text: str, limit: int = MAX_DESCRIPTION) -> str:
    text = re.sub(r"\s+", " ", text or "").strip()
    if len(text) > limit:
        text = text[: limit - 3].rsplit(" ", 1)[0] + "..."
    return text


def iter_operations(spec: dict):
    """Yield (path, method, operation, path-level parameters) for `paths` and `x-ms-paths`."""
    for key in ("paths", "x-ms-paths"):
        for path, item in (spec.get(key) or {}).items():
            shared = item.get("parameters", [])
            for method, op in item.items():
                if method in HTTP_METHODS:
                    yield path, method, op, shared


def resolve_ref(spec: dict, ref: str) -> tuple:
    if not ref.startswith("#/"):
        raise GenerateError(f"external $ref not supported: {ref}")
    node = spec
    for part in ref[2:].split("/"):
        node = node[part]
    return ref.rsplit("/", 1)[1], node


def api_version_of(spec: dict, op: dict, base_version: str) -> str:
    """The api-version an operation needs, from its `#/parameters/api-Version[-preview.N]` reference."""
    for p in op.get("parameters", []):
        ref = p.get("$ref", "")
        name = ref.rsplit("/", 1)[-1]
        if name.startswith("api-Version"):
            suffix = name[len("api-Version"):]
            if suffix and not re.fullmatch(r"-preview\.\d+", suffix):
                raise GenerateError(f"{op.get('operationId')}: unknown api-version parameter {name}")
            return base_version + suffix
        if p.get("name") == "api-version" and p.get("in") == "query":
            raise GenerateError(f"{op.get('operationId')}: inline api-version parameter, expected a $ref")
    raise GenerateError(f"{op.get('operationId')}: no api-version parameter")


def scopes_of(spec: dict, op: dict) -> list:
    sec = op.get("security", spec.get("security", []))
    return sorted({s for entry in sec for s in entry.get("oauth2", [])})


def param_schema(p: dict) -> dict:
    schema = {}
    ptype = p.get("type", "string")
    fmt = p.get("format", "")
    desc = one_line(p.get("description", ""))
    if fmt.startswith("array ("):
        # Azure DevOps takes these as one comma-separated string.
        ptype = "string"
        if "comma" not in desc.lower():
            desc = (desc + " " if desc else "") + "(comma-separated)"
    elif fmt in SCHEMA_FORMATS:
        schema["format"] = fmt
    schema["type"] = ptype
    if ptype == "array" and "items" in p:
        schema["items"] = {k: v for k, v in p["items"].items() if k in ("type", "enum")}
    if "enum" in p:
        schema["enum"] = p["enum"]
    if "default" in p:
        schema["default"] = p["default"]
    if desc:
        schema["description"] = desc
    return dict(sorted(schema.items()))


def body_schema(spec: dict, schema: dict, depth: int = 0, seen: tuple = ()) -> dict:
    """Inline `$ref` and `allOf`, down to BODY_DEPTH levels, without cycles. Read-only fields are dropped."""
    if "$ref" in schema:
        name, target = resolve_ref(spec, schema["$ref"])
        if name in seen or depth >= BODY_DEPTH:
            out = {"type": "object", "description": one_line(target.get("description") or name)}
            return out
        return body_schema(spec, target, depth, seen + (name,))
    out = {}
    props, required = {}, []
    for part in schema.get("allOf", []):
        merged = body_schema(spec, part, depth, seen)
        props.update(merged.get("properties", {}))
        required += merged.get("required", [])
    stype = schema.get("type") or ("object" if props or "properties" in schema else None)
    if stype:
        out["type"] = stype
    if schema.get("description"):
        out["description"] = one_line(schema["description"])
    for key in ("format", "enum"):
        if key in schema and not (key == "format" and schema[key] not in SCHEMA_FORMATS):
            out[key] = schema[key]
    for name, prop in (schema.get("properties") or {}).items():
        if name in DROP_BODY_PROPERTIES or prop.get("readOnly"):
            continue
        props[name] = body_schema(spec, prop, depth + 1, seen)
    if props:
        out["properties"] = dict(sorted(props.items()))
    required += schema.get("required", [])
    if required:
        out["required"] = sorted(set(required) & set(props) if props else set(required))
    if stype == "array" and "items" in schema:
        out["items"] = body_schema(spec, schema["items"], depth + 1, seen)
    if "additionalProperties" in schema and isinstance(schema["additionalProperties"], dict):
        out["additionalProperties"] = body_schema(spec, schema["additionalProperties"], depth + 1, seen)
    return out


def build_url(spec: dict, path: str, org: str, version: str, dropped: list) -> str:
    host = spec.get("host")
    if not host:
        raise GenerateError(f"{spec.get('info', {}).get('title')}: spec has no host")
    base = (spec.get("basePath") or "/").rstrip("/")
    path = path.split("?", 1)[0]
    for name in dropped:
        path = path.replace("/{" + name + "}", "", 1)
    path = path.replace("{organization}", urllib.parse.quote(org, safe=""))
    # utcp replaces ${NAME} with variables; a literal `$` before a path parameter has to be percent-encoded.
    path = path.replace("${", "%24{")
    return f"https://{host}{base}{path}?api-version={version}"


def is_binary(op: dict) -> bool:
    produces = op.get("produces") or ["application/json"]
    return all(any(t.startswith(b) for b in BINARY_TYPES) for t in produces)


def select(area: str, method: str, op: dict, conf=config):
    """Return None to include the operation, or the reason it is left out."""
    op_id = op.get("operationId", "")
    if method == "get":
        if op_id in conf.SKIP_GETS.get(area, {}):
            return "skipped: " + conf.SKIP_GETS[area][op_id]
        if is_binary(op):
            return "binary response"
        return None
    if method in ("delete", "head", "options"):
        return f"{method.upper()} never included"
    if op_id in conf.POST_READS.get(area, []) or op_id in conf.WRITES.get(area, []):
        return None
    return "write not selected"


def build_tool(spec: dict, area: str, path: str, method: str, op: dict, shared: list, org: str, api_version: str,
               conf=config):
    op_id = op["operationId"]
    params = []
    for p in shared + op.get("parameters", []):
        if "$ref" in p:
            _, p = resolve_ref(spec, p["$ref"])
        params.append(p)
    drop = conf.DROP_OPTIONAL_PATH_PARAMS.get(area, set())
    dropped = [p["name"] for p in params if p.get("in") == "path" and p["name"] in drop and
               p.get("x-ms-required") is False]
    props, required, body_field, header_fields = {}, [], None, []
    for p in params:
        name, where = p.get("name"), p.get("in")
        if name in ("organization", "api-version") or name in dropped:
            continue
        if where == "body":
            body_field = "body"
            schema = body_schema(spec, p.get("schema", {}))
            if p.get("description"):
                schema["description"] = one_line(p["description"])
            props["body"] = schema
            if p.get("required"):
                required.append("body")
        elif where in ("path", "query", "header"):
            if name == "body":
                raise GenerateError(f"{op_id}: a {where} parameter named 'body' clashes with the body field")
            props[name] = param_schema(p)
            if where == "path" or p.get("required"):
                required.append(name)
            if where == "header":
                header_fields.append(name)
        else:
            raise GenerateError(f"{op_id}: unsupported parameter location '{where}'")
    for name in conf.REQUIRED_PARAMS.get(area, {}).get(op_id, []):
        if name not in props:
            raise GenerateError(f"{area}/{op_id}: REQUIRED_PARAMS names '{name}', which the operation does not have")
        required.append(name)
    version = api_version_of(spec, op, api_version)
    produces = op.get("produces") or ["application/json"]
    accept = "application/json" if "application/json" in produces else produces[0]
    template = {
        "call_template_type": "http",
        "http_method": method.upper(),
        "url": build_url(spec, path, org, version, dropped),
        "headers": {"Accept": accept},
    }
    if body_field:
        consumes = op.get("consumes") or ["application/json"]
        template["headers"]["Content-Type"] = consumes[0]
        template["body_field"] = body_field
    if header_fields:
        template["header_fields"] = sorted(header_fields)
    template["auth"] = dict(AUTH)
    inputs = {"type": "object", "properties": dict(sorted(props.items()))}
    if required:
        inputs["required"] = sorted(set(required))
    tag = op.get("tags", [""])[0]
    description = one_line(op.get("description", "")) or op_id.replace("_", ": ")
    return {
        "operation_id": op_id,
        "method_name": op.get("x-ms-vss-method"),
        "scopes": scopes_of(spec, op),
        "tool": {
            "name": None,
            "description": description,
            "tags": ["azure-devops", area] + ([snake(tag)] if tag else []),
            "inputs": inputs,
            "outputs": {"type": "string" if accept.startswith("text/") else "object"},
            "tool_call_template": template,
        },
    }


def singular(words: str) -> str:
    head, _, last = words.rpartition("_")
    if last.endswith("ies"):
        last = last[:-3] + "y"
    elif last.endswith("sses") or last.endswith("xes"):
        last = last[:-2]
    elif last.endswith("s") and not last.endswith("ss"):
        last = last[:-1]
    return f"{head}_{last}" if head else last


def base_name(op_id: str) -> str:
    tag, _, method = op_id.partition("_")
    name = snake(method or tag)
    if "_" not in name:
        # A bare verb needs its resource: "Teams_List" -> "list_teams", "Teams_Get" -> "get_team".
        resource = snake(tag)
        name = f"{name}_{resource if name == 'list' else singular(resource)}"
    return name


def assign_names(manual: str, entries: list, conf=config):
    """Name tools after Microsoft's client method (`x-ms-vss-method`, e.g. GetTeamDaysOff -> get_team_days_off),
    else after the operationId. On a clash, prefix the area; NAMES in the config overrides either."""
    overrides = conf.NAMES
    for e in entries:
        e["name"] = (overrides.get(e["area"], {}).get(e["operation_id"]) or
                     (snake(e["method_name"]) if e.get("method_name") else base_name(e["operation_id"])))
    counts = {}
    for e in entries:
        counts[e["name"]] = counts.get(e["name"], 0) + 1
    for e in entries:
        if counts[e["name"]] > 1 and e["operation_id"] not in overrides.get(e["area"], {}):
            e["name"] = f"{snake(e['area'])}_{e['name']}"
    seen = {}
    for e in entries:
        if e["name"] in seen:
            raise GenerateError(
                f"{manual}: tool name '{e['name']}' is used by {seen[e['name']]} and {e['area']}/{e['operation_id']};"
                " add one of them to config.NAMES"
            )
        seen[e["name"]] = f"{e['area']}/{e['operation_id']}"
        e["tool"]["name"] = e["name"]


def scope_info(specs: dict) -> dict:
    info = {}
    for files in specs.values():
        for spec in files:
            scopes = (spec.get("securityDefinitions", {}).get("oauth2", {}) or {}).get("scopes", {})
            for name, desc in scopes.items():
                info.setdefault(name, one_line(desc, 300))
    return info


def is_write_scope(scope: str) -> bool:
    return scope.endswith(("_write", "_manage", "_full", "_execute"))


def covering_scopes(scope_sets: list) -> list:
    """Smallest practical set of scopes so every operation has one of its own (an operation listing several
    scopes accepts any of them). Prefers read scopes and scopes already chosen."""
    chosen = set()
    rank = lambda s: (is_write_scope(s), s)  # noqa: E731
    for scopes in sorted((s for s in scope_sets if s), key=len):
        if not chosen.intersection(scopes):
            chosen.add(min(scopes, key=rank))
    return sorted(chosen)


def minimal_scopes(scopes: set) -> list:
    """Drop a scope when a broader one of the same family is also needed (vso.work_write covers vso.work)."""
    order = ["", "_write", "_execute", "_manage", "_full"]

    def split(s):
        for suffix in reversed(order[1:]):
            if s.endswith(suffix):
                return s[: -len(suffix)], order.index(suffix)
        return s, 0

    best = {}
    for s in scopes:
        family, rank = split(s)
        if family not in best or rank > best[family][1]:
            best[family] = (s, rank)
    return sorted(v[0] for v in best.values())


def generate(specs: dict, org: str, api_version: str, source: dict, conf=config) -> tuple:
    """specs: area -> list of parsed spec files. Returns ({manual name: manual}, report)."""
    if not ORG_RE.match(org):
        raise GenerateError(f"'{org}' is not a valid Azure DevOps organization name")
    manuals, report = {}, {"api_version": api_version, "source": source, "manuals": {}, "left_out": []}
    descriptions = scope_info(specs)
    all_scopes, all_scope_sets = set(), []
    for manual_name, mconf in conf.MANUALS.items():
        entries = []
        for area in mconf["areas"]:
            for spec in specs.get(area, []):
                for path, method, op, shared in iter_operations(spec):
                    reason = select(area, method, op, conf)
                    if reason:
                        report["left_out"].append(
                            {"manual": manual_name, "area": area, "method": method.upper(),
                             "operation_id": op.get("operationId"), "reason": reason}
                        )
                        continue
                    entry = build_tool(spec, area, path, method, op, shared, org, api_version, conf)
                    entry["area"] = area
                    write_only = [s for s in entry["scopes"] if is_write_scope(s)]
                    is_write = op.get("operationId") in conf.WRITES.get(area, [])
                    if (entry["scopes"] and len(write_only) == len(entry["scopes"]) and not is_write and
                            not set(write_only) & set(conf.ALLOW_WRITE_SCOPES)):
                        report["left_out"].append(
                            {"manual": manual_name, "area": area, "method": method.upper(),
                             "operation_id": op.get("operationId"),
                             "reason": "read needs a write scope: " + ", ".join(write_only)}
                        )
                        continue
                    entries.append(entry)
        if not entries:
            continue
        entries.sort(key=lambda e: (e["area"], e["operation_id"]))
        assign_names(manual_name, entries, conf)
        entries.sort(key=lambda e: e["name"])
        scopes = covering_scopes([e["scopes"] for e in entries])
        all_scope_sets.extend(e["scopes"] for e in entries)
        all_scopes.update(s for e in entries for s in e["scopes"])
        missing = [a for a in mconf["areas"] if a not in specs]
        manual = {
            "manual_version": "1.0.0",
            "utcp_version": "1.0.0",
            "info": {
                "title": mconf["title"],
                "version": api_version,
                "description": (
                    f"{mconf['title']} for the '{org}' organization (REST API {api_version}). Generated by "
                    f"azdo-manuals from Microsoft's REST API specs. Authenticates with the PAT in {PAT_VAR}."
                ),
            },
            "tools": [e["tool"] for e in entries],
        }
        manuals[manual_name] = manual
        text = json.dumps(manual, separators=(",", ":"))
        report["manuals"][manual_name] = {
            "hint": mconf["hint"],
            "areas": mconf["areas"],
            "areas_without_spec": missing,
            "tools": len(entries),
            "reads": sum(1 for e in entries if e["tool"]["tool_call_template"]["http_method"] == "GET"
                         or e["operation_id"] in conf.POST_READS.get(e["area"], [])),
            "bytes": len(text),
            "approx_tokens": len(text) // 4,
            "scopes": scopes,
            "tools_without_scope": sorted(e["name"] for e in entries if not e["scopes"]),
        }
    needed = covering_scopes(all_scope_sets)
    report["scopes"] = [{"scope": s, "description": descriptions.get(s, "")} for s in minimal_scopes(set(needed))]
    report["scopes_all"] = sorted(all_scopes)
    return manuals, report
