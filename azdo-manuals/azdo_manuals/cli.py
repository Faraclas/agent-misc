"""Command line: `python3 -m azdo_manuals generate --org <org> --out <dir>`."""

import argparse
import json
import os
import shutil
import sys
from pathlib import Path

from . import __version__, config
from .archive import (
    PINS_FILE,
    Archive,
    ArchiveError,
    default_cache_dir,
    discover_pins,
    fetch_file,
    load_pins,
    load_spec,
)
from .generate import GenerateError, generate


def load_specs(pins: dict, cache_dir: Path, archive: Archive, log=print) -> dict:
    specs = {}
    for area in config.all_areas():
        for pin in pins["areas"].get(area, []):
            specs.setdefault(area, []).append(load_spec(fetch_file(archive, pin, cache_dir)))
    if archive.requests:
        log(f"Downloaded {archive.requests} file(s) from the archive (cache: {cache_dir}).")
    return specs


def write_json(path: Path, data):
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n", encoding="utf-8", newline="\n")
    os.replace(tmp, path)


def scopes_markdown(report: dict, org: str) -> str:
    lines = [
        f"# PAT scopes for the azdo manuals ({org})",
        "",
        f"REST API {report['api_version']}, specs from archive revision {report['source']['revision'][:12]}.",
        "Create the token at https://dev.azure.com/" + org + "/_usersSettings/tokens with these scopes:",
        "",
        "| Scope | What it allows |",
        "| --- | --- |",
    ]
    lines += [f"| `{s['scope']}` | {s['description']} |" for s in report["scopes"]]
    lines += ["", "| Manual | Tools | Approx. tokens if fully loaded | Scopes |", "| --- | --- | --- | --- |"]
    for name, m in report["manuals"].items():
        lines.append(f"| `{name}` | {m['tools']} | {m['approx_tokens']} | {', '.join(m['scopes'])} |")
    return "\n".join(lines) + "\n"


def cmd_generate(args) -> int:
    pins = load_pins(Path(args.pins))
    cache = Path(args.cache) if args.cache else default_cache_dir()
    archive = Archive()
    specs = load_specs(pins, cache, archive)
    source = {k: pins[k] for k in ("revision", "revision_date", "revision_message")}
    manuals, report = generate(specs, args.org, pins["api_version"], source)
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    for name, manual in manuals.items():
        write_json(out / f"{name}.utcp.json", manual)
    write_json(out / "report.json", report)
    (out / "scopes.md").write_text(scopes_markdown(report, args.org), encoding="utf-8", newline="\n")
    print(f"Wrote {len(manuals)} manuals to {out}:")
    for name, m in report["manuals"].items():
        print(f"  {name:18} {m['tools']:4} tools  ~{m['approx_tokens']:6} tokens  {', '.join(m['scopes'])}")
    print("PAT scopes needed: " + ", ".join(s["scope"] for s in report["scopes"]))
    for area, versions in pins.get("missing", {}).items():
        print(f"Not included: area '{area}' has no {pins['api_version']} spec (has {', '.join(versions)}).")
    print(f"Register with: utcp register <name> {out}{os.sep}<name>.utcp.json --hint '<hint>' (see README).")
    return 0


def cmd_pins(args) -> int:
    path = Path(args.pins)
    existing = load_pins(path) if path.exists() and not args.fresh else None
    archive = Archive()

    def save(result):
        write_json(path, result)

    try:
        discover_pins(archive, args.revision, args.api_version, config.all_areas(), existing=existing, save=save)
    finally:
        print(f"{archive.requests} archive request(s), {archive.remaining} left this hour.")
    print(f"Wrote {path}.")
    return 0


def cmd_check_routes(args) -> int:
    from . import routes

    pat = os.environ.get("AZURE_DEVOPS_EXT_PAT")
    if not pat:
        print("azdo-manuals: set AZURE_DEVOPS_EXT_PAT to a PAT for the organization", file=sys.stderr)
        return 2
    folder = Path(args.manuals)
    manuals = {p.name[: -len(".utcp.json")]: json.loads(p.read_text(encoding="utf-8"))
               for p in sorted(folder.glob("*.utcp.json"))}
    if not manuals:
        print(f"azdo-manuals: no *.utcp.json in {folder}", file=sys.stderr)
        return 2
    try:
        result = routes.check(manuals, args.org, pat)
    except RuntimeError as e:
        print(f"azdo-manuals: {e}", file=sys.stderr)
        return 1
    total = sum(len(m["tools"]) for m in manuals.values())
    print("Route lists: " + ", ".join(f"{b} ({n} routes)" for b, n in result["bases"].items()))
    for base, err in result["base_errors"].items():
        print(f"Could not read the route list of {base}: {err}")
    print(f"{result['matched']} of {total} tools match a live route at a supported version.")
    for name in result["unmatched"]:
        print(f"  no route:    {name}")
    for line in result["version"]:
        print(f"  version:     {line}")
    for name in result["unchecked"]:
        print(f"  not checked: {name}")
    return 0 if result["matched"] == total else 1


