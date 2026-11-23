# `utcp-rs`

A fast, cross-platform (Linux & Windows), Zero-MCP **Universal Tool Calling Protocol (UTCP)** CLI (`utcp` /
`utcp.exe`), optional `--user` local IPC daemon (`utcp daemon`), and 6-tab companion Terminal UI (`utcp ui`) in Rust.

## Key Features

- **5-Stage Lazy Progressive Disclosure (Small Idle Footprint):** At startup agents load only the two skills' short
  descriptions (about 740 characters, roughly 170 tokens, with three manuals registered), including a dynamic
  `(registered manuals: ...)` list, and discover tools on demand (`utcp search` -> `utcp schema` -> `utcp call`).
- **Per-Manual Hints:** A few words per manual (`--hint` on register/import, `utcp set-hint` / `get-hint`, Tab 1 `h`)
  appear next to its name in that description, e.g. `context7 (library and framework docs)`, so agents know what an
  unclear name is for. Hint words are searchable too (`utcp search "library docs"` finds `context7` tools).
- **Agents Can Manage utcp Themselves:** `utcp sync-ides` installs two skills. `utcp-tools` finds and calls tools;
  `utcp-admin` changes the setup (register, import, secrets, the daemon, local MCP servers, failed calls) and asks the
  user before changes that affect every agent, such as `deregister` or a daemon restart. Next to it sits the
  [operating guide](./docs/operating.md), the same reference for people and agents.
- **Back-End Context Window Protection:**
  - **Auto-Compact JSON:** Single-line minified JSON emitted automatically for agent callers (`--pretty` for indented
    human inspection) with zero data loss.
  - **Generic `--fields` Projection:** Optional top-level and dotted-path key filtering (`--fields sha,commit.message`)
    across any JSON response or `utcp batch` item without per-API hand-coding. Paths pass through arrays
    (`workflow_runs.id`), and wrapped lists like `{"total_count": N, "jobs": [...]}` accept item keys directly.
  - **Bounded Media Spool (`$XDG_RUNTIME_DIR/utcp-media` on Linux, `%TEMP%\utcp-media` on Windows):** Automatically
    intercepts inline MCP base64 images/blobs into `0600` files with TTL and count pruning
    (`utcp media <status|clear>`).
  - **Files In and Out:** A file answer (an archive, a log, an image) is streamed to the Downloads folder or
    `--out <path>` and the call prints its path; a JSON answer over 10 MB is saved to a file instead of flooding the
    context. Tools that upload take `{"file": "<path>"}`. The calling process reads and writes the files, never the
    daemon, and `max_upload_mb` / `max_download_mb` in `settings.toml` set the limits.
- **Dual-Mode Execution:**
  - **Daemon Mode (`utcp daemon start` / `utcp-daemon.service`):** `< 0.2 ms` Unix Domain Socket / Windows Named Pipe
    IPC with in-memory catalog, live `notify` reloads, `utcp daemon reload-env` shell sync (only env vars registered
    manuals use are kept in RAM), and warm `rustls` HTTP/2 connection pools.
  - **Standalone Direct Mode (Automatic Fallback):** Zero-Tokio synchronous `bincode` index reads (`list`, `search`,
    `schema`) and single-file manual lookups (`call`) directly from OS page cache when the daemon is stopped.
- **Outbound MCP Bridge (`utcp import-mcp`):** Absorbs third-party MCP-only servers (`--command` stdio or `--url` HTTP)
  into lazy UTCP manuals so IDEs and CLIs stay 100% free of MCP configs. `${VAR}` placeholders in `-e`/`-H` are
  resolved for discovery but saved as placeholders, so no secret lands in the manual. Many agents can call one stdio
  server at once: each gets its own answer, a stuck call times out after 60 s, and a crashed server restarts on the
  next call. Servers started through `npx` or `uvx` are stopped with their whole process tree (SIGTERM first, then
  SIGKILL after 2 s), so none are left running after discovery, `deregister` or `daemon stop`. A `docker run -i`
  container stops when its stdin closes or it gets the forwarded SIGTERM; one that ignores both keeps running, so
  add `--init` for such servers.
- **Manage Local MCP Servers:** `utcp daemon servers` shows each server the daemon runs (state, PID, uptime,
  restart count); `utcp daemon restart-server <manual>` and `stop-server <manual>` fix a hung one without
  restarting the daemon. The bundled agent skill teaches agents to do this themselves. Pressing Ctrl+C on a
  direct-mode `utcp call` (no daemon) stops the server that call started, too.
