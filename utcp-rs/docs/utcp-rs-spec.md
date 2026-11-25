# `utcp-rs`: Direct UTCP Rust Workspace Specification (Zero-MCP, Dual-Mode CLI + Daemon, Linux & Windows)

- **Status:** v1.16 (Implemented: Dual-Mode CLI + Daemon, `import-mcp`, `tmpfs` Media Spool, Auto-Compact & `--fields`,
  manual lifecycle (deregister / re-register / purge), env var allow-list, per-manual skill hints, 6-tab TUI with Try
  popup and MCP import, concurrent stdio MCP routing, stdio MCP process-tree cleanup, clear rate-limit errors,
  `utcp log recent`, stdio MCP server management, interruptible direct mode, `utcp-admin` skill and operating guide,
  secrets command, staging (stage / diff / promote / discard / rollback), file uploads and downloads with limits,
  large answers saved to files, stall timeout, self-updating `settings.toml` template, paged answers with `next_page`,
  `--where` list filter, daemon version and features in `daemon status`, arguments and secrets checked before
  sending (inside bodies too), `list` of manuals, grouped skill description, `search --staged`, per-tool `diff`,
  optional URL segments, `--where` comparisons of numbers and dates, keys with dots in `--fields` and `--where`)
- **Repository:** `agent-misc` (`/utcp-rs`)
- **License:** MIT. `utcp-core` is derived from `rs-utcp` 0.3.2 (`MIT OR Apache-2.0`; its Apache-2.0 text is kept as
  `crates/utcp-core/LICENSE-UPSTREAM-RS-UTCP`, see `NOTICE`).
- **Target Platforms:** Gentoo / Linux (`x86_64-unknown-linux-gnu`; static `x86_64-unknown-linux-musl` release archive
  for other distributions) & Windows (`x86_64-pc-windows-msvc`)

---

## 1. Objective & Architectural Guardrails

### Target Outcome

Build a cross-platform (Linux and Windows) direct **Universal Tool Calling Protocol (UTCP)** CLI (`utcp` / `utcp.exe`),
optional machine-wide local daemon (`utcp daemon`), and companion management suite in Rust (`utcp-rs`), shared across
**Zed Editor**, **VS Code**, **GitHub Copilot CLI**, **Google Antigravity CLI (`agy`)**, and **Hermes Agent** via
the Agent Skills standard (`.agents/skills/`, plus `~/.copilot/skills/` for VS Code's Copilot harness).

### Strict Architectural Constraints

1. **NO Per-Editor MCP or Context Servers:** Do not configure `mcpServers` (VS Code / `agy`), `servers`
   (`.vscode/mcp.json`), or `context_servers` (Zed `settings.json`). Do not use JSON-RPC stdio wrapper bridges
   (`@utcp/mcp-bridge`, `rust_mcp_bridge`) that spawn duplicate per-window servers.
2. **Dual-Mode Execution (Standalone Direct Mode + Optional Local Daemon):**
   - **Direct Mode (Always Available, Zero Setup):** If no daemon is running, every `utcp` CLI invocation executes
     standalone in-process against the disk cache (`search_index.bin` and `manuals/*.utcp.json`). It never fails if the
     daemon is stopped.
   - **Daemon Mode (Maximum Performance, Single Machine-Wide Instance):** When `utcp daemon` is running (started via
     `utcp daemon start`, `utcp ui`, or an optional user service), CLI invocations automatically forward requests over a
     local IPC socket (Unix Domain Socket on Linux, Named Pipe on Windows) in **< 0.2 ms**, reusing an in-memory catalog
     index, warm HTTPS/TLS connection pools, persistent WebSockets, and a single-writer activity log ring buffer.
3. **NO Node.js, Python, or System OpenSSL Runtime Dependencies:** Ship a single self-contained native binary (`utcp`
   on Linux, `utcp.exe` on Windows) using `rustls` (with OS native root certificates) so Gentoo (`cargo.eclass`) and
   Windows (`windows-latest`) builds require zero external C/OpenSSL libraries.
4. **Sub-2ms / Sub-0.2ms Hot-Path Execution:**
   - **Daemon Mode:** Local IPC round-trip + in-memory schema lookup in **< 0.2 ms** on Linux and **< 1 ms** on Windows.
   - **Direct Mode:** Standalone process startup, config lookup, and schema resolution in **< 2 ms** on Linux and **< 10
     ms** on Windows.

---

## 2. Repository, Cargo Workspace & Upstream `rs-utcp` Strategy

`agent-misc` is a multi-project repository: each project is self-contained in its own top-level folder. The Rust
project, with its docs, issue log, and packaging, lives under `utcp-rs/`:

```text
agent-misc/
├── LICENSE                            # MIT License
├── README.md                          # Project index
├── AGENTS.md                          # Repository layout rules (one self-contained folder per project)
├── .gitattributes, .gitignore         # LF line endings on every OS; ignored build output and local secrets
├── .markdownlint.json                 # Shared Markdown lint config (rumdl / markdownlint)
├── .github/workflows/utcp-rs.yml      # Linux + Windows tests; static Linux (musl) + Windows release archives (Run workflow)
└── utcp-rs/                           # Cargo Workspace root
    ├── AGENTS.md                      # Lockstep rule: code, tests, skill, docs, packaging change together
    ├── NOTICE                         # Licence note for the code derived from rs-utcp (shipped in the release archives)
    ├── Cargo.toml                     # Workspace manifest (resolver = "2")
    ├── README.md                      # utcp-rs usage & architecture guide
    ├── docs/
    │   ├── utcp-rs-spec.md            # This specification and implementation plan
    │   └── operating.md               # Operating guide for users and agents (installed with the admin skill)
    ├── issues/                        # One file per bug fixed locally (001-018): upstream rs-utcp bugs, plus
    │                                  # bugs of utcp-rs itself (mostly 016-018; each entry says which)
    ├── assets/
    │   ├── SKILL.template.md          # `utcp-tools` skill (finding and calling tools)
    │   └── SKILL-admin.template.md    # `utcp-admin` skill (changing the setup); both written by `utcp sync-ides`
    ├── packaging/
    │   ├── systemd/utcp-daemon.service  # `systemctl --user` reference unit (the only supported init system)
    │   └── windows/
    │       ├── install.cmd            # In the zip: runs install.ps1; pauses if double-clicked
    │       ├── install.ps1            # Per-user Windows install (PATH, skills, logon task), in the zip
    │       └── README.txt             # In the zip: how to run install.cmd
    ├── tests/load/hammer.py           # On-demand load test: many agents, one daemon (not in `cargo test` or CI)
    └── crates/
        ├── utcp-core/                 # Shared library: vendored & feature-gated rs-utcp core,
        │   │                          # catalog, manual loader, bincode index, skills, IPC server/client (`ipc.rs`)
        │   ├── LICENSE-UPSTREAM-RS-UTCP  # rs-utcp's licence text (Apache-2.0)
        │   └── tests/fixtures/        # Test-only mock servers (Node MCP server, run as `node <file>`)
        └── utcp-cli/                  # Single binary (`utcp` / `utcp.exe`):
            ├── src/main.rs            # Fast-path CLI router (IPC client -> Direct fallback)
            ├── src/daemon.rs          # Daemon lifecycle: start (detached or via systemd), stop, `daemon run`
            ├── src/tui/               # Phase 1 Companion UI: `ratatui` + `crossterm` (`utcp ui`)
            └── tests/                 # Subprocess integration tests (isolated temp HOME and dirs)
```

A Phase 2 desktop GUI crate (`utcp-gui`, `eframe`/`egui` or `iced`) is planned but does not exist yet.

### Upstream `rs-utcp` Vendoring & Feature-Gating Strategy

We **do not** rewrite UTCP or its transports from scratch, nor do we depend on the unpatched crates.io `rs-utcp v0.3.2`
package directly. Instead, we **vendor upstream `rs-utcp`'s source modules directly into `utcp-core`** and add the Cargo
feature flags that upstream omitted.

#### Why Upstream `rs-utcp v0.3.2` Cannot Be Pulled Unmodified from crates.io

Upstream `rs-utcp/Cargo.toml` defines **zero Cargo feature flags**, unconditionally compiling three heavy subsystems:

1. `webrtc = "0.14"`: Pulls in ~150 crates (DTLS, SRTP, STUN, TURN, ICE, SDP, MDNS) and `openssl-sys`.
2. `tonic = "0.11"` + `prost` + `tonic-build`: Pulls in ~60 gRPC crates and runs a `build.rs` requiring `protoc`.
3. `rhai = "1.18"`: Embedded scripting engine for the `codemode` plugin, which **currently fails to compile** on
   crates.io because `rhai 1.27` uses `smartstring` without enabling `smartstring/serde`.
4. `reqwest` & `tokio-tungstenite` with `native-tls`: Pulls in system `OpenSSL` instead of `rustls`.

#### What We Keep Active by Default in `utcp-core` (100% Upstream Code, Zero Rewriting)

By vendoring upstream `rs-utcp` (`MIT OR Apache-2.0`) into `utcp-core`, switching TLS to `rustls`
(`rustls-tls-native-roots`), and gating the 3 heavy subsystems behind optional Cargo features, we keep **all** of
upstream's core engine and outbound transports active out of the box:

- **Core Spec & OpenAPI Ingest (`src/spec.rs`, `src/call_templates.rs`, `src/openapi/`, `src/auth/`, `src/security.rs`):**
  Full UTCP 1.0 manual parsing, OpenAPI 2.0/3.x conversion, authentication schemes, and SSRF validation.
- **Default Pure-Rust Outbound Transports (`src/transports/`):**
  - `http` & `http_stream` (`reqwest` with `rustls`): REST APIs, OpenAPI endpoints, local HTTP services.
  - `websocket` (`tokio-tungstenite` with `rustls`): Bidirectional WebSocket services (e.g., an agent container's
    `ws://.../ws` endpoint or a streaming voice server).
  - `sse` (`eventsource-stream`): Server-Sent Events streaming endpoints.
  - `graphql`: GraphQL queries, mutations, and subscriptions.
  - `cli` (`tokio::process::Command`): Structured JSON wrapper around local CLIs (e.g., a container management script).
  - `text`: Local script execution transport (note: cached `ManualV1` `.utcp.json` files resolve tools directly to
    their underlying `http`/`websocket`/`cli` transports rather than routing through `TextTransport`).
  - `tcp` & `udp`: Raw socket transports.
  - `mcp` (Outbound Client Only, Default-Compiled): Pure-Rust JSON-RPC 2.0 client (`stdio` & `http`) requiring zero
    extra crates and running zero background activity unless a third-party MCP-only server is explicitly imported via
    `utcp import-mcp`. Allows IDEs, CLIs, and headless gateways to stay 100% free of MCP configurations even when a
    vendor only ships an MCP binary or URL.
    - One stdio server process per manual is shared by all callers. A single reader task routes each answer to the
      caller waiting on its JSON-RPC `id`, so concurrent calls never wait in line or receive another call's answer,
      and answers may arrive in any order. Each request times out after 60 s; a server that exits fails its waiting
      calls at once and is restarted on the next call (issue 014). HTTP MCP requests use unique ids too.
    - A stdio server is stopped with its whole process tree, because launchers such as `npx` and `uvx` start the real
      server as their own child: on Unix it runs in its own process group, which gets `SIGTERM`, then `SIGKILL` for
      whatever is still running 2 s later (the `utcp` binary waits for this before exiting); on Windows it runs in a
      Job Object that kills the tree when closed. A `docker run -i` / `podman run -i` container is outside the tree:
      it stops when its stdin closes or, on Unix, when the client forwards the `SIGTERM`; a container that ignores
      both keeps running (`--init` helps). This happens after `import-mcp` discovery, when `deregister` reloads the
      daemon, and on `daemon stop`. A server whose command, arguments or resolved environment changed
      (re-registration, rotated token) is replaced on its next call (issue 015).
    - **Windows process details:** every process utcp starts for a tool (stdio MCP server, CLI tool, text script) is
      created with `CREATE_NO_WINDOW`, so the console-less daemon never opens a console window per server or call.
      `utcp daemon start` spawns the daemon with `CreateProcessW` directly: no inherited handles at all (`Command`
      would pass on every inheritable handle, so a caller capturing the output, such as an SSH session or an agent's
      shell tool, would wait until the daemon exits), null standard handles, `CREATE_NO_WINDOW | DETACHED_PROCESS`,
      and `CREATE_BREAKAWAY_FROM_JOB`, so the daemon outlives a host that kills its job when the session ends
      (Windows OpenSSH). A job that forbids breakaway refuses it; the daemon then starts inside the job.
    - **Program lookup on Windows:** a bare program name (a stdio MCP server's `command`, a `cli` tool's program, the
      `[secrets]` command) is looked up on `PATH` with the extensions in `PATHEXT` that Windows starts without a file
      association (`.com`, `.exe`, `.bat`, `.cmd`), first folder first, `PATHEXT` order within a folder, so `npx`
      finds `npx.cmd` and `az` finds `az.cmd`. Rust's `Command` alone only finds `.exe` files. When the manual sets
      `PATH` in its environment, that `PATH` is searched. A name with a folder or one of those extensions, or one
      that is not found, is passed on unchanged (issue 020).
    - **`cli` tool command line:** a `cli` call template's `command_name` is split into the program and its arguments.
      `"..."` or `'...'` groups words (the quotes are removed), `""` gives an empty argument, backslashes are literal
      (so Windows paths work), and an unclosed quote is an error. There are no variables, globs or pipes.
    - **`udp` tools:** on Windows, a call to a port nobody listens on fails at once with "No UDP server is listening
      at …", since Windows reports the ICMP port-unreachable as a connection reset. Linux waits for the timeout.
    - **Interrupted direct mode:** Ctrl+C, or `SIGTERM` / `SIGHUP` on Unix, during a direct-mode `call`, `batch`,
      `register`, `import`, `import-mcp` or `refresh` stops the work, and with it any stdio server it started (whole
      tree, as above). `utcp` waits for that stop and exits with `128 + signal` (130, 143, 129). This never touches
      servers the daemon runs.
    - **Server management** (`utcp daemon servers | stop-server <manual> | restart-server <manual>`): the daemon
      lists each registered manual's stdio servers with state (`not started`, `starting`, `running`, `exited`,
      `stopped`), PID, uptime and restart count (every start after the first: crash recovery, changed settings,
      `restart-server`), and the program name only, never arguments or environment (they may hold secrets).
      `stop-server` stops the server now; calls waiting on it fail at once with `was stopped on request`, and the
      next call starts it again (use `deregister` to keep it off). `restart-server` stops it and starts it again at
      once, so a failing start is reported to the caller; it also starts a stopped or not-yet-started server. Stop
      and restart are recorded in the in-memory call list (`utcp log recent`). Listing never waits for a server that
      is starting (an `npx` download can take a minute); it reports `starting` instead.
