# Issue 016: Windows Daemon and Process Bugs Found by Running the Test Suite on Windows

- **Upstream Crate:** `rs-utcp` `v0.3.2` (the console-window part); the rest is local to `utcp-rs`
- **Upstream Files:** `src/transports/cli/mod.rs`, `src/transports/mcp/mod.rs` (processes started without
  `CREATE_NO_WINDOW`)
- **Local Files:** `crates/utcp-cli/src/daemon.rs`, `crates/utcp-core/src/ipc.rs`, `crates/utcp-core/src/transports/`,
  workspace `Cargo.toml` (`notify`)
- **Local Status:** Fixed in `utcp-rs`
- **Upstream Issue / PR:** Not filed yet

## How They Were Found

CI was the only place the Windows build ran, so Windows failures would have shown up only after a public push. The
test suite was cross-compiled for `x86_64-pc-windows-gnu` (mingw-w64) and run under Wine, with a portable Windows
`node.exe` for the Node fixtures. Wine is not Windows: findings marked *(Wine)* below only occurred there, but each is
fixed in a way that is correct on Windows too.

## Problems and Fixes

1. **`utcp daemon start` never returns to a caller that captures its output.** Windows starts children with handle
   inheritance on, so the detached daemon inherited the inheritable stdout/stderr handles its caller gave
   `daemon start`. An agent's shell tool, `$x = utcp daemon start`, or a test using `Command::output()` waited for
   end-of-file until the daemon stopped. **Fix:** `daemon start` clears `HANDLE_FLAG_INHERIT` on its own standard
   handles before spawning the daemon (`Stdio::inherit` still works for later children, because Rust duplicates the
   handles as inheritable at spawn time). Unix was not affected: the daemon's descriptors 0 to 2 are `/dev/null` and
   everything else is close-on-exec. *Replaced in issue 017 (item 4):* on real Windows other inherited handles still
   held the caller open, so the daemon is now started with `CreateProcessW` and no inherited handles.
2. **The daemon aborted on file-change events** *(Wine)*. `notify` 6.1 read `FILE_NOTIFY_INFORMATION` records through
   aligned pointers. Wine packs the records on 2-byte boundaries, so the read was misaligned: undefined behaviour, and
   an abort in debug builds (`misaligned pointer dereference`, exit code `0xc0000409`). The daemon died on the first
   `deregister` and the CLI silently fell back to direct mode. **Fix:** `notify` 8.2, which reads the records with
   `read_unaligned`. Real Windows pads records to 4 bytes, so release builds there were not known to fail.
3. **Each tool process could open a console window.** The daemon runs detached with no console, so every console
   program it started (an MCP server, `npx` through `cmd.exe`, a CLI tool, a text script) got a new console window of
   its own. **Fix:** `transports::hide_console_window` sets `CREATE_NO_WINDOW` on every tool process; their stdio is
   piped, so nothing is lost.
4. **A busy pipe meant "no daemon".** The daemon creates its next pipe instance right after a client connects; a client
   that arrives in that gap gets `ERROR_PIPE_BUSY`, which the client treated like a missing daemon, so the call ran in
   direct mode (for a stdio MCP manual: a fresh server per call). **Fix:** the client retries `ERROR_PIPE_BUSY` for up
   to 2 s.
5. **Another account could stand in for the daemon.** Pipe names are machine-wide, unlike the Unix socket in the
   user's private `XDG_RUNTIME_DIR`. Another local account could create `\\.\pipe\utcp-<USERNAME>` first and receive
   the CLI's requests: tool arguments, and API keys on `daemon reload-env`. **Fix:** before sending, the client checks
   that the pipe's owner is the current token's user or its default owner (Administrators in an elevated shell) and
   otherwise fails with an error naming the pipe. Wine reports no owner for pipes; that case is accepted, since every
   Wine process runs as the same Unix user and real Windows always assigns an owner.
6. **`daemon stop` could fail with "failed to fill whole buffer"** (all platforms; seen under Wine). The `Shutdown`
   request set the stop flag before its reply was written, so the daemon could exit with the reply unsent. **Fix:** the
   stop is triggered after the reply is written and the caller has hung up (at most 1 s wait).

## Tests

- Problems 1, 2 and 6 are covered by the existing `crates/utcp-cli/tests/integration_dual_mode.rs` tests when they run
  on Windows: without fix 1 every test that starts a daemon hangs; without fix 2 (under Wine)
  `stdio_mcp_server_tree_is_stopped_after_import_deregister_and_daemon_stop` fails; without fix 6 tests fail
  intermittently at `daemon stop`.
- The integration tests used one pipe name per test process, so tests running in parallel reached each other's
  daemons on Windows. Each test now uses a pipe named after its own temp directory.
- Problems 3, 4 and 5 have no automated test. 3 needs a desktop to see windows, 4 is a narrow race, and 5 needs a
  second Windows account. They are part of the manual Windows check. Problem 5 was checked on real Windows 11
  (2026-11-18): a pipe made by a temporary second account was refused from both an elevated and a normal
  shell, with nothing sent (see issue 017, Tests).
- Unix-only commands in tests (`sleep`, `echo`) were replaced with `node`, which the tests already require.
