#!/usr/bin/env python3
"""Load test ("hammer") for utcp: several simulated agents call one daemon at the same time.

Run on demand; it is not part of `cargo test`. From `utcp-rs/`:

    cargo build --release
    python3 tests/load/hammer.py                                   # all workloads, 60 s
    python3 tests/load/hammer.py --seconds 10 --agents 4 --workloads mcp,http
    python3 tests/load/hammer.py --kill-mcp-at 20                  # also crash the MCP server

Everything runs in a throwaway utcp home (temp dir, own socket, own daemon), so your real
manuals and daemon are not touched. Each call is its own `utcp` process, the way an agent calls
it, with a unique input, and each answer is checked against that call's own expected value.
All workloads run at once, each with `--agents` agents calling back to back:

  search  `utcp search` over the test catalog; each answer must match a reference taken first.
  http    an OpenAPI tool served by a mock HTTP server inside this script.
  cli     a CLI tool (this Python interpreter echoing stdin), like `d2.format`.
  mcp     a stdio MCP server (the Node test fixture), shared by all calls through the daemon.

`--kill-mcp-at S` kills the MCP server S seconds in. Calls in flight at that moment may fail
with "closed its output"; the next calls must succeed on a new server.

Prints calls, failures, wrong answers and latency per workload, plus the daemon's peak memory
(Linux), and exits 1 on any failure other than calls lost to `--kill-mcp-at`.

Needs Python 3.8+, Node.js (for `mcp`) and a built `utcp`.
"""
import argparse
import itertools
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

UTCP_RS = Path(__file__).resolve().parents[2]
FIXTURE = UTCP_RS / "crates" / "utcp-core" / "tests" / "fixtures" / "mock_mcp_server.cjs"
EXE = ".exe" if os.name == "nt" else ""
WORKLOADS = ["search", "http", "cli", "mcp"]
RUN = uuid.uuid4().hex[:6]


class MockApi(BaseHTTPRequestHandler):
    """`GET /openapi.json` describes one tool; `GET /echo/<tag>` answers `{"tag": "<tag>"}`."""

    protocol_version = "HTTP/1.1"
    # Headers and body go out in two writes; with Nagle on, each call stalls ~40 ms on the
    # client's delayed ACK, which would measure this mock instead of utcp.
    disable_nagle_algorithm = True

    def do_GET(self):
        if self.path == "/openapi.json":
            body = {
                "openapi": "3.0.3",
                "info": {"title": "hammer mock API", "version": "1.0.0"},
                "servers": [{"url": "http://" + self.headers.get("Host", "127.0.0.1")}],
                "paths": {
                    "/echo/{tag}": {
                        "get": {
                            "operationId": "echo",
                            "summary": "Echo a tag back",
                            "parameters": [
                                {"name": "tag", "in": "path", "required": True,
                                 "schema": {"type": "string"}}
                            ],
                            "responses": {"200": {"description": "the tag"}},
                        }
                    }
                },
            }
            status = 200
        elif self.path.startswith("/echo/"):
            body, status = {"tag": self.path[len("/echo/"):]}, 200
        else:
            body, status = {"error": "not found"}, 404
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *args):
        pass


class Utcp:
    """Runs the `utcp` binary against an isolated home."""

    def __init__(self, binary, home):
        self.binary = str(binary)
        self.env = dict(
            os.environ,
            UTCP_CONFIG_DIR=str(home / "config"),
            UTCP_CACHE_DIR=str(home / "cache"),
            UTCP_STATE_DIR=str(home / "state"),
            UTCP_MEDIA_DIR=str(home / "media"),
            HOME=str(home / "home"),
            USERPROFILE=str(home / "home"),
        )
        if os.name == "nt":
            self.env["UTCP_SOCKET_PATH"] = r"\\.\pipe\utcp-hammer-" + RUN
        else:
            self.env["UTCP_SOCKET_PATH"] = str(home / "utcp.sock")

    def run(self, *args, timeout=120):
        try:
            p = subprocess.run([self.binary, *args], env=self.env, capture_output=True,
                               text=True, timeout=timeout)
            return p.returncode, p.stdout, p.stderr
        except subprocess.TimeoutExpired:
            return -1, "", f"timed out after {timeout}s"

    def ok(self, *args):
        rc, out, err = self.run(*args)
        if rc != 0:
            sys.exit(f"setup failed: utcp {' '.join(args)}\n{err.strip()}")
        return out

    def daemon_pid(self):
        _, out, _ = self.run("daemon", "status")
        for line in out.splitlines():
            key, _, value = line.partition(":")
            if key.strip() == "PID" and value.strip().isdigit():
                return int(value.strip())
        return None


def rss_mb(pid):
    try:
        with open(f"/proc/{pid}/status") as fh:
            for line in fh:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1]) / 1024
    except OSError:
        pass
    return None