- **Built for Many Agents at Once:** Tested with three agents calling together for a minute (about 158,000 calls,
  each a separate `utcp` process with a unique input): zero errors, zero mixed-up answers.
- **Clear Rate-Limit Errors:** A `429` (or GitHub's `403` with no requests left) becomes `rate limited by
  api.github.com (HTTP 403): retry after 1233 s (...)`, so agents wait instead of retrying into the same limit.
- **Debug Without Disk Logging:** `utcp log recent [--failed]` shows the daemon's last 500 calls from memory, with
  the full error text of failures (secrets redacted).
- **Secrets Stay Where You Keep Them:** Manuals store `${VAR}` placeholders only. Values come from `.env`, your shell,
  or a secrets command (`[secrets]` in `settings.toml`) that prints them from a shell file, `pass` or a keyring, so the
  daemon has them after a reboot without copying them anywhere. `utcp doctor` shows where each one comes from.
- **Manual Lifecycle:** `utcp deregister` takes a manual out of search but keeps its settings, hint, and saved copy;
  `utcp register <name>` (no source) brings it back offline; `--purge-cache` deletes it for good.
- **Staging:** `utcp stage` adds a manual (or a new version of one) without agents seeing it. `utcp diff` shows the
  tools it adds, removes and changes, `utcp call --staged` tests it with the real secrets, `utcp promote` makes it
  active, and `utcp rollback` brings back the version it replaced.
- **6-Tab Companion TUI (`utcp ui`):** Interactive `ratatui` interface: 1 Manuals (`h` edits a hint, `a` shows
  deregistered manuals), 2 Search (`t` tries the selected tool: dry run or real call), 3 Import (OpenAPI / UTCP specs
  or MCP servers by URL or command, `m` switches), 4 Settings & Skills, 5 Live Activity (`Enter` shows a failed call's
  error, with secrets redacted), and 6 Daemon Monitor (the `--user` daemon and the media spool). `?` opens help for
  the current tab.

## Installation

The [operating guide](./docs/operating.md) (First Setup) covers the steps after installing: the agent skills, the
daemon at login, and secrets.

- **Gentoo:** `dev-util/utcp` in the [`adaptive-overlay`](https://github.com/Faraclas/adaptive-overlay) overlay. It
  builds from the release source and installs `/usr/bin/utcp`, the systemd user unit, and the docs in
  `/usr/share/doc/utcp-*/`.

  ```sh
  eselect repository add adaptive-overlay git https://github.com/Faraclas/adaptive-overlay.git
  emaint sync -r adaptive-overlay
  emerge --ask dev-util/utcp
  ```

- **Other Linux (x86_64):** the project's GitHub Releases (tags `utcp-rs-v*`) have
  `utcp-x86_64-unknown-linux-musl.tar.gz`, a static binary that runs on any distribution, provided as-is. It also
  holds the systemd unit (see below), `operating.md` and `secret-source.sh`. Copy `utcp` to `~/.local/bin/`.
- **Windows:** the release zip has `install.cmd`, which installs utcp for your account, with no admin rights
  needed. Its `README.txt` says how to run it.
- **From source** (the tests also need Node.js, for the mock MCP servers):

  ```sh
  cd utcp-rs
  cargo build --release -p utcp-cli
  cargo test --workspace
  install -Dm755 target/release/utcp ~/.local/bin/utcp
  ```

## Quick Start

```sh
# Install the utcp-tools and utcp-admin skills across Zed, VS Code, Copilot CLI, agy, and Hermes
utcp sync-ides --global

# Register an OpenAPI spec, UTCP manual, or third-party MCP server (by URL, or by the command that starts it)
utcp register myagent http://127.0.0.1:8765/utcp --hint 'my agent container'
utcp import-mcp context7 --url https://mcp.context7.com/mcp --register --hint 'library and framework docs'
utcp import-mcp everything --command npx --arg -y --arg @modelcontextprotocol/server-everything --register
# Secrets: pass placeholders; values come from ~/.config/utcp/.env, your shell, or a secrets command
utcp import-mcp demo --command /usr/bin/some-mcp -e 'API_KEY=${DEMO_API_KEY}' --register

# Take a manual out of search (settings + saved copy kept), restore it offline, or delete it for good
utcp deregister myagent
utcp register myagent
utcp deregister myagent --purge-cache

# Stage a manual to review and test it before agents see it, then make it active (or discard it)
utcp stage petstore ./petstore.utcp.json --hint 'pet store demo API'
utcp diff
utcp call --staged petstore.get_pet '{"petId": 1}'
utcp promote

# After a promote replaced an older version of a manual, bring that version back (run it again to undo)
utcp rollback petstore

# Check paths, the daemon, staged manuals, the secrets command and .env, each secret, and the skills
utcp doctor

# Add, check, or clear the few words agents see next to a manual's name in the skill
utcp set-hint context7 'library and framework docs'
utcp get-hint
utcp set-hint context7 --clear

# (Optional) Start the per-user sub-0.2ms IPC daemon; send it this shell's variables and rerun the secrets command
utcp daemon start
utcp daemon reload-env

# Discover, inspect, and invoke lazily (with optional field projection for JSON answers: --fields a,b.c)
utcp search "library docs"
utcp schema context7.resolve-library-id
utcp call context7.resolve-library-id '{"libraryName": "tokio", "query": "spawn a task"}'

# See recent calls and why any failed (from daemon memory; disk logging can stay off)
utcp log recent --failed

# Local MCP servers (import-mcp --command) the daemon runs: list, restart a hung one, or stop one
utcp daemon servers
utcp daemon restart-server <manual>
utcp daemon stop-server <manual>
```

## Running the Daemon at Login (`systemd --user`)

[`packaging/systemd/utcp-daemon.service`](./packaging/systemd/utcp-daemon.service) is the reference unit. Packages
(such as the Gentoo ebuild) install it as is, with `ExecStart=/usr/bin/utcp daemon run`; then run
`systemctl --user enable --now utcp-daemon`. For a manual install, copy it and point `ExecStart` at your binary:

```sh
install -Dm644 packaging/systemd/utcp-daemon.service ~/.config/systemd/user/utcp-daemon.service
sed -i 's|^ExecStart=/usr/bin/utcp|ExecStart=%h/.local/bin/utcp|' ~/.config/systemd/user/utcp-daemon.service
systemctl --user daemon-reload
systemctl --user enable --now utcp-daemon
```

The unit puts `~/.local/bin` on `PATH` for CLI-based manuals. It does not load `~/.config/utcp/.env` into the
daemon's environment: utcp reads that file itself, so the secrets do not leak to every program the daemon starts.
A daemon started at login does not see what your shell exports (for example from `~/.bashrc`). To give it secrets you
keep in a shell file or a password store, set a secrets command: copy
[`examples/secret-source.sh`](./examples/secret-source.sh), adjust its allowlist, and in
`~/.config/utcp/settings.toml` (utcp creates it with the `[secrets]` section commented out, so remove the `#` signs):

```toml
[secrets]
command = ["/bin/bash", "~/scripts/utcp-secret-source.sh"]
```

The daemon runs it at start and on `utcp daemon reload-env`; `utcp doctor` shows whether it worked. See the
[operating guide](./docs/operating.md) (section 3, Secrets) for the details.

Only `systemd` is supported; units for other init systems are welcome as contributions.

## Load Testing (On Demand)

[`tests/load/hammer.py`](./tests/load/hammer.py) checks that many agents can call one daemon at the same time. It is
not part of `cargo test` or CI; run it after changes to the daemon, concurrency, or transports:

```sh
cargo build --release
python3 tests/load/hammer.py                       # 8 agents each on search, http, cli and mcp for 60 s
python3 tests/load/hammer.py --seconds 10 --agents 4 --workloads mcp,http
python3 tests/load/hammer.py --kill-mcp-at 30      # also kill the MCP server mid-run
```

It uses a throwaway utcp home with its own daemon and local mock servers, so your real manuals and daemon are not
touched. Every call is a separate `utcp` process with a unique input, and every answer is checked. It prints calls,
failures, wrong answers and latency per workload plus the daemon's peak memory, and exits 1 on any failure. Needs
Python 3.8+ and Node.js.

## Architecture & Full Specification

See [`docs/operating.md`](./docs/operating.md) for day-to-day use (setup, adding tools, the daemon, local MCP
servers, failed calls), [`docs/utcp-rs-spec.md`](./docs/utcp-rs-spec.md) for the complete specification,
[`issues/README.md`](./issues/README.md) for tracked upstream `rs-utcp` patches, and [`AGENTS.md`](./AGENTS.md) for
the contributor lockstep rules.