- **Vendored Hygiene & Modernized Crate Stack:**
  - Strip library-level `println!`/`eprintln!` calls from vendored `rs-utcp` so `stdout` stays strictly reserved for raw
    command/JSON output and `localhost` calls never spam `stderr`.
  - Align dependencies on `reqwest 0.12` (`rustls-tls-native-roots`), `tokio-tungstenite 0.24`
    (`rustls-tls-native-roots`), `base64 0.22`, and `serde_norway` (a maintained `serde_yaml` fork; replaces both the
    deprecated `serde_yaml 0.9` and `serde_yml`, which RUSTSEC-2025-0068 flags as unsound) to avoid duplicate HTTP/TLS
    stacks in `Cargo.lock`.
- **Optional Feature-Gated Subsystems (Available Anytime via Cargo Features):**
  - `--features grpc` (`src/transports/grpc/`, `tonic`, `prost`)
  - `--features webrtc` (`src/transports/webrtc/`, `webrtc`)
  - `--features codemode` (`src/plugins/codemode/`, `rhai` with `smartstring/serde` patched)

---

## 3. Cross-Platform Directory, Socket & State Layout (Linux & Windows)

### Path Resolution Rule

> **Refinement vs. Initial Draft:**
> Calling `directories::ProjectDirs::from("io", "utcp", "utcp")` on Windows produces redundant nested directories
> (`%APPDATA%\utcp\utcp\config`) because both `organization` and `application` are set to `"utcp"`.
>
> Instead, `utcp-core` uses `directories::ProjectDirs::from("", "", "utcp")` (with `UTCP_CONFIG_DIR` / `UTCP_CACHE_DIR`
> / `UTCP_STATE_DIR` env overrides for testing/containers, plus `UTCP_SOCKET_PATH` for the daemon endpoint and
> `UTCP_MEDIA_DIR` for the media spool), producing clean single-level paths on both Linux and Windows:

| Artifact | Purpose | Linux Path (XDG) | Windows Path (Known Folders) |
| :--- | :--- | :--- | :--- |
| **Binary (`PATH`)** | Single CLI & daemon executable | `/usr/bin/utcp` or `~/.local/bin/utcp` | `%LOCALAPPDATA%\Programs\utcp\utcp.exe` |
| **Daemon IPC Socket** | Local IPC endpoint for Daemon Mode | `$XDG_RUNTIME_DIR/utcp.sock` (fallback `~/.cache/utcp/utcp.sock`; a separate setup: `utcp.sock` in its cache folder) | `\\.\pipe\utcp-<USERNAME>` (a separate setup: `utcp-<USERNAME>-<hash of its cache folder>`) |
| **`utcp.sock.lock`** | Held by the running daemon, so only one can bind the socket (empty, `0600`) | Next to the socket | None: the pipe is created first-instance-only |
| **`settings.toml`** | The user's settings (`cache_on_register`, `daemon_idle_timeout_mins`, `max_upload_mb`, `max_download_mb`, `download_dir`, media spool) and the optional `[secrets]` command (`command`, `timeout_secs`). Created from a commented template on first use; each run adds settings a newer utcp knows (never changing existing lines; old file kept as `settings.toml.bak`). One list in `settings_file.rs` drives the template, the sync and `doctor`'s unknown-setting check. An invalid file is left alone, reported by the daemon and `doctor`, and the defaults apply | `~/.config/utcp/settings.toml` | `%APPDATA%\utcp\config\settings.toml` |
| **`.utcp_config.json`** | Shared UTCP manual registry (machine-wide per user) | `~/.config/utcp/.utcp_config.json` | `%APPDATA%\utcp\config\.utcp_config.json` |
| **`.env`** | Namespaced API secrets (`chmod 600` / user ACL) | `~/.config/utcp/.env` | `%APPDATA%\utcp\config\.env` |
| **`manuals/*.utcp.json`** | Converted & cached UTCP manuals on disk | `~/.cache/utcp/manuals/` | `%LOCALAPPDATA%\utcp\cache\manuals\` |
| **`manuals/staged/`, `manuals/previous/`** | Staged manuals (`utcp stage`), and the version each `promote` replaced (`utcp rollback`) | `~/.cache/utcp/manuals/staged/`, `.../previous/` | `%LOCALAPPDATA%\utcp\cache\manuals\staged\`, `...\previous\` |
| **`search_index.bin`** | Pre-computed tool metadata & schema index | `~/.cache/utcp/search_index.bin` | `%LOCALAPPDATA%\utcp\cache\search_index.bin` |
| **`logging.enabled`** | Sentinel file present only when disk logging is on | `~/.local/state/utcp/logging.enabled` | `%LOCALAPPDATA%\utcp\data\logging.enabled` |
| **`activity.jsonl`** | Opt-in call audit log (with `.bak` rotation) | `~/.local/state/utcp/activity.jsonl` | `%LOCALAPPDATA%\utcp\data\activity.jsonl` |
| **`daemon.log`** | Daemon exit errors, panics, startup warnings and secrets command failures, always on (256 KiB, `.bak` rotation) | `~/.local/state/utcp/daemon.log` | `%LOCALAPPDATA%\utcp\data\daemon.log` |
| **`daemon.pid`** | PID of the running daemon (removed on exit if still its own) | `~/.local/state/utcp/daemon.pid` | `%LOCALAPPDATA%\utcp\data\daemon.pid` |
| **`utcp-media/`** | Bounded, self-cleaning spool for intercepted media (must be a real folder owned by the user; mode `0700` enforced on Unix, else media stays inline) | `$XDG_RUNTIME_DIR/utcp-media/` (`0700` RAM `tmpfs`; without `XDG_RUNTIME_DIR`, `utcp-media-$USER` in the temp folder) | `%TEMP%\utcp-media\` |

### Shared User Catalog & Explicit Override Rule

1. By default, every `utcp` invocation, regardless of what directory Zed, VS Code, Antigravity, or Hermes was launched
   from, uses the single shared user catalog (`~/.config/utcp/.utcp_config.json` and `~/.cache/utcp/`). No implicit
   `cwd` `.utcp_config.json` lookup is performed, avoiding directory-dependent surprises when agents start in `~`.
2. Isolation for tests, containers, or custom registries is always explicit via `--config <path>` or `UTCP_CONFIG` /
   `UTCP_CONFIG_DIR` / `UTCP_CACHE_DIR` / `UTCP_STATE_DIR` environment variables. The platform folders are looked up
   only for a directory without an override: on Windows that lookup goes through the Known Folders API, which fails
   when `%USERPROFILE%` points at a missing folder, so setting all three variables avoids it. Such a separate setup
   (`--config`, `UTCP_CONFIG`, `UTCP_CONFIG_DIR` or `UTCP_CACHE_DIR`) also gets its own daemon endpoint unless
   `UTCP_SOCKET_PATH` is set, so it never reaches the default setup's daemon; `utcp daemon start`, `stop` and
   `restart` never delegate to the systemd unit for it. A separate config (`--config`, `UTCP_CONFIG` or
   `UTCP_CONFIG_DIR`) without `UTCP_CACHE_DIR` gets its own cache folder, `setups/<hash>` in the default one (12 hex
   digits of the SHA-256 of the config file's absolute path), so it never shares cached manuals or `search_index.bin`
   with the default setup. An empty variable counts as unset. A separate setup never rewrites skills folders
   another registry wrote (each folder's `utcp-tools/.utcp-registry` names the `.utcp_config.json` that wrote it;
   folders without that file belong to the default setup; `sync-ides` always writes and takes a folder over).
3. The daemon watches its config folder (`.utcp_config.json`, `settings.toml`, `.env`) and `manuals/*.utcp.json`
   (`mtime` / `notify`) so registering manuals or updating API keys takes effect immediately without restarting
   `utcp daemon`.

---

## 4. Dual-Mode Execution: Daemon Mode + Standalone Direct Mode

Only **one** `utcp` binary is installed on a host. Multiple agents (10, 50, or 100 concurrent processes across Zed, VS
Code, Antigravity, and Hermes) can invoke `utcp` simultaneously.

### 1. How the CLI Chooses Between Daemon Mode and Direct Mode

On every hot-path invocation (`utcp list`, `utcp search`, `utcp schema`, `utcp call`, `utcp batch`), unless `--direct`
is passed:

1. **Non-Blocking IPC Probe (< 0.05 ms):**
   - `utcp` attempts to connect to the local IPC endpoint (`$XDG_RUNTIME_DIR/utcp.sock` on Linux,
     `\\.\pipe\utcp-<USERNAME>` on Windows).
2. **If Daemon is Running (Daemon Fast Path: < 0.2 ms):**
   - The OS kernel creates a dedicated private bi-directional stream (Unix socket mode `0600` on Linux; multi-instance
     Named Pipe created ahead of accept on Windows) between that specific `utcp` CLI process and the daemon. A Windows
     client that still meets `ERROR_PIPE_BUSY` (all instances momentarily taken) retries for up to 2 seconds instead of
     falling back to Direct Mode.
   - Pipe names are machine-wide on Windows, so the CLI first checks that the pipe's owner is the current account (the
     token's user, or its default owner such as Administrators in an elevated shell). A pipe created by another account
     is refused with an error before anything is sent; the daemon itself creates the pipe with `first_pipe_instance`, so
     it never serves on a pipe someone else made.
   - One daemon per endpoint, even when several start at the same instant. On Linux the daemon holds an exclusive
     `flock` on `<socket>.lock` for its whole life (the OS drops it when the process ends, crash included), and only
     then replaces a stale socket file; on exit it deletes the socket and `daemon.pid` only if they are still its own,
     then releases the lock. A lock held by a daemon that is shutting down is retried for up to 2 seconds, so
     `daemon restart` never fails on it. On Windows the first-instance pipe is the lock. Every loser exits with
     "utcp daemon is already running at …".
   - A daemon endpoint this process may not open is an error too, never a quiet fall back to Direct Mode, a STOPPED
     status, or a second daemon: "Cannot use the utcp daemon at …: access denied" on Windows (a daemon started from
     an elevated shell admits only elevated callers, by the pipe's default permissions) and "…: permission denied" on
     Unix (another account's socket, for example root's through `sudo`). The permissions are left as they are; opening
     an elevated daemon to normal callers would let them run code as administrator. `--direct` still works.
   - The CLI sends one frame over its private stream: a `u32` little-endian length prefix, then JSON
     `{"caller", "force_log", "command": {"type": ..., ...}}`. The answer is framed the same way:
     `{"status": "ok", "data": {"kind": ..., "value": ...}}` or `{"status": "error", "message": ...}`.
   - **Files on a `call` or `batch` connection (`ipc_files.rs`).** Before the answer, both sides may exchange file
     frames. A length with its top bit set marks a data frame: a `u32` stream id, then bytes; an empty one ends that
     stream. The daemon asks for an upload with `{"frame": "send_file", "id", "path", "offset", "length"}`; the CLI
     opens the file itself (checking `max_upload_mb`) and answers `upload_ready {size}` and the bytes, or
     `upload_failed {message}`. A file answer, or a JSON answer over 10 MB, comes as `{"frame": "file", "id", "item",
     "meta"}` and bytes; the CLI writes a `.part` file (checking `max_download_mb`; `cancel {id, message}` stops the
     daemon) and replaces `{"utcp_file_stream": id}` in the answer with the saved file's reference. The CLI stays
     single-threaded and handles one frame at a time; the daemon reads and writes in separate tasks and always
     drains what the CLI sends. So the daemon never opens a user's file, and an agent's sandbox stays the boundary.
   - The daemon spawns an isolated Tokio task for that connection, executes the request against its in-memory index and
     warm outbound connection pool, and writes the response back down that exact same private stream.
   - Because each caller has its own OS-managed socket file descriptor, 50 concurrent agents from different Zed/VS
     Code/Antigravity windows never collide or mix responses.
3. **If Daemon is NOT Running (Direct Standalone Fallback: < 2 ms):**
   - If the socket does not exist or refuses connection, `utcp` immediately executes the exact same `utcp-core` operation
     in-process against the disk cache (`search_index.bin` and `manuals/<name>.utcp.json`).

### 2. What the Optional `--user` Daemon (`utcp daemon`) Holds in RAM

`utcp daemon` runs as a per-user service (`systemctl --user enable --now utcp-daemon` on Gentoo Linux; normal user
process on Windows) so its `0600` socket, credentials, and cache belong strictly to the logged-in user. When running,
the single machine-wide user daemon maintains:

- **In-Memory Search Index & Parsed Manuals:** Pre-parsed in the shared `Arc<DaemonServerState>` (catalog behind a
  `RwLock`), automatically reloaded via
  `notify` whenever `.utcp_config.json` or `manuals/*.utcp.json` changes on disk. Zero file reads or JSON parsing during
  `list`, `search`, or `call`.
- **User Environment & Live `reload-env` Synchronization:**
  - **Allow-list, not a copy of the environment:** the daemon keeps in RAM only the variables registered manuals can
    read: every `${VAR}` placeholder in a tool's provider or call template and every `required_env_vars` entry,
    expanded with the doubled-underscore lookup names (`resolve_secret_var`). Every other variable (`PATH`, `HOME`,
    unrelated tokens in a caller's shell) is dropped on receipt and never stored. The allow-list is recomputed when
    manuals change. `env_var_count` in `utcp daemon status` counts only these variables. Tools the daemon starts (CLI
    and MCP stdio) still inherit the daemon's own process environment, so `PATH` keeps working for them.
  - **The CLI sends only allowed names too:** `search_index.bin` stores each manual's allow-list (`env_var_names`),
    and `utcp call`, `utcp batch`, and `utcp daemon reload-env` send the daemon only the caller variables on it. If the
    index cannot be read, the CLI sends none rather than its whole environment. The daemon still filters on receipt.
    An index from an older layout (different leading format tag) is rebuilt automatically.
  - At startup, reads process environment variables, `~/.config/environment.d/*.conf`, `systemctl --user
    show-environment` (on Linux), and `~/.config/utcp/.env` (re-read on `.env` `mtime` changes), keeping only allowed
    names.
  - **Secrets Command (`[secrets]` in `settings.toml`, `secrets.rs`):** a program the user names to print the
    secrets, so they can stay in a shell file, `pass`, a keyring or a password manager. A daemon started by
    `systemd --user` never reads `~/.bashrc`, so without it, secrets exported there reach the daemon only through
    a caller. The Hermes gateway's `secrets.command` follows the same model.
    - `command` is a list (program, then arguments; no shell, a leading `~` is the home folder). utcp appends every
      allowed name as a further argument and reads `NAME=value` lines from stdout: values literal to the end of the
      line (no `$` expansion, so no `dotenvy`), a leading `export` and one pair of matching quotes removed, blank
      lines and `#` comments skipped. Only allowed, non-empty names are kept; a line that is not `NAME=value` fails
      the run.
    - It runs with stdin closed and stderr discarded, under `timeout_secs` (default 10), with stdout capped at
      1 MiB. Errors name the problem (could not start, exit code, timeout, line number), never output.
    - The daemon runs it at startup (before it listens; `utcp daemon start` waits 3 s plus `timeout_secs`), on every
      reload (`notify` on the config folder, which covers `register` and `settings.toml` edits) and on
      `reload-env`, outside the catalog lock. A failed run keeps the values of the last good one. A new failure and
      the recovery are logged to stderr and `daemon.log`, not every repeat.
    - **Precedence:** secrets command > pushed or absorbed caller variables > startup environment and `.env`.
    - `daemon status --json` and `doctor` show its last run (`secrets_command`: command, error, provided names).
  - **Startup warnings:** after loading, the daemon writes to stderr (the journal) and `daemon.log` an invalid
    `settings.toml`, a failed secrets command, and every required variable still unset, with its manual.
  - **Cold-Boot Shell Variable Auto-Absorption:** On `utcp call` and `utcp batch`, non-empty variables a registered
    manual needs that are present in the caller's shell but were absent when `systemd --user` started the daemon at
    boot (for example, variables exported in `~/.bashrc`) are absorbed over the `0600` IPC socket, preventing
    cold-boot `401` errors. All other caller variables are ignored.
  - Supports **`utcp daemon reload-env`** (and hotkey **`E`** in `utcp ui`) to explicitly rotate/overwrite already
    loaded manual variables in daemon RAM without restarting the daemon. It also rereads the daemon's own environment
    and `.env` and runs the secrets command again.
- **Shared Warm Outbound Connection Pools:**
  - Persistent `reqwest::Client` (`rustls`) pool keeping HTTP/1.1 keep-alive and HTTP/2 multiplexed TLS connections warm
    across sequential `utcp call` invocations (eliminating repeated 50–150 ms TCP+TLS handshakes).
  - Reusable WebSocket & outbound MCP stdio handles across turns.
- **Bounded `tmpfs` Media Spool & Sweeper (`utcp-media/`):**
  - Intercepts inline base64 `"type": "image"` / `"type": "audio"` blocks returned by outbound MCP servers, decodes them
    into `$XDG_RUNTIME_DIR/utcp-media/` (`0700` RAM `tmpfs` on Linux; `%TEMP%\utcp-media\` on Windows,
    overridable via `settings.toml` `media_dir` or `UTCP_MEDIA_DIR`), and replaces the base64 blob with a compact JSON
    reference (`path`, `mime_type`, `bytes`, and a multimodal read `hint`) so responses never flood the LLM context
    window.
  - Enforces automatic cleanup (`max_media_files = 64`, `media_ttl_mins = 60`) on every write and periodic daemon sweep
    so `utcp` never litters the filesystem.
- **In-Memory Activity Ring Buffer + Single-Writer Log Flush:**
  - Keeps a bounded in-memory ring (`VecDeque` behind a `RwLock`) of the last 500 calls (failures keep their redacted
    error text) for instant
    inspection in `utcp ui` Tab 5 and `utcp log recent`, with disk logging off. When disk logging is enabled, it
    serializes writes to `activity.jsonl` through a single background writer task (zero multi-process disk
    contention).
- **Optional Idle Auto-Exit:** Supports `--idle-timeout <minutes>` (default: `0` / stay resident until stopped) so it
  can either run persistently or shut itself down when idle.

### 3. Standalone Direct Mode Optimizations (When Daemon is Off)

Even when running without the daemon, Direct Mode remains sub-2ms on Linux:

- **Zero-Tokio Synchronous Execution for Local Commands (`list`, `search`, `schema`):** Reads `search_index.bin`
  directly from the OS RAM page cache (~15 microseconds) without initializing Tokio or TLS.
- **Direct Single-Manual Lookup on `utcp call <manual>.<tool>`:** Reads the small registry, then opens only
  `~/.cache/utcp/manuals/<manual>.utcp.json` (1 file read from OS page cache), and loads `.env` only when `${...}`
  secret interpolation is required. The secrets command, if configured, is then asked for that manual's names only.
  As through the daemon, a manual the registry does not list as active is refused (`is staged, not active yet`, or
  `is not registered`), even when a deregistered manual's saved copy is still in the cache.
- **Secrets Command Without the Daemon:** other commands that resolve secrets (`batch`, `register` / `import-mcp`
  placeholders, `doctor`) ask it for every name registered manuals use, plus the placeholders of a manual being
  registered. A process reuses a run for the same names for 60 s. Its values win over the environment and `.env`.
- **Lock-Free Reads & Atomic Writes:** Cold-path writes (`register`, `refresh`, `deregister`) acquire an advisory file
  lock (`~/.config/utcp/.utcp.lock`) and write to `.<file>.tmp.<pid>.<nanos>` followed by atomic `std::fs::rename()`,
  ensuring concurrent readers never see half-written files.

### 4. Strictly Opt-In Activity Logging & 1-Backup Rotation (`activity.jsonl`)

- **Off by Default (Zero Bottleneck):** Disk activity logging is **disabled by default**. Opening `utcp ui` does **not**
  automatically enable disk logging; the user must explicitly opt in via `utcp log on` (or the toggle inside `utcp ui`),
  set `UTCP_LOG=1` (or `true`) in the environment, or pass `--log` for a single command.
- **Sub-Microsecond Sentinel Check:** When disk logging is enabled, `utcp` creates `~/.local/state/utcp/logging.enabled`.
  In Direct Mode, `utcp` checks `Path::exists()` (< 1 microsecond kernel dentry `stat()`). When absent, zero disk
  logging runs.
- **1-Backup Size Cap Rotation (`activity.jsonl` + `activity.jsonl.bak`):**
  - Before writing, if `activity.jsonl` exceeds **1 MB** (~5,000 entries):
    1. Delete existing `activity.jsonl.bak` (if present).
    2. Atomically rename `activity.jsonl` $\rightarrow$ `activity.jsonl.bak`.
    3. Start a fresh `activity.jsonl`.
  - Preserves up to ~10,000 entries across the active and `.bak` files with $O(1)$ zero-copy rollover.
- **Error Details (`detail`):** A failed `call` / `dry-run` records the full error text in an optional `detail`
  field; a `batch` with failed items records one `<id> <tool>: <error>` block per failed item and the status
  `k/N OK`. Successful entries omit the field, and older log lines without it still load.
  - Before storing, values of env vars whose names contain `TOKEN`, `KEY`, `SECRET`, `PASSWORD`, `PASSWD`, `AUTH`,
    `CREDENTIAL`, `COOKIE`, or `_PAT` (values of 8+ characters) are replaced with `***REDACTED:<NAME>***`.
  - The text is capped at 4,000 characters (`MAX_DETAIL_CHARS`). The daemon ring buffer and `activity.jsonl` both
    keep it.
- **Zero-Cost Caller Identification (`caller.rs`), first match wins:**
  1. `UTCP_CALLER` (explicit override; the TUI's Try popup records `tui`).
  2. Agent markers: `ANTIGRAVITY_CLI` / `GEMINI_CLI` -> `agy`; `HERMES_SESSION_ID` / `HERMES_AGENT` -> `hermes`.
  3. Agent ancestry: up to 6 parent PIDs via `/proc/<pid>/stat` (and `/proc/<pid>/cmdline` for `node` / `python`
     wrappers) looking for `agy` / `antigravity` / `hermes`. This runs before the terminal checks, so `agy` started
     inside a Zed terminal tab is still reported as `agy`.
  4. Terminal hosts: `ZED_TERM` or `TERM_PROGRAM=zed` -> `zed`; `TERM_PROGRAM=vscode` or `VSCODE_PID` -> `vscode`.
  5. The parent process name (`/proc/<ppid>/comm` on Linux; on Windows a Toolhelp process snapshot, searched for the
     parent PID), else `terminal`. Names are matched exactly (`zed`, `zeditor`, `zed-editor`, `zed-preview` ->
     `zed`; `code`, `code-insiders`, `code-oss`, `codium`, `vscodium` -> `vscode`); `agy` or names containing
     `antigravity` -> `agy`, names containing `hermes` -> `hermes`.
  - Never calls full process-tree enumeration (`sysinfo::System::new_all()`).
- **Daemon Failure Log (`daemon.log`, separate and always on):** the daemon process also writes what it would print
  to stderr to `daemon.log` in the state folder (`daemon_log.rs`): the error `utcp daemon run` exits with, every
  panic (a panic hook, installed only in the daemon, records the message, location and thread name; a panic inside one
  task, which the daemon survives, is recorded too), its startup warnings, and secrets command failures and
  recoveries. Its stderr is discarded when it runs detached or from the Windows logon task, and only systemd keeps it
  otherwise. Nothing is written per call, so it is on even with activity logging off. Past **256 KiB** it rotates to
  one `daemon.log.bak`, like `activity.jsonl`.
  - `utcp daemon start` notes the file's size before spawning, stops waiting as soon as the detached daemon exits (or
    after 3 s, plus the secrets command's `timeout_secs` when one is set), and puts whatever was appended since into
    its error ("utcp daemon exited before listening ... It logged ...").
  - `utcp daemon status` of a stopped daemon prints the file's last write time and path (`daemon_log` in `--json`).

### 5. Single-Process Batch Execution (`utcp batch`)

- `utcp batch [--stdin | --file <path>] [--concurrency 8]` accepts a JSON array of calls (without `--file`, stdin is
  read when `--stdin` is given or stdin is not a terminal, so `utcp batch < items.json` works):

```json
[
  {
    "id": "1",
    "tool": "github.get_issue",
    "args": { "owner": "rust-lang", "repo": "rust", "issue_number": 1 }
  },
  {
    "id": "2",
    "tool": "myagent.status",
    "args": {}
  }
]
```

- Executes concurrently (via the daemon if running, or a shared in-process Tokio runtime and connection pool in Direct
  Mode), returning a structured JSON array of results (`id`, `tool`, `ok`, `status`, `result` or `error`, `next_page`
  when `result` is a page of a longer list, `note` when the arguments had undeclared keys, `--where` kept none of a
  list's items or a `--fields` path matched nothing, `latency_ms` rounded to 0.01) so partial failures never abort
  the entire batch.
- An item has `tool` and optionally `id`, `args` (alias `arguments`), `fields`, `where` and `out`. `id` defaults to
  the item's position (`"0"`, `"1"`, ...). Any other key is refused before anything is called (so a misspelled
  `feilds` can't be ignored quietly), and so are two items with the same id.
- Every item's answer or error is printed; when any item failed, `utcp batch` then exits with status 2 (and
  `utcp: k of n batch items failed (ids: ...)` on stderr). A batch that could not run at all exits with 1.

---

## 5. Manual Ingestion, OpenAPI Conversion & Caching (`utcp-core`)

`utcp-core` wraps upstream `rs-utcp`'s spec and OpenAPI ingestion behind a clean loader supporting:

1. **Native UTCP Manual JSON** (`.utcp.json` or `/utcp` discovery endpoint, supporting `http`, `websocket`, `cli`,
   `sse`, `graphql`, `tcp`, `udp`, and `text` call templates).
2. **OpenAPI 3.0 / 3.1** (JSON and YAML).
3. **OpenAPI 2.0 / Swagger** (JSON and YAML).

### OpenAPI-to-UTCP Normalization Rules

- **Tool Naming:** Uses `operationId`, with any character other than letters, digits, `_` and `-` replaced by `_`
  (GitHub's `actions/create-workflow-dispatch` with tag `actions` arrives as `actions_create-workflow-dispatch`; names
  are not converted to `snake_case`). If `operationId` is absent, synthesizes `<method>_<sanitized_path>` (e.g.,
  `get_users_by_id`).
- **Parameter Mapping:** Distinguishes `path` parameters (interpolated into URL template `/users/{id}`), `query`
  parameters, `header` parameters, and JSON `body` properties so `utcp call` routes each argument to its proper HTTP
  location automatically.
- **Input Schemas:** Parameter and body schemas are expanded in full (`$ref`s inlined), since agents need them to build
  calls. A schema met again inside its own expansion (a tree node holding its children) is replaced by a short
  "Recursive: the same schema as ... above" note instead of recursing. `type: object` is added only when the schema has
  no `type`, `oneOf`, `anyOf`, `allOf`, `enum`, `const` or `not`.
- **Output Schemas:** The `200` (or `201`) JSON response is outlined one level deep: its `type`, `description`,
  `required` and fields, each field with only its `type`, `format` and item type (an `anyOf` / `oneOf` field lists its
  variants' types). Nested fields are not expanded, so one tool's output schema stays a few KB even on large APIs
  (GitHub: 482 KB -> 5.5 KB at most, [issue 022](../issues/022-openapi-import-output-schemas-oneof-and-recursive-refs.md)).
- **Auth & Secret Namespacing:**
  - Follows UTCP's doubled-underscore rule: for manual `my_api` requiring `API_KEY`, the primary `.env` variable is
    `my__api_API_KEY` (plus supporting `<MANUAL_UPPER>_API_KEY` as a convenient fallback).
  - Extracts OpenAPI `securitySchemes` (`bearer`, `apiKey` in header/query, `basic`, `oauth2`) into the UTCP manual's
    auth template automatically.

### SSRF Protection & Local / LAN Service Support

- **Default Policy:**
  - Always block cloud metadata endpoints, by the host as written in the URL: `169.254.0.0/16` (including
    `169.254.169.254` and `169.254.170.2`), `fd00:ec2::254`, `metadata.google.internal`, `metadata.goog` and
    `100.100.100.200`. A DNS name that resolves to one of these addresses is not caught (only relevant on cloud VMs).
  - Block HTTP redirects to those endpoints, and redirects from a public host to a private or local one (loopback,
    RFC 1918, link-local, `100.64.0.0/10`, IPv6 `fc00::/7`, `localhost`, `*.local`, or a single-label name), with
    at most 10 redirects.
  - Allow direct user-configured `localhost` / `127.0.0.1` / LAN (`192.168.x.x`, `10.x.x.x`, Tailscale/WireGuard, or
    single-label hostnames like `nas` and `media-server`) so local services, such as a voice server on another LAN
    machine or an agent container on this one, work without extra configuration.

### Rate Limits: Clear Errors So Agents Back Off

All agents on a machine share one daemon and usually one token, so a rate limit hit by one agent is hit by all. A bare
`HTTP 403` makes agents retry at once; instead every HTTP-based path (`http`, `http_stream`, `graphql`, `sse`, MCP over
HTTP, and spec downloads) turns a rate limit into one clear error (`transports::rate_limit`):

```text
rate limited by api.github.com (HTTP 403): retry after 1233 s (about 21 min, at 2026-11-18 03:32:32 UTC); limit 5000
requests per window. Every agent using this token shares the limit, so retrying sooner fails too. Server said: ...
```

- **Detected:** any `429`, and a `403` with `x-ratelimit-remaining: 0` or `Retry-After` (GitHub's primary and
  secondary limits). Other `403`s stay ordinary errors.
- **Wait time:** `Retry-After` (seconds or HTTP date), else `x-ratelimit-reset` / `ratelimit-reset` (Unix time or
  seconds). Without either, the error says the server gave no time.
- **Debugging without disk logging:** the error is returned to the calling agent and kept in the daemon's in-memory
  ring, so `utcp log recent --failed` (or `utcp ui` Tab 5) shows it afterwards.

### Checks Before Sending: Errors About the Call, Not the API

A call the API misreads gets an answer that points the wrong way: a missing argument leaves `{repo}` in the URL and
the API says 404 (the agent concludes the repository doesn't exist), a misspelled filter becomes a query parameter
the API ignores (the call succeeds with the wrong data), and an unset secret goes out empty (401). So `call` and
each `batch` item check before sending, through the daemon and in direct mode, and an error says
`Nothing was sent`:

- **Secrets:** every `${VAR}` placeholder the manual's call template uses must have a value (secrets command,
  environment, `.env`): `these placeholders were not set: VAR (manual m needs it). Nothing was sent. ...` with
  where to set it and `utcp doctor`. `--dry-run` shows a set secret as `***REDACTED:VAR***` and an unset one as
  `***UNSET:VAR***`.
- **Arguments** (`arguments.rs`, against the tool's input schema; every problem is listed together, then
  `utcp schema <manual.tool>`):
  - Every `required` input without a `default`, and every `{name}` left in an HTTP URL template, must be passed,
    except a URL placeholder the schema declares and does not require: left out, it is passed as `""`. A required
    placeholder may not be empty (`'x' is empty, but it is part of the URL and required`), since that would call
    another URL.
  - Filling the URL (`fill_url`): each value is URL-encoded, and an empty (or `null`) value leaves its placeholder's
    whole path segment out, as Azure DevOps routes with optional segments expect (`{organization}/{project}/_apis/...`
    without a project; `/_apis/build/builds/{buildId}/timeline/{timelineId}` without a timeline).
  - Names must be declared inputs, URL placeholders, header or body fields, or the paging names utcp itself puts
    in `next_page` (`continuationToken`, `x-ms-continuationtoken`, `page`, `after`, `before`, `cursor`), unless the
    schema has no `properties` (its inputs are unknown) or allows `additionalProperties`. `"properties": {}`
    declares that the tool takes no arguments, and the error says so. An unknown name gets `did you mean ...?` when
    a declared one is close (optimal string alignment distance, so a swap of two letters is one edit).
  - A name that differs only in case from exactly one declared name, which the call does not also pass, is renamed
    to it (`Fields` to `fields`), at the top level and inside bodies.
  - `integer`, `number` and `boolean` inputs take those values or strings that parse as them; `string` inputs take
    any single value but not a list or object, except a list of plain values for a query input, which is joined
    with commas first (Azure DevOps and GitHub declare list filters such as `definitions` and `labels` as strings).
    A string given for an `integer`, `number` or `boolean` input (not also declared `string`) becomes that JSON
    value, so a body field gets `5`, not `"5"`.
  - Enum values match ignoring case (a comma-separated string value by value, for Azure DevOps flag enums) and are
    rewritten to the declared spelling; a number passes a string enum. The error lists up to 30 allowed values.
  - Objects whose schema declares `properties` (a JSON body, the objects inside it, and the items of lists of
    objects, up to 16 levels) are checked the same way, with paths such as `body.fields` and `body[0].op` in the
    errors. An undeclared key close to a declared key the object lacks (`feilds`, `Title`) is refused, since the API
    would ignore it. Other undeclared keys are sent: a body is often an answer read back with keys the schema
    leaves out (`_links`, `url`, read-only fields), so refusing them would break round trips. They are named in a
    note (`sent as given, but not in the tool's schema (the API may ignore them): ...`, up to 8, then `and N more`)
    that travels like a `--where` note: on stderr for `call`, in `note` for a `batch` item, in the dry-run output.
    `required` inside a body is left to the API, whose error names the field.
- **Lists in the query:** a list of plain values for a query parameter is sent once, joined with commas
  (`ids=1,2,3`), as Azure DevOps reads it.
- **The API's own errors** are printed on one line: `HTTP request failed with status <code>: <body>`, with a JSON
  body re-serialized compactly and any other body (HTML, plain text, a body cut at 64 KB) with each run of whitespace
  collapsed to one space.

### Mode A: Cache-on-Register (`utcp register <name> <url_or_path>`)

Reads `cache_on_register` (default: `true`) from `settings.toml` (overridable via `--cache` / `--no-cache`):

1. **When `cache_on_register = true`:**
   - Fetches and parses the remote or local OpenAPI / UTCP manual.
   - Normalizes into a `UtcpManual` structure with embedded `_meta`:
     `{ source_url, sha256, fetched_at, tool_count, required_env_vars }`.
   - Writes atomically (`.<file>.tmp.<pid>.<nanos>` + rename) to `manuals/<name>.utcp.json`.
   - Registers `<name>` in `.utcp_config.json` pointing to the local cached file path.
   - Atomically rebuilds `search_index.bin` (and notifies running daemon if active).
2. **When `cache_on_register = false`:**
   - Writes the remote HTTP call template into `.utcp_config.json` and still indexes the currently discovered tools into
     `search_index.bin`.

### Mode B: Proactive Ingest / Cache-Before-Register (`utcp import`)

- Command: `utcp import <url_or_path> [-o <out.utcp.json>] [--name <ns>] [--register]`
- Converts any remote URL or local OpenAPI/Swagger file (JSON or YAML) into a standalone `.utcp.json` file without
  touching `.utcp_config.json` (unless `--register` is passed). Without `--register` the file goes to `-o` (default
  `<name>.utcp.json` in the current folder); with `--register` the manual is saved in `manuals/` and `-o` also
  writes a copy there (printed as `Copy`).
- Prints a summary: manual name, output path, source, protocols, tool count, SHA-256, and the exact `.env` variable
  names required (`Secrets`).

### Manual Lifecycle: Deregister, Re-register, Purge

- `utcp deregister <name>` removes the manual from search and calls, but moves its registry entry (source URL, base
  URL, headers) into a `deregistered` map in `.utcp_config.json` and keeps `manuals/<name>.utcp.json`. The map is
  omitted from the file while empty.
- `utcp register <name>` with no source re-registers it offline from that saved copy, with its previous settings.
  `--cache`, `--no-cache`, `--base-url`, and `-H` need a source and are refused without one. A `.utcp.json` in
  `manuals/` with no registry entry at all (for example from `utcp import -o`) can be registered the same way; its
  source becomes the file path.
- `utcp deregister <name> --purge-cache` deletes it for good: the registry entry (active or deregistered) and the saved
  copy. It also cleans up a saved copy with no entry. Adding it back then needs the original source.
- A manual's **hint** (`hint` in its registry entry) survives `refresh`, `deregister`, re-registration without a
  source, and re-registration from a source without `--hint`; `--purge-cache` deletes it with the entry.

### Staging: Stage, Diff, Promote, Discard, Rollback

Like git's index, one staging area lets a manual be reviewed and tested before agents see it.

- `utcp stage <name> <source>` fetches and converts like `register` but stores the entry in a `staged` map in
  `.utcp_config.json` and the copy in `manuals/staged/<name>.utcp.json`. At most one staged copy per name; staging
  again replaces it. Without `--hint` it keeps the hint of the staged, active or deregistered manual of that name.
- Staged manuals are not in `search_index.bin`, `search` or the skills; `list` without a manual shows them in their own
  Staged section. `list`, `search`, `schema`, `call` and `batch` reach them with `--staged` (daemon or direct mode).
  `search --staged` answers the review question "will agents find the right tool?": it searches the active and
  staged manuals together, a staged manual replacing the active version of the same name, and marks staged results
  `[staged]` (`"staged": true` in `--json`). The daemon
  loads them next to the active manuals, adds their variable names to its allow-list and to the names it asks the
  secrets command for, and `doctor` lists their credentials with `"staged": true`.
- `utcp diff [<name>...]` compares each staged manual with the manual it replaces: tools added, removed, changed (any
  difference in description, schemas, tags or request) and unchanged, and the hint. Each changed tool says what
  changed: `description`; `inputs added: a, b`, `inputs removed: ...`, `now required: ...`, `no longer required: ...`,
  `inputs changed: ...` (or `inputs`); `method: GET -> POST`, `url: A -> B`, `auth`, `headers`, or the name of another
  request key; or another top-level key's name (`outputs`, `tags`). A tool keeps its request under both `provider`
  and `tool_call_template`, so each change is listed once, not once per copy (since 0.2.5).
- `utcp diff <manual>.<tool>...` shows one staged tool against the version it replaces, part by part: `description`,
  `input <name>` (that input's schema plus `"required"`), `inputs` (the rest of the input schema), `method`, `url`,
  `auth`, `headers`, another request key, or another top-level key, each with its old value after `-` and its new
  one after `+` (`(none)` for missing). When both are present and either is over 120 characters or has several
  lines, the part shows as one word diff instead (`description (word diff: [-old-]{+new+}):`): words, runs of space
  and single punctuation marks are compared, removed text is `[-...-]` (red in a terminal) and added `{+...+}`
  (green), a space between two changes joins them, and unchanged text keeps 60 characters next to each change,
  ending at a word, with `...` for the rest. In `auth` and `headers`, every string except a `${VAR}` placeholder and
  the values of `auth_type`, `location` and `var_name` prints as `***`, so a changed secret shows as a change
  without its value. A tool only in the staged copy is `added`, only in the active one `removed`. `--json` gives
  `[{"manual", "tool", "replaces", "status", "parts": [{"part", "old", "new"}]}]`. Mixing manual names and
  `<manual>.<tool>` names is an error, and so is a tool neither copy has.
- `diff`, `promote`, `discard` and `refresh` take several names; a name ending in `*` selects every manual that
  starts with the rest (`'azdo_*'`; only a trailing `*`). The selection is sorted and has no repeats, and a name or
  pattern that selects nothing is an error that lists the candidates (`No staged manual matches 'gh_*' (staged: ...)`,
  `'x' is not staged (staged: ...)`), before anything changes. One promote or discard saves the registry, rebuilds the
  index and rewrites the skills once.
- `utcp promote <name>...` (or `--all` for every staged manual) makes them active, keeps the registration each replaced
  (active or deregistered) in a `previous` map with its copy in `manuals/previous/`, rebuilds the index, reloads the
  daemon and rewrites the installed global skills. It is the only staging step that changes what agents see.
- `utcp discard <name>...` (or `--all`) drops staged manuals. Without a name or `--all`, `promote`, `discard` and
  `refresh` refuse and list the candidates, so a bare command never changes every manual by accident.
- `utcp rollback <name>` swaps the active manual with its previous version, so a second rollback undoes the first.
  `deregister --purge-cache` also deletes the staged copy and the previous version. The `staged` and `previous` maps
  are omitted from the file while empty.

### Host + Per-Container Manual Pattern (Containerized Agents)

For self-evolving containerized agents (for example a container named `myagent`, and sibling containers running on
other ports or hosts), tools naturally span two boundaries:

1. **Host-Side Habitat & Airlock Operations (`cli` transport on the host):**
   Lifecycle, snapshot, and Git airlock commands (`stage`, `harvest`, `unstage`, `snapshot`, `up`, `stop`) execute on
   the host outside the container boundary and stay uniform across all container instances.
2. **Per-Container Self-Advertised Endpoints (`GET /utcp` -> `http` + `websocket` transports):**
   Each agent container advertises its own native UTCP manual at `http://<ip>:<port>/utcp` (superior to
   `/openapi.json` because UTCP natively describes both REST `/api/*` routes and `/ws` WebSocket endpoints in one JSON
   document). Whenever an agent evolves new endpoints, `utcp refresh <name>` picks them up immediately.

`utcp-rs` supports two ways to expose both sides cleanly:

- **Pattern A: Split Namespaces (Works Out of the Box):**
  Register one parameterized host manual (`agent_host.*` accepting `instance`) alongside one live manual per
  container (`utcp register myagent http://127.0.0.1:8765/utcp`, `utcp register <next> http://127.0.0.1:8766/utcp`).
  Unified `utcp search` discovers both host airlock tools and container-specific tools together.
- **Pattern B: Single-Namespace Overlay Fusion (Planned Extension):**
  Add an optional `--overlay <host-template.utcp.json>` flag to `utcp register` / `utcp refresh` that merges the
  parameterized host-side `cli` templates (bound to that container's name and port) directly with the container's live
  `GET /utcp` tools into a single `<instance>.*` manual (`myagent.stage`, `myagent.harvest`, `myagent.get_state`, etc.).

---

## 6. Complete CLI Interface Contract (`utcp-cli`)

To avoid PowerShell vs. POSIX shell JSON-quoting failures across Windows and Linux, every execution command accepts
arguments via positional JSON, `--args-file <path>`, or `--args-stdin`. Any hot-path command also accepts `--direct` to
bypass the daemon and force standalone execution.

| Command | Description | Execution Path |
| :--- | :--- | :--- |
| Global flags: `--config <path>`, `--direct` | `--config` uses another `.utcp_config.json` instead of the shared user catalog (a separate setup with its own cache folder and daemon endpoint, section 3); `--direct` skips the daemon. | All commands |
| `utcp list [<manual>] [--tools] [--json] [--staged]` | Without a manual, one line per manual: name, hint, tool count, and whether it is active or staged (`--json`: `name`, `hint`, `tools`, `staged`); `--tools` lists every tool instead (`name`, `protocol`, `description`). With a manual, that manual's tools; with `--staged`, the staged manuals (or one staged manual's tools). Tool descriptions are cut to the first sentence, at most about 120 characters (`schema` has the full text). A manual with no match errors and says what to run instead (for example `--staged` for a staged manual). | **Hot (Daemon IPC or Sync Direct)** |
| `utcp search <query> [--limit 10] [--schemas] [--json] [--staged]` | Ranked keyword/tag search returning compact 1-line tool summaries by default (the first sentence of each description, at most about 120 characters; or full schemas with `--schemas`). Matches tool names, descriptions, tags and the manual's hint words. `--staged` searches the staged manuals with the active ones (a staged manual replaces its active version) and marks their results `[staged]`; through a daemon without the `search_staged` feature it fails and says to restart it or use `--direct`. | **Hot (Daemon IPC or Sync Direct)** |
| `utcp schema <manual.tool> [--staged]` | Prints the exact JSON input/output schema for a single tool on demand in < 1 ms. | **Hot (Daemon IPC or Sync Direct)** |
| `utcp call <manual.tool> [<json_args>] [--args-stdin] [--args-file <path>] [--fields <k1,k2>] [--where <path=v1\|v2 \| path~=v1\|v2 \| path^=v1\|v2 \| path>v \| path>=v \| path<v \| path<=v; ! before =, ~=, ^= negates>]... [--pretty \| --compact] [--dry-run] [--log] [--staged] [-o \| --out <path>]` | Executes tool; emits compact JSON automatically for agent/non-TTY callers (`--pretty` for indented human output). Optional `--fields` projects top-level or dotted keys (e.g., `sha,commit.message`); a key that itself contains dots is matched whole, the longest run of path parts that names a key winning (`fields.System.State` is the key `System.State` under `fields`; `--where` reads paths the same way); dotted paths pass through arrays (`workflow_runs.id`). Each path is resolved on its own: one whose first part names a top-level key projects that key, and the others project the fields of the items of the response's list (`items`/`results`/`data`, else its only array of objects, else on an empty page its only empty array; found before `--where`, so a list `--where` emptied stays, with its count), so `--fields name,total_count` on `{"total_count": 5, "jobs": [...]}` keeps `total_count` and each job's `name`. Optional `--where` (repeatable; every condition must hold) keeps the list items whose value at the path equals (`=`), contains (`~=`) or starts with (`^=`) one of the values separated by `\|`, before `--fields`; a `!` before the operator (`!=`, `!~=`, `!^=`) keeps the items that match none of them. Values are trimmed and compared as text, ignoring case (`null`, `true`); with `=`, a number field and a number value compare as numbers (`RemainingWork=0` matches the `0.0` Azure DevOps sends). `>`, `>=`, `<` and `<=` keep the items whose value is greater, at least, less or at most one value: a JSON number, or an ISO date with a time and zone (`2026-11-24T10:00:00Z`, `+01:00`), with a time and no zone (UTC), or alone (`2026-11-24`, which compares the field's day in UTC, so `<=2026-11-24` includes that whole day; `=` and `!=` with a date alone also mean that whole day, `completed_at=2026-11-24` matching any time on it, while a full timestamp, or a field that is itself a date alone, compares as text); numbers written as strings count, and a field that is not a number (or not a date) does not match, with a note counting those items (`left out K of the N items in <list> whose <path> is not a number (as: ...)`). A comparison with several values, a value that is neither, a `!`, `~` or `^` before it (the opposite of `>` is `<=`), and `=<5` or `=>5` (write `<=5`) are errors; `\\|` is a `\|` inside a value and a comma is literal (matrix job names); an empty value is refused for `~=` and `^=` (it would match everything), while `path=` means the empty string. A missing field, or a path through a missing or `null` parent, counts as `null`; an object at the end of the path never matches; an array on the path matches when any element does (an empty array has no value), and the matching item keeps that inner array whole. The list is the root array; else the top-level array the path's first part names (when the path has more parts); else the response's `value`, `items`, `results` or `data` array; else its only array with objects; else, when every other top-level value is plain (an empty page such as `{"total_count": 0, "workflow_runs": []}`), its only empty array. Some problems show only in the answer, after the request was sent (it may have been a write), so they never fail the call: it succeeds (exit 0) with the whole answer, unfiltered (`--fields` still applied), and the note `the call succeeded; --where was not applied, so no items were left out: <reason>`. The reasons: a path that is only the name of a top-level array, whatever the array holds (`value.value` filters the items' own `value`; for a non-empty array of plain values the reason says only that it can't be filtered); a path whose first part is another top-level key of an object answer, naming the answer's list when it has one (that field belongs to the answer, not to its items); a non-empty list with no objects (log lines); several candidate arrays, naming the arrays whose items have the path's first part, with the path to write (`files.filename`), or all of them when none do; a single object; a non-JSON answer. A file answer is saved as usual, with the note that `--where` and `--fields` were not applied. Separately, a path no item of the list has at all (often a typo) counts as `null` in every item, with a note (`no item in <list> has <path> (...), so it counted as null in each`), since `!=` would otherwise silently keep everything. A condition without an operator or a field, with `==`, or with an empty path part (`a..b`) is an error before anything is called. The rest of the answer (`count`, `total_count`) is unchanged. Conditions on different lists each filter their own. When a condition keeps none of a non-empty list's items, `call` prints on stderr `note: --where '<condition>' kept none of the N items in <list> (<path> here: <values>)`: up to 10 distinct values the path has on that list, as JSON (strings quoted, a missing field `null`, `, and K more` after them), or `(no item has a value at <path>)`. Likewise a `--fields` path that matches nothing gets a note listing the keys there (a note, not an error: an optional field can be missing from a whole page). Several notes are joined with `; `, and stdout is the answer as without the note. Between the daemon (or Direct Mode) and the printer, the note travels beside the answer as `{"result", "next_page"?, "note"}`. Before sending, the arguments and the manual's secrets are checked (section 5, Checks Before Sending). With a daemon whose Status lacks the `where` feature (0.2.3 or older), `call --where` and a `batch` with any condition fail before calling and say to restart it or use `--direct`. `--staged` calls a staged manual (logged as `call --staged`). When the call fails because the manual is only on the other side of `--staged`, the error gives the command with or without `--staged`; `utcp call x y` (a tool name where the JSON goes) errors with `utcp call x.y` instead of a JSON parse error. `schema` does the same for `--staged`. A file answer (non-JSON, non-text, sent with `Content-Disposition: attachment`, or the template's `"response_type": "file"`) is saved and its reference `{"type": "file", "path", "mime_type", "bytes", "filename"}` printed (`mime_type` is the `Content-Type` essence, lowercased, without parameters: `application/zip; api-version=7.1` becomes `application/zip`; `application/octet-stream` when the server sends none): to `--out <path>` (overwritten; a folder keeps the server's name; `-` is stdout), else `download_dir`, the Downloads folder, or `<cache>/downloads`. A JSON answer over 10 MB after `--where` and `--fields` goes to `<cache>/downloads` (pruned after a day) with a `note`; with `--out` any JSON answer goes to that file, and when that file can't be written the answer prints instead, with the note `the call succeeded, but its answer could not be saved, so it is shown here: <reason>`. A template's `"body_type": "file"` uploads `{"file": "<path>", "offset"?, "length"?}`, read by the calling process; only POST, PUT and PATCH carry it, so on GET, DELETE or HEAD the call fails before sending (a broken manual). Arguments not used in the path or `header_fields` go to the query, except the body: POST, PUT and PATCH send the `body_field` argument (else all arguments as one JSON object); DELETE sends the `body_field` argument as the body only when the template sets `body_field` and the call passes it; GET and HEAD send none. `"body_type": "text"` sends the `body_field` string as is (POST, PUT, PATCH or DELETE; `Content-Type` from the template's headers, default `text/plain; charset=utf-8`); an unknown `body_type` fails as a broken manual. `--dry-run` prints the planned `query` and `body` next to `args`; when the arguments have a note, or `--fields` or `--where` were given, it prints `{"result": <request>, "note"}`, with the argument note first and then that a dry run has no answer for `--fields` and `--where` to shape. Limits: `max_upload_mb`, `max_download_mb`. A JSON or text answer that is one page of a longer list prints as `{"result": <answer>, "next_page": {<args>}}`; calling again with the call's own arguments plus `next_page` gets the next page, and the last page prints the answer alone. The HTTP transport reads two paging headers: `x-ms-continuationtoken` (Azure DevOps) becomes `{"continuationToken": <token>}`, or `{"x-ms-continuationtoken": <token>}` when the template's `header_fields` declares that name (any case); `Link` with `rel="next"` (RFC 8288, GitHub) becomes the next URL's query pairs that differ from the ones this call sent (the template URL's query and the query arguments), as strings, after resolving a relative URL. `--where`, `--fields` and `--out` apply to `result` only (a page with no matching items keeps `next_page`); file answers ignore paging headers. | **Hot (Daemon IPC or Async Direct)** |
| `utcp batch [--stdin \| --file <path>] [--concurrency 8] [--fields <k1,k2>] [--where <cond>]... [--pretty \| --compact] [--log] [--staged]` | Executes multiple tool calls concurrently over a shared pool (supports per-item `"fields": [...]` (or one comma-separated string) as well as `--fields`, per-item `"where": ["path=v", ...]` (or one condition as a string; another type is an error naming the item) as well as `--where` (the flags apply to items without their own; a bad condition fails the batch before any call, naming the item), and per-item `"out": "<path>"` like `call --out`, but not `-`). An item whose answer is a page has `next_page` next to its `result` (see `call`), and an item with an argument, `--where` or `--fields` note has it as `note` there (several joined with `; `). `args` may also be written `arguments`. `latency_ms` is rounded to 0.01 ms. `id` is optional (default: the item's position from 0); unknown item keys and duplicate ids are refused before any call; when any item failed, the command exits with status 2 after printing every result. With `--staged`, every item calls a staged manual. Without `--file`, the array is read from stdin when `--stdin` is given or stdin is not a terminal (a pipe or file; nothing read is an error). | **Hot (Daemon IPC or Async Direct)** |
| `utcp daemon <start \| stop \| status \| restart \| reload-env \| run>` | Manages the `--user` local IPC daemon. On Linux `start`, `stop` and `restart` delegate to `systemctl --user` when `utcp-daemon.service` is installed (`systemctl --user cat` succeeds), the setup is the default one (not a separate setup, section 3) and `UTCP_SOCKET_PATH` is not set (for `start` and `restart`, also no `--idle-timeout`). `reload-env` syncs the shell variables registered manuals use into daemon RAM, rereads the daemon's environment and `.env`, and runs the `[secrets]` command again; it prints the command's result. `start`, `restart`, and `run` take `--idle-timeout <minutes>`; `status` takes `--json` and shows the secrets command's last run, the daemon's `version` and its `features` (request fields older daemons ignore, currently `where` and `search_staged`; a daemon before 0.2.4 sends neither and shows as `0.2.3 or older`), with a `WARNING` line when the version differs from the CLI's. | Management |
| `utcp daemon servers [--json]` | Lists the local (stdio) MCP servers of registered manuals: state, PID, uptime, restart count, program name. Errors when no daemon is running (in direct mode each call starts and stops its own server). | Management (Daemon IPC) |
| `utcp daemon stop-server <manual>` | Stops a manual's local MCP server now; waiting calls fail at once and the next call starts it again. Errors for unknown manuals and manuals without a local server. | Management (Daemon IPC) |
| `utcp daemon restart-server <manual>` | Stops a manual's local MCP server and starts it again now, printing the new PID (or the start error). | Management (Daemon IPC) |
| `utcp import <source> [-o <path>] [--name <ns>] [--base-url <url>] [-H <k:v>] [--register [--hint <text>]]` | Proactive OpenAPI/Swagger/UTCP conversion to disk ("cache-before-register"). The name defaults to the first URL host label or the file name. `${VAR}` placeholders in `-H` are resolved for the spec download (also `register` and `refresh`) but saved as placeholders; a failed download names unset ones. | Cold |
| `utcp import-mcp [<name>] (--command <program> [--arg <a>...] [-e KEY=VALUE...] \| --url <url> [-H <k:v>...]) [-o <path>] [--register [--hint <text>]]` | Discovers tools via MCP `initialize` + `tools/list` (60 s timeout) and writes a lazy `.utcp.json` manual (`mcp` outbound client). `--command` is the program only; each `--arg` is one argument (values may start with `-`). `${VAR}` placeholders in `-e`/`-H` are resolved for discovery from the shell, `.env` and the secrets command but saved as placeholders; discovery errors name unset ones. The name defaults to the URL host (`mcp.context7.com` -> `context7`) or the launched package (`npx -y @upstash/context7-mcp` -> `context7`). | Cold |
| `utcp register <name> [<source>] [--cache \| --no-cache] [--base-url <url>] [-H <k:v>] [--hint <text>]` | Registers a manual into `.utcp_config.json`, caches `manuals/<name>.utcp.json`, and rebuilds `search_index.bin`. Without `<source>`, re-registers a deregistered manual offline from its saved copy (`--hint` is still allowed). `--hint` sets the few words shown next to the name in the skill description; without it an existing hint is kept. Prints a `Hint` line (or how to add one). | Cold |
| `utcp stage <name> <source> [--base-url <url>] [-H <k:v>] [--hint <text>]` | Like `register`, into the staging area (`manuals/staged/`); the active manuals, index and skills are untouched, and the daemon reloads so `--staged` calls work. Prints `Staged`, `Replaces`, `Hint`, `Secrets` and `Next` lines. | Cold |
| `utcp diff [<name>... \| <manual>.<tool>...] [--json]` | With `<manual>.<tool>` names, shows each tool's changed parts, old against new (section 5). Otherwise compares staged manuals with the manuals they replace: `new manual, N tools`, or counts plus `+` added, `-` removed and `~` changed tools (`~ tool: what; what`, section 5), and a hint change. `--json` gives `changed` as `[{"tool", "what": [...]}]`. Ends with a `Next` list: see the tools (`utcp list <manual> --staged`), check they are found (`utcp search --staged`), test, promote, discard; the promote and discard lines name exactly the manuals shown, a shared `<prefix>_` as `'<prefix>_*'` when that selects no other staged manual (`utcp promote 'azdo_*' zoo`); and one change (`utcp diff <manual>.<tool>`). | Cold (Sync) |
| `utcp promote <name>...` / `--all` | Makes staged manuals (names or `prefix*` patterns, or all with `--all`) active, keeping the replaced registration for `rollback`; rebuilds the index, reloads the daemon, rewrites the installed global skills. | Cold (Sync) |
| `utcp discard <name>...` / `--all` | Drops staged manuals (names or `prefix*` patterns, or all with `--all`). | Cold (Sync) |
| `utcp rollback <name>` | Swaps the active manual with the version the last `promote` replaced (twice undoes it); rebuilds, reloads, rewrites skills. | Cold (Sync) |
| `utcp set-hint <name> (<text> \| --clear)` | Sets or clears a registered or deregistered manual's hint (whitespace collapsed, at most 60 characters, no control characters). For an active manual it also rebuilds the search index (hint words are searchable), tells the daemon to reload, and rewrites the installed global skill copies. | Cold |
| `utcp get-hint [<name>]` | Prints one manual's hint (or how to add one), or every registered manual with its hint (`-` when none). | Cold (Sync) |
| `utcp deregister <name> [--purge-cache]` | Removes a manual from search and calls, keeping its settings and saved copy for `utcp register <name>`; `--purge-cache` deletes both for good, with any staged copy and previous version. Rebuilds index. | Cold |
| `utcp refresh <name>...` / `--all` | Re-fetches (names or `prefix*` patterns) and updates cached `.utcp.json` manuals from their recorded `source_url`, or for `import-mcp` manuals asks the MCP server again (recovered from the saved manual, placeholders resolved as at import), and rebuilds `search_index.bin`. | Cold |
| `utcp log <on \| off \| status \| clear>` | Explicitly enables/disables opt-in disk activity logging (`logging.enabled` sentinel) or clears `activity.jsonl` / `.bak`. | Cold (Sync) |
| `utcp log recent [--limit 20] [--failed] [--json]` | Shows recent calls, newest first, with the full redacted error text of failures, in local time to the second (`--json` keeps the stored RFC 3339 UTC timestamps). Reads the daemon's in-memory ring (last 500 calls, works with logging off); without a daemon it reads the log files. | Hot (Daemon IPC) |
| `utcp media <status \| clear>` | Inspects or clears the bounded media spool (`$XDG_RUNTIME_DIR/utcp-media` on Linux, `%TEMP%\utcp-media` on Windows). | Cold (Sync) |
| `utcp doctor [--json]` | Checks daemon status (with the daemon's version, `daemon_version` in `--json`, and a warning when it differs from the CLI's), config paths, index freshness, the number of staged manuals, credential status (`SET` vs. `MISSING`, with the source: `secrets command`, `caller shell`, `daemon environment`, or `environment` without a daemon; one line per secret listing every manual that uses it, staged ones marked), the secrets command (always shown: its last run, or `not set`), the `.env` file (always shown, with whether it exists), an invalid `settings.toml`, the settings file and any setting in it with no effect (with a hint such as a commented section header), the upload and download limits, the downloads folder and where it comes from, logging state, and IDE skills (in a separate setup, which compares the skills with its own manuals, an `OUTDATED` line gets a note that `sync-ides` there would replace skills another setup wrote). With the daemon running, the credentials and secrets command are the daemon's (what calls use). | Cold (Sync) |
| `utcp sync-ides [--global] [--workspace <dir>] [--status] [--print-skill]` | Installs or repairs the `utcp-tools` and `utcp-admin` skills (plus `utcp-admin/operating.md`) across Zed, VS Code, Copilot CLI, Antigravity CLI, and Hermes without requiring Windows Admin symlink privileges. `--print-skill` writes the rendered `utcp-tools` skill to stdout (release archives and ebuilds). | Cold (Sync) |
| `utcp ui` | Launches the interactive `ratatui` companion Terminal UI (does not auto-enable logging). | Interactive |

**Text output.** `list` and `search` rows start with `* `, so a long description that wraps is not mistaken for a
new tool. In a terminal, tool names (`list`, `search`) and manual names (`diff`) are blue, and `SET` (`doctor`) and
`UP TO DATE` (`doctor`, `sync-ides --status`) are green. Color is off when stdout is not a terminal, `NO_COLOR` is set,
or `TERM=dumb`, so agents, pipes and `--json` get plain text. When stdout closes early (`utcp list | head`,
`utcp call ... --out - | head -c 100`), the rest of the output is dropped and the command ends quietly with exit code
0 (a download to stdout stops); SIGPIPE stays ignored, so a closed daemon socket or MCP server pipe is an ordinary
error, never a crash.

---

## 7. Companion Application Specification (Phase 1 `ratatui` TUI -> Phase 2 GUI)

`utcp ui` (Phase 1 `ratatui`) connects to `utcp daemon` when running (for real-time in-memory metrics and warm pool
stats) and watches state/config directories via the `notify` crate when in standalone mode. Opening `utcp ui` is
strictly read-only until the user triggers an action.

### TUI Views & Keybindings

1. **Top Status Bar (Always Visible):**
   - Shows **Execution Mode:** `DAEMON RUNNING (PID 1234, 87 reqs, 3 manual env)` (requests served, env vars held)
     vs `STANDALONE DIRECT MODE`, with a single hotkey (`D`) to Start / Stop the local daemon on demand.
   - Shows **Activity Log Status:** `LOG: OFF` vs `LOG: ON`, with hotkey (`L`) to toggle.
   - **Keys that work on every tab:** `Tab` / `Shift+Tab` / `1`-`6` switch tabs, `D` starts or stops the daemon, `R`
     restarts it, `E` sends the shell's manual env vars to it, `L` toggles the log, `?` opens help, `q` quits.
   - **Help (`?`):** opens a scrollable popup listing the current tab's keys first, then the keys that work
     everywhere, then the other tabs. On Tab 3 it also explains what importing and MCP are, with examples and the
     equivalent CLI commands. While open it takes every key (`j`/`k`, `PgUp`/`PgDn`, `Home`/`End`, wheel scroll;
     `Esc`, `?`, `q` or `Enter` close). While a text field is being edited, `?` is typed as text.
   - **Key hints are light blue:** every on-screen hint (`Enter: Browse tools`, `[Shift+D]`, `x: clear`, ...) in
     titles, hint lines, popups, the header and the status line is drawn in light blue, while labels and `|`
     separators keep their normal colour, so the available keys are easy to spot on every tab.
2. **Tab 1: Manuals & Cache Inspector:**
   - Table of registered manuals showing: Name, Protocol(s) (`http`, `ws`, `cli`, etc.), Mode (Cached Disk / Live
     Remote), Tool Count, File Size, SHA-256 (short), `.env` Credential Status (`SET` in green vs
     `MISSING: my__api_TOKEN` in red, never exposing secret values), and State (last column).
   - `a` toggles **Registered Manuals** (default) and **All Manuals**, which also lists deregistered manuals and saved
     `.utcp.json` files with no registry entry. Those rows are dim and italic, with State `○ deregistered`,
     `○ saved file`, or `○ copy missing` (cannot be restored offline) instead of a green `● registered`.
   - Actions on a registered row: `r` Refresh manual, `Enter` Browse its tools (opens Tab 2 filtered to `<manual>.`
     with the first tool selected; there is no intermediate popup), `d` Deregister, `p` Purge. On an unregistered row:
     `r` Register (offline, from the saved copy), `p` Purge. `i` opens the Import tab.
   - `d` and `p` open a confirmation popup first, with **No** highlighted. While it is open it takes every key: `←` /
     `→` move between Yes and No and `Enter` picks the highlighted one; `y` confirms and `n` / `Esc` cancel directly.
     The mouse is ignored.
   - **Skill hint strip:** below the table, a one-line box shows how the selected manual appears in the skill
     description (`context7 (library and framework docs)`), or says it has no hint and that agents see only its
     name, or that an unregistered manual is not in the skill (a kept hint is shown).
   - **Hint popup (`h`):** edits the selected manual's hint (registered or deregistered; a saved file with no
     registry entry must be registered first). It is always in `TYPING` mode, prefilled with the current hint,
     caret at the end (`←`/`→`, `Home`/`End`, `Backspace`/`Delete`), and shows a live `Agents will see:` preview
     and an `N/60 characters` counter. `Enter` saves (empty text clears the hint), `Esc` cancels; an invalid hint
     (too long, control characters) keeps the popup open with the error. Saving an active manual's hint rewrites
     the installed global skill copies and reloads the search index (hint words are searchable) right away. Every
     key goes to the popup; the mouse is ignored.
3. **Tab 2: Tool Search & Schema Browser:**
   - Live interactive search bar querying the catalog as you type, showing latency in microseconds, 1-line tool
     summaries on the left (Matches pane), and a vertically scrollable right pane (with a visual scrollbar) for the
     selected tool. Outside the search box, `x` clears the query. `Esc` / `Backspace` returns directly to Tab 1 with
     the same manual still selected when Tab 2 was opened from there; otherwise it first clears a non-empty query,
     then returns to Tab 1. Results are capped at 500 so a `<manual>.` filter lists every tool of a large manual.
   - **Schema / Docs toggle (`d`):** the right pane has two views. **Schema** (the default) shows the one-line
     summary, the raw Input Schema (Parameters) JSON, and the Output Schema JSON. **Docs** shows the full
     multi-paragraph description, a readable parameter list derived from the input schema (required first; type,
     description, `enum` values, `default`), and tags. The choice lives only in memory: it persists across tool
     changes and tab switches but every `utcp ui` launch starts on Schema. While typing in the search box, `d` is typed
     as a letter.
   - **Typing in the search box (`/` or `e`):** the box gets a thick yellow border, a black-on-yellow `TYPING` badge
     and a cursor, so it is clear that keys go into it. `Esc`, `Enter`, `↓` or `←` stop typing and focus Matches;
     `→` stops typing and focuses the right pane. In Matches, `↑` (`k`) on the first row (or with no matches) goes back
     up into the box, like `/`.
   - **Try popup (`t`):** opens a popup for the selected tool with a JSON arguments box, prefilled with the schema's
     required properties as empty values of their type (`""`, `0`, `false`, `[]`, `{}`; `{}` when none are
     required). With required properties the box starts in `TYPING` mode with the caret inside the first `""`;
     `←`/`→`, `Home`/`End`, `Backspace`/`Delete` edit in place and `Esc`/`Enter` stop typing (`e` or `/` resume).
     Outside the box, `d` runs a **dry run** (the resolved request, secrets redacted, nothing sent) and `Enter` makes
     the **real call**; the output (text as-is, JSON pretty-printed, at most 5,000 lines) or error (secrets redacted)
     shows in the lower pane, which scrolls with `↑`/`↓`, `PgUp`/`PgDn`, `Home`/`End` and the wheel. `Esc` or `q`
     closes the popup. Arguments that are not a JSON object show an error and nothing runs. The run is queued until
     after the next draw, so "Calling the tool..." is visible while a slow tool answers. It goes through the daemon
     when it is running, otherwise directly, like `utcp call` (caller `tui`), so it appears on Tab 5 while the log is
     on.
   - **Pane focus:** `←`/`→` (or `h`/`l`) move keyboard focus between Matches and the right pane. The focused pane
     has a thick yellow border and a `● … — focused` title; the selected match stays highlighted (`▸`, reversed)
     while the right pane is focused. Opening a manual from Tab 1 focuses Matches so `↑`/`↓` step through its tools.
   - **Keys:** `↑`/`↓` (`k`/`j`) move the selection when Matches is focused, or scroll one row when the right pane
     is focused. `PgUp`/`PgDn` (aliases `[`/`]`) page the right pane by its visible height (minus one row of
     overlap) from either pane. `Home`/`End` jump to the top or bottom.
   - **Mouse:** `utcp ui` enables terminal mouse capture. The wheel scrolls the pane under the pointer (3 rows per
     notch in the right pane, one match per notch in Matches); a left click focuses a pane. On Tabs 1 and 5 the
     wheel moves the selection. While capture is on, most terminals need `Shift`+drag to select text.
   - Scroll limits, the `Line X/Y` title, and the scrollbar count **wrapped** terminal rows (ratatui
     `Paragraph::line_count`, feature `unstable-rendered-line-info`), so long description lines are reachable.
4. **Tab 3: Import (OpenAPI / UTCP spec or MCP server):**
   - A mode line on top; `m` switches between **OpenAPI / UTCP spec** and **MCP server**. Each mode keeps its own
     form, so switching loses nothing. Five fields: Source (spec URL or file; or MCP server URL or command line),
     Name (optional; the field title shows the inferred default), Hint (optional; a few words for the skill
     description, used by `r` only), Headers / Env vars (`;`-separated: `Header: value` for URLs, `KEY=value` for MCP
     commands; the title follows what Source holds), and Output file (optional).
   - An MCP command line is split on whitespace into the program and its arguments (no quoting; quotes are refused
     with a pointer to the CLI's `--arg`).
   - Keys: `e`/`Enter` start typing in the highlighted field (`Tab`/`↓` next field, `Shift+Tab`/`↑` previous,
     `Enter`/`Esc` done; that field has a thick border, a `TYPING` badge and a cursor, and long values show their
     end), `j`/`k` pick a field, `x` clears the form (the mode stays), `c` imports and saves a file only, `r` imports
     and registers (then notifies the daemon).
   - `c` with an empty Output writes `<cache_dir>/imports/<name>.utcp.json` (never the folder `utcp ui` was started
     from); with `r` the Output is an optional extra copy. `~/` is expanded.
   - `c`/`r` first show a "working" status, and the import runs after that frame is drawn (an MCP handshake can
     take up to 60 s while `npx` downloads a package). The result line names the manual, tool count, file or
     registration, the variables it needs, and warns when a header/env value without `${VAR}` was saved as typed.
     After `r` with an empty Hint it suggests adding one with Tab 1 `h`.
   - The panel below the fields shows mode-specific examples (`https://petstore3.swagger.io/api/v3/openapi.json`,
     `https://mcp.context7.com/mcp`, `npx -y @modelcontextprotocol/server-everything`); `?` has the full guide.
5. **Tab 4: Settings & Skills:**
   - Toggle switches for the daemon (`D`), `cache_on_register` (`c`, rewrites only that line of `settings.toml`,
     keeping every comment, and uncomments it if needed), and opt-in
     `record_activity_log` (`L`, toggles the `logging.enabled` sentinel), plus the config registry path.
   - **Agent Skills: Your Account** lists the account-wide skills folders (Zed, VS Code + Copilot CLI, Antigravity
     `agy`, Hermes) as `UP TO DATE` / `OUTDATED` / `NOT INSTALLED`. `s` rewrites them (**Sync / Repair**).
   - **This Folder: Project Skills** treats the folder `utcp ui` was started from as a project skill location. It
     shows whether `.agents/` exists, which other skills are in `.agents/skills/`, and the status of the utcp skills.
     `w` adds or updates `.agents/skills/utcp-tools/` and `utcp-admin/` there after a Yes/No popup (default No); a
     folder that cannot be written reports the error in the status bar. `s` only updates this copy once it exists, so
     it never creates files in the start folder. To manage another repo, project, or workspace, start `utcp ui` there.
6. **Tab 5: Recent Activity Feed (`activity.jsonl` + Daemon Ring Buffer):**
   - Live-tailing table of recorded `search`, `call`, and `batch` invocations across the daemon ring buffer and
     `activity.jsonl` (plus `.bak`): Time (local; `HH:MM:SS` for today, `MM-DD HH:MM:SS` for earlier days; the log
     keeps the RFC 3339 UTC timestamp), Mode (`daemon` / `direct`), Caller (`zed`, `vscode`, `agy`, `hermes`, `tui`,
     `terminal`, or a parent process name), Target `manual.tool`, Status (`OK`, `ERR`, `k/N OK` for a partly failed
     batch), and Latency (`ms`).
   - `Enter` opens a details popup for the selected row: every field (with the full local date and time) plus the
     recorded error text (`detail`).
     It keeps a copy of the row, so the live reload does not change it. `↑`/`↓`/`j`/`k`, mouse wheel,
     `PgUp`/`PgDn`, and `Home`/`End` scroll it; `Esc`, `Enter`, or `q` close it.
   - Actions: `L` toggles the disk log, `x` clears `activity.jsonl` and `.bak`.
7. **Tab 6: Daemon & Environment Monitor (`--user` Service & Spool Inspector):**
   - Live daemon telemetry: PID, socket/pipe path, uptime, requests served, loaded manuals/tools, count of env vars
     used by manuals (the only ones the daemon keeps),
     Linux `systemd --user` `utcp-daemon.service` unit state, and media spool usage (`files`, `bytes`, `TTL`).
   - Live **Daemon Credential Table**: shows whether each manual's required env var is `SET` inside the running daemon's
     RAM vs `MISSING` (without ever exposing secret values).
   - Actions: `D` Start/Stop daemon, `R` Restart daemon, `E` **Sync / Reload Current Shell Env into Daemon**, `m` Clear
     media spool.

---

## 8. Lazy Progressive Disclosure & Cross-Client Agent Skill (`SKILL.md`)

### Lazy Context Loading & Token Budget (Why UTCP Beats Eager MCP)

Traditional MCP servers inject the full JSON schema of every registered tool into every agent's system prompt at
session startup, burning thousands of tokens before the user even types a message. `utcp-rs` uses **5-stage lazy
progressive disclosure** so an agent's context window is never polluted with unused tool definitions:

| Stage | What Happens | Context Cost | What Stays Hidden from Context |
| :--- | :--- | :--- | :--- |
| **1. Session Startup** | Agent loads only the short `description` frontmatter of the two skills (`utcp-tools`, `utcp-admin`). | **~170 tokens** for both with three manuals (~110 for `utcp-tools`, growing a few per manual name and hint; ~70 for `utcp-admin`) | All manuals, all tool names, all parameter schemas, and all transport/auth configs. |
| **2. Skill Activation** | When a task requires an external service, the agent reads `SKILL.md` to learn the CLI workflow. | **~150 tokens** | The entire registered tool catalog remains on disk / in daemon RAM. |
| **3. Tool Discovery (`utcp search`)** | Agent runs `utcp search "<query>"` and receives compact 1-line summaries (`manual.tool`, `protocol`, short `description`) for only the top matches. | **~50–100 tokens** | Full JSON parameter schemas and all non-matching tools across other manuals. |
| **4. Schema Inspection (`utcp schema`)** | Agent runs `utcp schema <manual.tool>` for the **single** tool it chose, loading only that tool's input JSON schema. | **~100–250 tokens** | Schemas for the other 99% of registered tools, plus underlying HTTP headers, URLs, and `.env` secrets. |
| **5. Execution (`utcp call`)** | Agent invokes `utcp call <manual.tool> '{...}'` and receives the raw JSON response payload. | **Response only** | Connection pooling, TLS handshakes, auth injection, and protocol framing. |

Even with 20 manuals and 500+ tools registered on the machine, an agent pays **zero tokens** for the 499 tools it does
not use in a given conversation.

### Platform-Native Skill Rendering (`SKILL.template.md` + `utcp sync-ides`)

`assets/SKILL.template.md` and `assets/SKILL-admin.template.md` are embedded into the binary via `include_str!` and
rendered for the target platform when packaged or when `utcp sync-ides` runs (using regular file writes on both Linux
and Windows so Windows never fails on unprivileged symlink creation):

- **On Linux (`utcp sync-ides`, release archives, and the Gentoo ebuild):** Renders POSIX shell syntax
  (`<<'EOF'` heredocs)
  and XDG paths (`~/.config/utcp/.env`, `$XDG_RUNTIME_DIR/utcp.sock`), omitting Windows PowerShell instructions so
  Linux agents do not waste tokens on irrelevant OS syntax.
- **On Windows (`utcp.exe sync-ides` & Windows release zip):** Renders PowerShell-native stdin piping
  (`@' {"param": "value"} '@ | utcp.exe call <manual.tool> --args-stdin`) and Windows paths
  (`%APPDATA%\utcp\config\.env`, `\\.\pipe\utcp-<USERNAME>`). Messages from the CLI and TUI name the `.env` file this
  machine actually uses.
- **Daemon-Transparent:** In both cases, the skill instructions for agents remain 100% identical whether `utcp daemon`
  is running or not: the CLI handles Daemon vs. Direct mode transparently.

### Manual Hints in the Skill Description

The frontmatter `description` is the only part of the skill every agent loads up front, so it lists the registered
manuals by name. A name such as `github` explains itself; `context7` does not. Each manual can therefore carry a
**hint**: a few words (at most 60 characters) rendered next to its name, e.g.
`registered manuals: context7 (library and framework docs), d2 (diagrams from text), github`. Manuals without a hint
show the bare name. Hints cost a few tokens per manual in every session, so they stay short.

Manuals named `<prefix>_<rest>` that share a prefix with another manual become one label, so a family of manuals
costs its common words once: `azdo_* (Azure DevOps, org ni: admin (permissions, connections, hooks), git (repos and
PRs), ...)` instead of nine hints each ending in `org ni`. The words every member's hint starts with, and the words
every hint ends with, go before the colon; each member keeps the rest of its hint. Shared words are used only when
every member has a hint; otherwise each member shows its own hint (or none). `utcp sync-ides --print-skill` shows
the result.

The description also says that these tools are not in the agent's own tool list or MCP servers and that the `utcp`
CLI reaches them, and that the skill applies whenever a request names one of the manuals. Without that, a VS Code
Copilot agent asked to "make an echo call to mocktest" (an imported MCP server) searched its own tools for a
`mocktest` MCP server, found none, and told the user to start it, although the skill was loaded.

- Set at registration with `--hint` (`register`, `import --register`, `import-mcp --register`) or Tab 3's Hint field;
  change later with `utcp set-hint <name> <text>` / `--clear` or Tab 1 `h`; read with `utcp get-hint [<name>]`.
- Stored as `hint` in the manual's `.utcp_config.json` entry (omitted when unset). Text is normalized: whitespace runs
  and newlines become one space; control characters and longer text are refused.
- Every change to an active manual's hint rewrites the installed global skill copies (like register/deregister), so
  `utcp doctor` and Tab 4 stay `UP TO DATE`.
- Hint words are also searchable: they are part of each tool's search text, so `utcp search "library docs"` finds
  `context7` tools even when no tool description uses those words. Changing a hint rebuilds the index and reloads
  the daemon.
- The skill tells agents to always pass `--hint` when they register something, and how to fix one later.

### Target Skill Paths

`utcp sync-ides` writes the same three files into each client's skills folder:

- `utcp-tools/SKILL.md`: finding and calling tools (rendered below). Its description is the only part every agent
  loads up front.
- `utcp-admin/SKILL.md`: changing the setup (register, import, deregister, hints, secrets, the daemon, local MCP
  servers, failed calls). Its rules make agents ask the user before `deregister` and `daemon stop` / `restart`,
  which affect every agent on the machine. It is rendered from `assets/SKILL-admin.template.md` with the same
  platform placeholders. Its description (~70 tokens) does not overlap the tools skill, so an agent loads only the
  one it needs.
- `utcp-admin/operating.md`: the operating guide, [`docs/operating.md`](./operating.md) as embedded in the binary,
  so it always matches the installed version. The admin skill lists its sections; agents read only the one they need.

`exists` (`NOT INSTALLED` otherwise) means `utcp-tools/SKILL.md` is there; `UP TO DATE` means all three files match
this binary. Register, deregister, promote, rollback and hint changes rewrite all three in every global folder that has
the tools skill (and that this registry wrote, see section 3).

| Client | Workspace Skills Folder (Linux & Windows) | Global Skills Folder (Linux) | Global Skills Folder (Windows) |
| :--- | :--- | :--- | :--- |
| **Zed Editor** | `.agents/skills/` | `~/.agents/skills/` | `%USERPROFILE%\.agents\skills\` |
| **Antigravity CLI (`agy`)** | `.agents/skills/` | `~/.gemini/antigravity-cli/skills/` | `%USERPROFILE%\.gemini\antigravity-cli\skills\` |
| **Hermes Agent** | `.agents/skills/` | `~/.hermes/skills/software-development/` (only when `~/.hermes/skills` exists) | `%USERPROFILE%\.hermes\skills\software-development\` (only when `%USERPROFILE%\.hermes\skills` exists) |
| **VS Code (1.146+, Copilot Chat, default Copilot harness)** | `.github/skills/` (documented; not written by utcp). A root `.agents/skills/` appears to load too (observed, not documented) | `~/.copilot/skills/` | `%USERPROFILE%\.copilot\skills\` |
| **GitHub Copilot CLI** | `.agents/skills/` | `~/.copilot/skills/` (also reads `~/.agents/skills/`) | `%USERPROFILE%\.copilot\skills\` |
| **Gentoo System Share** | - | `/usr/share/utcp/SKILL.md` (tools skill only; installed by the ebuild in the separate `adaptive-overlay` repo) | - |

The old Gemini CLI path (`~/.gemini/skills`) is not a target: Antigravity replaced Gemini CLI and does not read it.
VS Code's default Copilot harness documents `~/.copilot/skills/` and `.github/skills/` and does not read
`~/.agents/skills/` (observed on Windows 11); its Claude and Codex harnesses document `~/.claude/skills/` and
`~/.agents/skills/`. The VS Code docs say their table is not every folder a harness recognizes, and a workspace-root
`.agents/skills/` does appear to load in the Copilot harness (observed, unverified). Copilot CLI reads both
`~/.copilot/skills/` and `~/.agents/skills/` and lists a skill found in both only once (the `~/.copilot` copy wins),
so writing both causes no duplicates.
The workspace column is whatever folder an agent opens as a project; `utcp sync-ides --workspace <dir>` and Tab 4 `w`
write it explicitly, and nothing creates it implicitly.

### Rendered Contents of `.agents/skills/utcp-tools/SKILL.md` (Linux Example)

````markdown
---
name: utcp-tools
description: >-
  Call tools from utcp manuals (registered manuals: context7 (library and framework docs), d2 (diagrams from text), github): REST and OpenAPI APIs, CLI tools, WebSockets and
  imported MCP servers. These tools are not in your own tool list or MCP servers; run them with the
  `utcp` CLI (`utcp search`, `schema`, `call <manual.tool>`). Use this skill whenever a request names
  one of these manuals, instead of ad-hoc `--help` probing or scratch files.
---

<!-- Written by utcp from its built-in template and your registered manuals; edits here are overwritten. Change
manuals and hints with `utcp register`, `promote`, `deregister` or `set-hint`, which update this file. -->

# UTCP Direct Tool Calling (`utcp`)

Use the native `utcp` CLI to discover, inspect, and invoke external APIs and local tools lazily on demand so
your context window stays clean and you avoid wasting turns on `--help` probing or temporary files. Follow the
progressive disclosure workflow (`search` -> `schema` -> `call`):

## 1. Discover & Inspect Tools Lazily

- **Step 1: Search tools by keyword (compact summary):**

  ```sh
  utcp search "<keywords>"
  ```

- **Step 2: Fetch the exact JSON input schema for only the tool you need:**

  ```sh
  utcp schema <manual.tool>
  ```

- **List the manuals (name, tool count, hint), or one manual's tools:**

  ```sh
  utcp list
  utcp list <manual>
  ```

- **Staged manuals** (added with `utcp stage` for testing, not yet promoted) are not in `search`, and `list`
  shows them under Staged. Reach them only when the user asks you to test one, with `--staged` on `search`, `list`,
  `schema`, `call` and `batch`:

  ```sh
  utcp list --staged
  utcp call --staged <manual.tool> '{"param": "value"}'
  ```

## 2. Call a Tool

- **Simple JSON arguments:**

  ```sh
  utcp call <manual.tool> '{"param": "value"}'
  ```

- **Complex or multiline JSON arguments (via stdin):**

  ```sh
  utcp call <manual.tool> --args-stdin <<'EOF'
  {"param": "value"}
  EOF
  ```

- **Preview a call without executing (`--dry-run`; it shows the URL, query and body it would send):**

  ```sh
  utcp call <manual.tool> '{"param": "value"}' --dry-run
  ```

- **Filter Verbose API Responses (`--fields`):** For noisy APIs (such as GitHub commits, issues, or repos), pass
  `--fields key1,nested.key2` (for example, `--fields sha,commit.message,commit.author,html_url`) to strip unneeded
  boilerplate keys while keeping full fidelity when omitted. Dotted paths pass through lists (`assets.name`), and
  for a wrapped list such as `{"total_count": N, "workflow_runs": [...]}` you can name the item keys directly
  (`--fields id,name,conclusion`), also next to top-level keys (`--fields total_count,name`). A key that itself has
  dots is one name (`fields.System.State` on an Azure DevOps work item). A path that matches nothing gets a note
  listing the keys there.
- **Keep only matching list items (`--where`):** use the tool's own filter arguments first (see `schema`), then
  `--where 'path=a|b'` (equals `a` or `b`), `~=` (contains) or `^=` (starts with); `!=`, `!~=`, `!^=` keep the
  others. `>`, `>=`, `<`, `<=` compare numbers and dates (`'completed_at>=2026-11-24'`); a date alone means that
  whole day in UTC, with `=` too. Quote the condition. Case is ignored; a missing field is `null`
  (`pull_request!=null` keeps the items that have one); name a field inside an object (`user.login=octocat`); `\|`
  is a `|` inside a value. Repeat `--where` for more conditions; all must hold. Start the path with the list to
  filter when the answer has several (`'records.result=failed|canceled'`). Through a list inside each item
  (`labels.name=bug`), an item matches when any element does. `count` and `total_count` still count the unfiltered
  list, and `--where` runs before `--fields`. When nothing matches, a note (`note` in `batch`) lists the values the
  field has: fix the value rather than conclude there are none. When the answer has no list to filter (one object),
  the call still succeeds: you get the whole answer and a note saying why, so don't call again.
- **Long lists come in pages:** when a list has more, utcp prints `{"result": <answer>, "next_page": {...}}`. For
  the next page, call the same tool again with your arguments plus the ones in `next_page` (such as
  `{"page": "2"}` or `{"continuationToken": "..."}`). The last page prints the answer alone. `--where`, `--fields`
  and `--out` apply to `result`, so a page with no matching items still has `next_page`; in `batch`, `next_page` is
  next to the item's `result`. Fetch more pages only when you need them, and say so when you stop early.
- **Images and Binary Media:** If a tool writes an image or binary payload to a file path on disk (for example,
  `"type": "media_file"` with a `"path"`), pass that file path to your multimodal file-reading/vision tool only when
  visual inspection is needed.
- **Files a tool answers with** (an archive, a log, a PDF): utcp saves the file and prints
  `{"type": "file", "path": ..., "mime_type": ..., "bytes": ..., "filename": ...}` instead of its content
  (`mime_type` is the bare media type, such as `application/zip`, without parameters). Without `--out` it goes to
  the user's Downloads folder (`a.zip` becomes `a (1).zip` if it exists; nothing is overwritten). To choose the
  place, pass `--out <path>` (overwrites, like `curl -o`; a folder keeps the server's file name), or `--out -` to get
  the raw bytes on stdout for a pipe. Open the file only if you need its content.
- **Answers too large to print:** a JSON or text answer over 10 MB is saved to utcp's cache folder
  (`~/.cache/utcp/downloads/`; on Windows `%LOCALAPPDATA%\utcp\cache\downloads\`), not to Downloads, and the call
  prints the file reference above, with its `path`, and a `note`. That folder is emptied after a day, so use `--out`
  for a file you want to keep. Read the parts you need (for example with `jq`), or call again with `--where` or
  `--fields` to get less.
- **Uploading a file:** a tool that uploads takes the file in its body argument (see `schema`) as
  `{"file": "<path>"}`; add `"offset"` and `"length"` (bytes) to send one part. A relative path is relative to your
  working folder, and utcp reads the file with your permissions.
- **Size limits:** when an upload or download fails "only because of this limit", tell the user: the error names
  the setting (`max_upload_mb` or `max_download_mb` in utcp's `settings.toml`) and how to change it. Do not change
  it yourself unless the user asks.

## 3. Execute Multiple Calls in Parallel (`batch`)

Each item has `tool` and `args`, and optionally `id` (default: its position, from 0), `"fields": ["key1",
"nested.key2"]`, `"where": ["path=value"]` and `"out": "<path>"` to save that item's answer to a file. Other keys are
refused. `--fields` and `--where` on `batch` apply to the items without their own. Every item's answer or `error` is
printed; when any item failed, `utcp` then exits with status 2:

```sh
utcp batch --stdin <<'EOF'
[
  {"id": "1", "tool": "manual.tool_a", "args": {"id": 1}},
  {"id": "2", "tool": "manual.tool_b", "args": {"id": 2}}
]
EOF
```

## 4. When a Call Fails

- **Nothing was sent:** an error that says so (a required argument missing, an argument or body field name the
  tool doesn't have, a wrong type or value, or a secret placeholder that isn't set) is about your call, not the API.
  Fix the arguments from `utcp schema <manual.tool>`; an unset placeholder is for the user to set (see
  the utcp-admin skill). A `note` that keys were "sent as given, but not in the tool's schema" means the API may
  ignore them: if the result shows no change, check those keys.
- **The API said no:** the error has its status and answer. A rate-limited API says when to retry: wait that long
  instead of retrying at once. `utcp log recent --failed` lists recent failed calls with their full errors.
- **Adding or changing manuals, hints and secrets, or the daemon and its local MCP servers** (a hung server, calls
  that time out): use the utcp-admin skill.
````

*(On Windows, Sections 2 and 3 render PowerShell `@' ... '@ | utcp.exe ... --args-stdin` syntax instead of `<<'EOF'`.)*

---

## 9. Build, Packaging & Distribution Strategy

### 1. Gentoo Linux (`adaptive-overlay`)

The ebuild and its test script live in the separate
[`adaptive-overlay`](https://github.com/Faraclas/adaptive-overlay) repository, not here. This section is what they
do; changes to it are agreed with that repository.

- **Package category/name:** `dev-util/utcp` in `adaptive-overlay` (`inherit cargo systemd`).
- **Offline Pure-Rust Build:** Uses Gentoo's `cargo.eclass` with offline vendored `${CARGO_CRATE_URIS}`. Because
  `utcp-rs` vendors `rs-utcp` into `utcp-core` with `rustls` and pure-Rust default features (no
  OpenSSL/C-bindgen/WebRTC/`protoc`), `cargo_src_compile` runs cleanly inside `containers/testenv-rust`.
- **Installed Artifacts (`src_install`):**
  1. **Unified Binary:** `dobin "$(cargo_target_dir)/utcp"` $\rightarrow$ `/usr/bin/utcp` (contains CLI, daemon, and TUI).
  2. **Systemd User Service (`utcp-daemon.service`):** Installed from the source tree via
     `systemd_douserunit utcp-rs/packaging/systemd/utcp-daemon.service` (`ExecStart=/usr/bin/utcp daemon run`,
     `Restart=on-failure`), allowing any user session to enable the persistent sub-0.2ms daemon via:
     `systemctl --user enable --now utcp-daemon`. The ebuild keeps no copy in `files/`: the unit is maintained
     upstream so manual builds use the same file. Only `systemd` is supported.
  3. **System Skill File:** Installs the Linux-rendered `SKILL.md` to `/usr/share/utcp/SKILL.md` (and `pkg_postinst`
     prints `elog` instructions for `systemctl --user enable --now utcp-daemon` and `utcp sync-ides --global`).
  4. **Docs:** `README.md`, `NOTICE`, `docs/operating.md` and the licenses with `dodoc`; from 0.1.1 also
     `examples/secret-source.sh`, the example secrets command.
- **Container Verification:** `adaptive-overlay` includes `containers/testenv-rust/verify-utcp.sh` to validate binary
  dependencies (`ldd`), Direct Mode and Daemon Mode hot-path benchmarks, OpenAPI import, and IDE skill sync.

### 2. Release Archives (`.github/workflows/utcp-rs.yml`)

- GitHub Actions workflow (`.github/workflows/utcp-rs.yml`) triggered on pushes to `main` and pull requests touching
  `utcp-rs/**`, and `workflow_dispatch`. The build jobs have read-only access; only the `release` job can write.
- **Releasing:** the version is `version` in `utcp-rs/Cargo.toml` and is never typed anywhere else. To release, run
  the workflow on `main` with `release` ticked (Actions > Run workflow, or
  `gh workflow run utcp-rs.yml --ref main -f release=true`). A first `version` job reads the version and, before any
  build starts, refuses with a short explanation when the run is not on `main`, when that version is already
  released (its `utcp-rs-v<version>` tag exists: `Cargo.toml` was not bumped), or when it is lower than the latest
  release. After every build and test passed, the `release` job checks that the built binary's `utcp --version`
  matches, checks the tag is still free, then creates the tag `utcp-rs-v<version>` on the built commit together
  with the GitHub Release. Pushing a tag by hand does not trigger anything.
- Each run:
  - Runs the full test suite on `ubuntu-latest` (`x86_64-unknown-linux-gnu`) and `windows-latest`
    (`x86_64-pc-windows-msvc`), testing both Direct Mode and Daemon Mode (Unix socket / Named Pipe), plus
    `--args-stdin` with JSON piped to the process's stdin (an integration test, on both platforms).
  - Builds the release binaries: a static Linux `utcp` (`x86_64-unknown-linux-musl`, `musl-tools` for `ring`'s C code;
    the job fails unless `file` reports a static binary) and `utcp.exe`. The glibc test build is not published.
  - Uploads release archives containing the binary, the rendered `SKILL.md`, `operating.md`, the platform's example
    secrets command (`secret-source.sh` or `secret-source.ps1`), `LICENSE`, `NOTICE` and
    `LICENSE-UPSTREAM-RS-UTCP` as build artifacts; on a release run the `release` job attaches them to the GitHub
    Release (and fails if none matched):
    - `utcp-x86_64-unknown-linux-musl.tar.gz`, which also holds `utcp-daemon.service`. Its
      `ExecStart=/usr/bin/utcp daemon run` is the packaged path; for `~/.local/bin` change it to
      `ExecStart=%h/.local/bin/utcp daemon run` (see `operating.md`). **Provided as-is:** it has no
      runtime dependencies (no glibc version, no OpenSSL) and reads CA certificates from the system store. Verified on
      Gentoo (DNS, HTTPS, daemon, parallel stdio MCP calls) and Ubuntu 26.04 LTS (DNS, HTTPS, daemon, parallel calls,
      the `systemd --user` unit from `~/.local/bin`); other distributions are expected to work but are not tested.
      musl's memory allocator is slower than glibc's under heavy multi-threaded allocation, which does not matter for a
      daemon that spends its time waiting on the network.
    - `utcp-x86_64-pc-windows-msvc.zip`, which also holds `install.cmd`, `install.ps1` and `README.txt` (plain text,
      so it opens in Notepad). The README says to run `install.cmd` (double-click, or `.\install.cmd` in a terminal):
      `install.ps1` is unsigned and the zip is downloaded, so a bare `.\install.ps1` is blocked by the default
      execution policy (Restricted on Windows clients; RemoteSigned refuses downloaded unsigned scripts) before any of
      it runs. A batch file is not subject to that policy, so `install.cmd` runs
      `powershell -NoProfile -ExecutionPolicy Bypass -File install.ps1` with the given options. It clears
      `PSModulePath` first, because Windows PowerShell 5.1 started from PowerShell 7 inherits 7's module path and
      cannot load its own modules. When Explorer started it (double-click), it pauses at the end so the window stays
      open. CI checks that the README names `.\install.cmd` and that `secret-source.ps1` parses (the Linux job runs
      `bash -n` on `secret-source.sh`), then unzips it, runs `install.cmd`, checks
      `PATH`, the skills, the licence files and the logon task, re-runs it with `-NoDaemonAtLogon` and checks that the
      task is gone, installs again, then runs `-Uninstall` and checks that the program, the task and the `PATH` entry
      are gone.
- **`packaging/windows/install.ps1`** (Windows PowerShell 5.1 and later, no admin rights): copies `utcp.exe` to
  `%LOCALAPPDATA%\Programs\utcp` (stopping a running daemon first, and renaming the old `utcp.exe` to
  `utcp.exe.old-<id>` because any running `utcp` process, such as `utcp ui`, still locks it; the old file is deleted
  after the copy, or by the next install if something still runs it), copies `operating.md`, `SKILL.md`, `LICENSE`,
  `NOTICE` and `LICENSE-UPSTREAM-RS-UTCP` next to it, adds that folder to the
  user `PATH` (read and written raw as `REG_EXPAND_SZ`, so other entries keep their `%VAR%` references), runs
  `utcp sync-ides --global`, and registers the `utcp daemon` scheduled task (logon trigger, `Interactive` logon,
  `Limited` run level, runs `utcp daemon start`, which detaches without a console window). It then starts the daemon
  through that task, so the daemon never runs elevated, even when the script does. `-NoDaemonAtLogon` skips the task
  and removes one an earlier install made; a daemon that was running before is then restarted directly (not from an
  elevated shell). Re-running upgrades in place; `-Uninstall` removes the task, the `PATH` entry and the program
  folder and keeps manuals, secrets and skills. No MSI or `winget` package for now.

---

## 10. Verification & Performance Acceptance Criteria

1. **Zero-Runtime & Shared Library Check:**
   - `cargo build --release -p utcp-cli` produces a single native binary (`utcp` / `utcp.exe`) containing the CLI,
     daemon, and TUI.
   - `ldd target/release/utcp` on Linux (the glibc build; the musl release binary is static) shows only standard
     system libraries (`libc.so`, `libm.so`, `libgcc_s.so`), with
     zero `libssl` / `libcrypto` / Python / Node.js dependencies.
2. **Dual-Mode Hot-Path Latency & Concurrency Benchmark:**
   - With 5+ cached OpenAPI manuals registered (~500+ total endpoints):
     - **Direct Mode (`--direct` / daemon stopped):** `utcp search "user"` completes in **< 5 ms on Linux** (**< 15 ms
       on Windows**), and `utcp call` local setup overhead prior to network I/O is **< 5 ms**.
     - **Daemon Mode (`utcp daemon start`):** `utcp search "user"` completes in **< 1 ms on Linux** (**< 3 ms on
       Windows**), and `utcp call` reuses warm HTTP/WebSocket pools across calls.
   - 20 concurrent `utcp search` and `utcp call` invocations complete with zero file corruption, response mixing, or
     lock contention in both Direct and Daemon modes.
   - Several agents at once, each with unique inputs checked against their own expected answers, get zero errors and
     zero mixed-up answers. Measured on a Gentoo desktop (3 agents for 60 s, 20 parallel calls each, every call a
     separate `utcp` process): `search` 83,763 calls, `d2.format` 32,038, stdio MCP echo answering in random order
     42,625; 0 errors, 0 wrong answers, p95 under 50 ms, daemon RSS 21 MB afterwards.
   - `python3 tests/load/hammer.py` (on demand) passes: 8 agents each on `search`, an OpenAPI tool, a CLI tool, and a
     stdio MCP tool, all at once for 60 s, with zero errors and zero wrong answers. With `--kill-mcp-at`, only calls
     in flight at the kill fail, and later calls succeed on a new server. Measured on a Gentoo desktop (32 agents,
     60 s, MCP server killed at 30 s): 227,249 calls, 0 errors, 0 wrong answers, 8 calls lost to the kill, p95 34 ms
     or less, daemon peak RSS 19 MB.
3. **Seamless Fallback Check:**
   - Stopping the daemon via `utcp daemon stop` causes subsequent `utcp search` and `utcp call` invocations to succeed
     transparently via Direct Mode with zero errors.
   - No stdio MCP server outlives its owner: after `import-mcp` discovery, `deregister`, `daemon stop`,
     `daemon stop-server` / `restart-server`, and Ctrl+C or `SIGTERM` on a direct-mode `utcp call` (exit 130 / 143),
     the server's whole process tree is gone, including servers that ignore stdin closing.
4. **Cache-on-Register vs. Import Verification:**
   - `utcp import <openapi_spec> -o test.utcp.json` writes a valid UTCP manual without modifying `.utcp_config.json`.
   - `utcp register petstore <openapi_spec>` with `cache_on_register = true` writes `manuals/petstore.utcp.json`,
     updates `.utcp_config.json`, rebuilds `search_index.bin`, and live-reloads the daemon catalog if running.
5. **Opt-In Activity Log & 1-Backup Rotation Check:**
   - Confirm `activity.jsonl` is untouched when `logging.enabled` is absent (including after opening/closing `utcp ui`).
   - Confirm `utcp log on` records calls and rotates `activity.jsonl` $\rightarrow$ `activity.jsonl.bak` cleanly when
     reaching the 1 MB size cap, keeping at most 1 backup file.
6. **Cross-Platform IDE Sync & Stdin Quoting Check:**
   - `utcp sync-ides` populates the `utcp-tools` and `utcp-admin` skills and `utcp-admin/operating.md` without
     requiring elevated privileges on Windows, `utcp doctor` reports `OUTDATED` when any of the three is missing or
     stale, and piping JSON via `utcp call ... --args-stdin` succeeds identically under POSIX `sh`/`bash` and Windows
     PowerShell.