def kill_process(pid):
    if os.name == "nt":
        subprocess.run(["taskkill", "/F", "/PID", str(pid)], capture_output=True)
    else:
        os.kill(pid, signal.SIGKILL)


def parse(out):
    try:
        return json.loads(out)
    except ValueError:
        return None


def setup(utcp, home, workloads, api_port):
    """Register the test manuals and take the search reference."""
    if "cli" in workloads:
        manual = {
            "manual_version": "1.0.0",
            "utcp_version": "1.0.0",
            "info": {"title": "hammer CLI echo", "version": "1.0.0"},
            "tools": [{
                "name": "echo",
                "description": "Echo stdin back (hammer load test)",
                "inputs": {"type": "object", "properties": {"text": {"type": "string"}},
                           "required": ["text"]},
                "outputs": {"type": "object"},
                "tool_call_template": {
                    "call_template_type": "cli",
                    "command": sys.executable,
                    "direct_exec": True,
                    "stdin_field": "text",
                    "args_template": ["-c", "import sys; sys.stdout.write(sys.stdin.read())"],
                },
            }],
        }
        path = home / "hammer-cli.utcp.json"
        path.write_text(json.dumps(manual))
        utcp.ok("register", "hammercli", str(path))
    if "http" in workloads:
        utcp.ok("register", "hammerhttp", f"http://127.0.0.1:{api_port}/openapi.json")
    if "mcp" in workloads or "search" in workloads:
        if not shutil.which("node"):
            sys.exit("the mcp and search workloads need Node.js (`node`) on PATH")
        utcp.ok("import-mcp", "hammermcp", "--command", "node", "--arg", str(FIXTURE),
                "--register")
    utcp.ok("daemon", "start")
    for _ in range(100):
        if utcp.daemon_pid():
            break
        time.sleep(0.05)
    else:
        sys.exit("the test daemon did not start")

    reference = {}
    if "search" in workloads:
        for query in ["echo", "echo tool", "stats", "crash", "silent tool", "tag back", "stdin"]:
            reference[query] = utcp.ok("search", query, "--json")
        if not any(parse(out) for out in reference.values()):
            sys.exit("setup failed: every search reference is empty")
    return reference


class Results:
    def __init__(self):
        self.lock = threading.Lock()
        self.calls = {}
        self.mcp_pids = []
        self.kill_time = None

    def add(self, workload, record):
        with self.lock:
            self.calls.setdefault(workload, []).append(record)
            pid = record.pop("mcp_pid", None)
            if pid is not None and (not self.mcp_pids or self.mcp_pids[-1] != pid):
                self.mcp_pids.append(pid)


def one_call(utcp, workload, i, reference):
    """Make call number `i` of a workload; return (ok, expected, rc, out, err, mcp_pid)."""
    pid = None
    if workload == "search":
        queries = sorted(reference)
        query = queries[i % len(queries)]
        rc, out, err = utcp.run("search", query, "--json")
        expected = reference[query]
        ok = rc == 0 and out == expected
    elif workload == "http":
        expected = f"h{RUN}-{i}"
        rc, out, err = utcp.run("call", "hammerhttp.echo", json.dumps({"tag": expected}))
        ok = rc == 0 and (parse(out) or {}).get("tag") == expected
    elif workload == "cli":
        expected = f"c{RUN}-{i}"
        rc, out, err = utcp.run("call", "hammercli.echo", json.dumps({"text": expected}))
        ok = rc == 0 and (parse(out) or {}).get("stdout") == expected
    else:
        expected = i
        rc, out, err = utcp.run("call", "hammermcp.echo", json.dumps({"tag": i}))
        answer = parse(out) or {}
        ok = rc == 0 and answer.get("tag") == expected
        pid = answer.get("pid") if ok else None
    return ok, expected, rc, out, err, pid


def agent(utcp, workload, counter, deadline, reference, results):
    while time.time() < deadline:
        i = next(counter)
        started = time.time()
        ok, expected, rc, out, err, pid = one_call(utcp, workload, i, reference)
        record = {"i": i, "ok": ok, "rc": rc, "ms": (time.time() - started) * 1000,
                  "t0": started, "mcp_pid": pid}
        if not ok:
            record.update(expected=str(expected)[:200], got=out[:200], stderr=err.strip()[:300])
        results.add(workload, record)


def killer(results, at, start):
    time.sleep(max(0.0, start + at - time.time()))
    with results.lock:
        pid = results.mcp_pids[-1] if results.mcp_pids else None
    if pid is None:
        print(f"--kill-mcp-at {at}: no MCP server seen yet, nothing killed", flush=True)
        return
    results.kill_time = time.time()
    kill_process(pid)
    print(f"killed MCP server {pid} at {time.time() - start:.1f}s", flush=True)


def percentile(sorted_values, fraction):
    return sorted_values[min(len(sorted_values) - 1, int(len(sorted_values) * fraction))]


