# Issue 015: Stdio MCP Servers Behind a Launcher Keep Running After Discovery, Deregister, and Daemon Stop

- **Upstream Crate:** `rs-utcp` `v0.3.2`
- **Upstream Files:** `src/transports/mcp/mod.rs` (`McpStdioProcess::spawn`, `McpTransport::get_or_create_stdio_process`,
  `McpTransport::deregister_tool_provider`)
- **Local Files:** `crates/utcp-core/src/ipc.rs` (`DaemonServerState::reload`), `crates/utcp-core/src/catalog.rs`
- **Local Status:** Fixed in `utcp-rs/crates/utcp-core/src/transports/mcp/` and `utcp-rs/crates/utcp-core/src/ipc.rs`
- **Upstream Issue / PR:** Not filed yet (the daemon part is local to `utcp-rs`)

## Problem

Most stdio MCP servers are started through a launcher: `npx -y <package>`, `uvx <package>`, `pnpm dlx`, or
`docker run -i`. The launcher is the process utcp starts, and it starts the real server as its own child. Found while
testing against `@modelcontextprotocol/server-everything` and a mock server that keeps running after stdin closes:

1. **`import-mcp` leaves its discovery server running.** Discovery stops the server by dropping it, which kills only
   the launcher. The real server is re-parented to `init` and keeps running.
2. **`daemon stop` leaves the server running** for the same reason.
3. **`deregister` does not stop the server at all.** The daemon keeps one server per manual name in its transport and
   nothing releases it when a reload removes the manual. Even the launcher keeps running until the daemon stops.
4. **A re-registered manual keeps its old server.** The daemon reuses a live server by manual name only, so a manual
   re-registered with a different command, argument or environment variable (for example a rotated token) is still
   served by the server started with the old settings.

Well-behaved servers, such as the official SDK servers, exit by themselves when stdin closes, which hid the problem.
A server that does not exit on stdin close leaks one process per discovery, per deregistration and per daemon stop.

## Root Cause

`kill_on_drop` kills only the direct child, not its process tree. The daemon had no hook that releases a manual's
transport state when the manual goes away, and the cached server was not checked against the provider's current
launch settings.

## Local Fix in `utcp-rs`

- New `transports/mcp/process_tree.rs`. On Unix each server is started in its own process group
  (`process_group(0)`). Stopping it sends `SIGTERM` to the whole group, waits up to 2 s (`STOP_GRACE`) for the group
  to exit, then sends `SIGKILL` to whatever is left. The wait runs on a background thread, and the `utcp` binary calls
  `transports::wait_for_stopping_servers` before it exits, so a CLI command or a stopping daemon does not skip the
  `SIGKILL`. A server that exits on `SIGTERM` is stopped at once. On Windows the server is assigned to a Job Object
  with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so closing the job handle kills the whole tree; the system closes it too
  if utcp itself dies. `kill_on_drop` stays as the fallback for the direct child.
- The cached server remembers its launch settings (command, arguments, resolved environment). A provider whose
  settings differ gets a fresh server; calls still running on the old one finish first.
- `DaemonServerState::reload` (run after `register`, `deregister` and the other catalog changes) calls the new
  `catalog::release_manual_connections` for every manual the reload removed. It deregisters that manual's providers
  from their transports, which stops its stdio server tree.

### Containers (`docker run -i` / `podman run -i`)

A container is not a child of the `docker` or `podman` command: `dockerd` or `conmon` runs it, so it is outside the
process group and the Job Object. utcp stops it in two ways, both through the client process:

- Its stdin closes when the client stops. Most servers exit then.
- `SIGTERM` (Unix only) is forwarded into the container by the client (`--sig-proxy`, on by default). The first fix
  sent only `SIGKILL`, which cannot be forwarded, so a container that kept running after stdin closed was left behind.

Checked with `podman run -i --rm` (podman 5, rootless) and the Node fixture inside the container, through `import-mcp`,
a daemon call, `deregister`, another call and `daemon stop`:

| Server in the container | Left running before | Left running now |
|---|---|---|
| Exits when stdin closes | no | no |
| Ignores stdin, handles `SIGTERM` | yes | no |
| Ignores stdin and `SIGTERM` | yes | yes |

Known limits:

- A container whose server ignores both stdin closing and `SIGTERM` keeps running, because utcp cannot reach inside
  it. A process that runs as PID 1 in a container ignores `SIGTERM` unless it installs a handler; `docker run --init`
  (or `podman run --init`) puts a small init in front of it that does handle `SIGTERM`. On Windows only the stdin
  route applies.
- On Windows a launcher that starts its child in the first moment after spawn, before utcp assigns it to the job,
  would escape the job. `npx` and `uvx` take far longer than that to start the server.

Regression tests (offline; the Node fixture `crates/utcp-core/tests/fixtures/mock_mcp_server.cjs` with `--wrapper`
acts like `npx`, `--stubborn` ignores stdin closing, and `--ignore-term` ignores `SIGTERM`):

- `transports::mcp::tests::node_stdio_server_behind_a_launcher_is_stopped_with_its_whole_tree` (deregister, dropping
  the transport, and changed launch settings)
- `transports::mcp::tests::node_stdio_server_gets_sigterm_first_and_sigkill_after_the_grace_period` (Unix: a server
  that handles `SIGTERM` gets it and stops at once; one that ignores it is killed after the grace period)
- `transports::mcp::tests::stdio_dead_server_is_replaced_and_live_one_reused` (changed settings are not reused)
- `stdio_mcp_server_tree_is_stopped_after_import_deregister_and_daemon_stop` in
  `crates/utcp-cli/tests/integration_dual_mode.rs` (through the real CLI and daemon, with a server that ignores both
  stdin closing and `SIGTERM`)

The transport tests and the CLI test fail without the process-group stop, the `SIGTERM` test fails with a
`SIGKILL`-only stop, and the CLI test also fails without the reload release or when the CLI exits without waiting for
pending stops. The container table is a manual check (it needs a local image with Node), not part of `cargo test`.