def cmd_read_test(args) -> int:
    from . import readtest

    if not os.environ.get("AZURE_DEVOPS_EXT_PAT"):
        print("azdo-manuals: set AZURE_DEVOPS_EXT_PAT to a PAT for the organization", file=sys.stderr)
        return 2
    utcp = args.utcp or shutil.which("utcp")
    if not utcp:
        print("azdo-manuals: no utcp binary (use --utcp)", file=sys.stderr)
        return 2
    folder = Path(args.manuals)
    manuals = {p.name[: -len(".utcp.json")]: json.loads(p.read_text(encoding="utf-8"))
               for p in sorted(folder.glob("*.utcp.json"))}
    if args.only:
        manuals = {k: v for k, v in manuals.items() if k in args.only}
    seeds = {"project": args.project}
    if args.team:
        seeds["team"] = args.team
    for s in args.seed or []:
        key, _, value = s.partition("=")
        seeds[key] = value
    test = readtest.ReadTest(utcp, manuals, seeds)
    try:
        test.register(folder)
        results = test.run()
    finally:
        test.close()
    write_json(folder / "readtest.json", results)
    counts = readtest.summary(results)
    print("Results: " + ", ".join(f"{n} {k}" for k, n in sorted(counts.items())))
    print(f"Details (contains organization data, keep private): {folder / 'readtest.json'}")
    failed = [k for k, r in results.items() if r["status"] not in ("ok", "not exercised")]
    return 1 if failed else 0


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(prog="azdo-manuals", description=__doc__)
    parser.add_argument("--version", action="version", version=f"azdo-manuals {__version__}")
    sub = parser.add_subparsers(dest="command", required=True)

    g = sub.add_parser("generate", help="download the pinned specs and write the manuals and the scope report")
    g.add_argument("--org", required=True, help="Azure DevOps organization name (dev.azure.com/<org>)")
    g.add_argument("--out", required=True, help="folder for the manuals; keep it private, they name your org")
    g.add_argument("--cache", help="spec cache folder (default: ~/.cache/azdo-manuals)")
    g.add_argument("--pins", default=str(PINS_FILE), help=argparse.SUPPRESS)
    g.set_defaults(func=cmd_generate)

    p = sub.add_parser("pins", help="maintainers: record the spec file hashes of an archive revision in pins.json")
    p.add_argument("--revision", default=config.ARCHIVE_REVISION)
    p.add_argument("--api-version", default=config.API_VERSION)
    p.add_argument("--fresh", action="store_true", help="ignore the existing pins.json instead of continuing it")
    p.add_argument("--pins", default=str(PINS_FILE), help=argparse.SUPPRESS)
    p.set_defaults(func=cmd_pins)

    r = sub.add_parser("check-routes", help="check generated manuals against the organization's live route list "
                       "(read-only; PAT from AZURE_DEVOPS_EXT_PAT)")
    r.add_argument("--org", required=True)
    r.add_argument("--manuals", required=True, help="folder written by generate")
    r.set_defaults(func=cmd_check_routes)

    t = sub.add_parser("read-test", help="call every read tool through utcp, chaining ids (read-only; PAT from "
                       "AZURE_DEVOPS_EXT_PAT)")
    t.add_argument("--manuals", required=True, help="folder written by generate")
    t.add_argument("--project", required=True, help="project to read from")
    t.add_argument("--team", help="team for board and iteration tools")
    t.add_argument("--seed", action="append", help="fix a path parameter: name=value (repeatable)")
    t.add_argument("--only", action="append", help="test only this manual (repeatable)")
    t.add_argument("--utcp", help="utcp binary (default: utcp on PATH)")
    t.set_defaults(func=cmd_read_test)

    args = parser.parse_args(argv)
    try:
        return args.func(args)
    except (ArchiveError, GenerateError) as e:
        print(f"azdo-manuals: {e}", file=sys.stderr)
        return 1