def report(results, workloads, elapsed, peak_rss, agents):
    failed_overall = False
    print(f"\n{'workload':8} {'calls':>7} {'calls/s':>8} {'failed':>7} {'wrong':>6} "
          f"{'p50 ms':>7} {'p95 ms':>7} {'max ms':>7}")
    details = []
    for workload in workloads:
        calls = results.calls.get(workload, [])
        bad = [c for c in calls if not c["ok"]]
        lost = []
        if workload == "mcp" and results.kill_time:
            # Calls that were in flight when the server was killed may fail; at most one per agent.
            lost = [c for c in bad if c["rc"] != 0 and "closed its output" in c.get("stderr", "")
                    and c["t0"] <= results.kill_time + 1]
            if len(lost) > agents:
                lost = []
        unexpected = [c for c in bad if c not in lost]
        wrong = sum(1 for c in unexpected if c["rc"] == 0)
        lat = sorted(c["ms"] for c in calls) or [0.0]
        print(f"{workload:8} {len(calls):7} {len(calls) / elapsed:8.1f} "
              f"{len(unexpected) - wrong:7} {wrong:6} {percentile(lat, 0.5):7.0f} "
              f"{percentile(lat, 0.95):7.0f} {lat[-1]:7.0f}")
        if lost:
            details.append(f"{workload}: {len(lost)} call(s) in flight lost to the server kill")
        if not calls:
            details.append(f"{workload}: no calls completed")
            failed_overall = True
        if unexpected:
            failed_overall = True
            for c in unexpected[:3]:
                details.append(f"{workload} call {c['i']}: rc={c['rc']} expected={c['expected']!r} "
                               f"got={c['got']!r} stderr={c['stderr']!r}")
    if "mcp" in workloads:
        details.append(f"MCP server processes used: {results.mcp_pids}")
        if results.kill_time and len(results.mcp_pids) < 2:
            details.append("MCP server was killed but no call succeeded on a new one")
            failed_overall = True
    if peak_rss is not None:
        details.append(f"daemon peak memory: {peak_rss:.1f} MB")
    print("\n" + "\n".join(details))
    print("\nRESULT:", "FAIL" if failed_overall else "PASS")
    return failed_overall


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--seconds", type=float, default=60.0, help="how long to run (default 60)")
    ap.add_argument("--agents", type=int, default=8,
                    help="agents per workload calling back to back (default 8)")
    ap.add_argument("--workloads", default=",".join(WORKLOADS),
                    help="comma-separated subset of " + ",".join(WORKLOADS))
    ap.add_argument("--kill-mcp-at", type=float, metavar="S",
                    help="kill the MCP server S seconds into the run")
    ap.add_argument("--bin", type=Path, default=UTCP_RS / "target" / "release" / f"utcp{EXE}",
                    help="utcp binary (default: target/release/utcp)")
    ap.add_argument("--keep", action="store_true", help="keep the temp utcp home afterwards")
    args = ap.parse_args()

    workloads = [w.strip() for w in args.workloads.split(",") if w.strip()]
    unknown = set(workloads) - set(WORKLOADS)
    if unknown:
        sys.exit(f"unknown workload(s): {', '.join(sorted(unknown))}")
    if not args.bin.exists():
        sys.exit(f"{args.bin} not found; run `cargo build --release` or pass --bin")

    home = Path(tempfile.mkdtemp(prefix="utcp-hammer-"))
    (home / "home").mkdir()
    utcp = Utcp(args.bin, home)
    api = ThreadingHTTPServer(("127.0.0.1", 0), MockApi)
    api.daemon_threads = True
    threading.Thread(target=api.serve_forever, daemon=True).start()
    failed = True
    try:
        reference = setup(utcp, home, workloads, api.server_address[1])
        daemon_pid = utcp.daemon_pid()
        print(f"utcp hammer: {', '.join(workloads)} x {args.agents} agents for {args.seconds:g}s "
              f"(daemon pid {daemon_pid}, home {home})", flush=True)

        results = Results()
        start = time.time()
        deadline = start + args.seconds
        threads = [
            threading.Thread(target=agent, daemon=True,
                             args=(utcp, w, counter, deadline, reference, results))
            for w in workloads
            for counter in [itertools.count()]
            for _ in range(args.agents)
        ]
        if args.kill_mcp_at is not None and "mcp" in workloads:
            threads.append(threading.Thread(target=killer, daemon=True,
                                            args=(results, args.kill_mcp_at, start)))
        for t in threads:
            t.start()
        peak_rss = None
        while any(t.is_alive() for t in threads):
            rss = rss_mb(daemon_pid) if daemon_pid else None
            if rss is not None:
                peak_rss = max(peak_rss or 0.0, rss)
            time.sleep(0.5)
        failed = report(results, workloads, time.time() - start, peak_rss, args.agents)
    finally:
        utcp.run("daemon", "stop")
        api.shutdown()
        if args.keep:
            print(f"kept {home}")
        else:
            shutil.rmtree(home, ignore_errors=True)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
