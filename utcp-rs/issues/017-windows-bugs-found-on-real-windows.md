# Issue 017: Windows Bugs Found on a Real Windows Machine (Paths, Quoting, UDP, Detached Daemon, Elevation)

- **Upstream Crate:** `rs-utcp` `v0.3.2` (items 2, 3 and 7); the rest is local to `utcp-rs`
- **Upstream Files:** `src/transports/cli/mod.rs` (`split_whitespace`), `src/transports/udp/mod.rs`,
  `src/security.rs` (`validate_command`)
- **Local Files:** `crates/utcp-core/src/paths.rs`, `crates/utcp-core/src/transports/cli/mod.rs`,
  `crates/utcp-core/src/transports/udp/mod.rs`, `crates/utcp-cli/src/daemon.rs`, `crates/utcp-core/src/ipc.rs`,
  `crates/utcp-cli/src/main.rs`, `crates/utcp-core/src/security.rs`, `packaging/windows/install.ps1`
- **Local Status:** Fixed in `utcp-rs`
- **Upstream Issue / PR:** Not filed yet

## How They Were Found

After issue 016 (Wine), the cross-compiled test suite and release binary ran on a real Windows 11 VM, over SSH, from
a profile folder with a space in its name (like `C:\Users\Jane Doe`), with an elevated SSH token and, through a
scheduled task, a normal (non-elevated) token. None of these showed up under Wine.

## Problems and Fixes

1. **Every integration test failed: "Failed to determine user home/project directories".** The tests point
   `USERPROFILE` at a folder that does not exist and set `UTCP_CONFIG_DIR` / `UTCP_CACHE_DIR` / `UTCP_STATE_DIR`.
   `UtcpPaths::resolve` still asked `ProjectDirs` first, and on Windows the Known Folders lookup fails for a missing
   profile. **Fix:** the platform folders are looked up only for a directory without an override, and the error now
   names the three variables.
2. **A `cli` tool whose path has a space could not run.** `command_name` was split with `split_whitespace`, so
   `node "C:\Users\Jane Doe\tool.js"` became three words with the quotes kept. **Fix:** `split_command_line`:
   `"..."` and `'...'` group words and are removed, `""` is an empty argument, backslashes are literal (Windows
   paths), an unclosed quote is an error. Existing manuals without quotes split exactly as before.
3. **A UDP call to a closed port failed with a raw "connection reset".** Windows reports the ICMP port-unreachable on
   the next `recv_from` as `WSAECONNRESET`; Linux ignores it and times out. **Fix:** `ConnectionReset` /
   `ConnectionRefused` there becomes "No UDP server is listening at host:port (port unreachable)". The timeout test now
   uses a bound socket that never answers, so it tests the timeout on both systems.
4. **The "detached" daemon died with the SSH session, and then the SSH session hung.**
   - Windows OpenSSH runs each session in a job object that kills every process in it when the session ends,
     including the daemon `utcp daemon start` had just started.
   - With `CREATE_BREAKAWAY_FROM_JOB` the daemon survived, but the session no longer ended: `std::process::Command`
     lets the child inherit every inheritable handle of the parent, not only the std handles, so the daemon held a
     pipe of the SSH session open. Issue 016's fix (clearing the inherit flag on our own std handles) did not cover
     those.

   **Fix:** `spawn_detached_daemon` calls `CreateProcessW` directly with `bInheritHandles = FALSE`, null std handles,
   `CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_UNICODE_ENVIRONMENT | CREATE_BREAKAWAY_FROM_JOB`, and retries without
   breakaway when the job forbids it (`ERROR_ACCESS_DENIED`). Arguments are quoted the way the C runtime splits them
   (`push_windows_arg`), since the program path may contain spaces.
5. **A daemon started as administrator was invisible to normal shells.** A daemon started from an elevated shell
   creates its pipe with the elevated token's default permissions, which admit only elevated callers. A normal shell
   got access denied, and the client treated that like "no daemon": calls quietly ran in direct mode, `daemon status`
   printed STOPPED, and `daemon start` reported success while a second daemon failed to start on the same pipe. Unix
   had the same pattern for another account's socket (root through `sudo`). **Fix:** access denied (Windows) or
   permission denied (Unix) on connect is now the error `DaemonUnreachable`, "Cannot use the utcp daemon at …", which
   says why and what to do. `daemon status|start|stop` use `query_daemon_status_checked`, which passes it on. The
   pipe permissions are left alone: admitting normal callers to an elevated daemon would let them run tools as
   administrator. The reverse works as before: an elevated shell can use a normal daemon.
6. **`install.ps1` could not register its logon task over SSH.** It named the user `%USERDOMAIN%\%USERNAME%`, and over
   SSH `USERDOMAIN` is the workgroup, which does not resolve ("No mapping between account names and security IDs").
   **Fix:** it uses the token's own name. The task is now on by default (opt out with `-NoDaemonAtLogon`), runs with
   the `Limited` run level, and the installer starts the daemon through it, so an install from an elevated shell no
   longer leaves an elevated daemon (item 5).
7. **`import-mcp --command` refused every absolute Windows path.** `validate_command` rejects shell metacharacters
   in a stdio MCP server's program, and its list held `\`, `(` and `)`: so `C:\...\node.exe` failed with "Command
   contains dangerous characters", and so would `C:\Program Files (x86)\...`. Only bare names found on `PATH` worked,
   which is all the tests used. The program is started directly, never through a shell, so on Windows these are
   plain path characters. **Fix:** on Windows the list no longer has `\`, `(` and `)`; the rest stays, and Unix is
   unchanged.
8. **`install.ps1` could not upgrade while any `utcp` was running.** It stops the daemon first, but `utcp ui` or an
   agent's call in progress still locks `utcp.exe`, so `Copy-Item` failed and the upgrade stopped after stopping the
   daemon. Found when upgrading the VM while a `utcp ui` was open. **Fix:** Windows lets a running program be renamed,
   so the installer renames the old file to `utcp.exe.old-<id>` before copying (and moves it back if the copy fails).
   The next install deletes leftovers that nothing runs any more. Follow-up (0.1.0 release test): the old file was
   kept even when nothing ran it, so every upgrade left a 12 MB copy. The installer now deletes it right after the
   copy; only a file that is still running waits for the next install.

## Tests

- Items 1 to 4 are covered by the suite when it runs on Windows: without fix 1 every integration test fails; the
  unit tests `split_command_line_keeps_quoted_spaces_and_literal_backslashes`,
  `call_tool_names_a_port_nobody_listens_on` (Windows only; Wine behaves like Linux here and fails it) and
  `windows_args_are_quoted_the_way_the_c_runtime_splits_them` cover 2 to 4.
- Item 5 on Unix: `daemon_this_process_may_not_use_is_a_clear_error` makes the socket unreadable (skipped as root).
  On Windows it needs two tokens, so it was checked by hand: elevated daemon with a normal client (clear error from
  `status`, `start`, `stop` and `search`), and normal daemon with an elevated client (works).
- Issue 016 problem 5 (another account's pipe), checked by hand with a temporary account `utcptest` that ran a
  daemon on `\\.\pipe\utcp-ownerprobe`. From an elevated shell the owner check refuses it ("another account
  created it"); from a normal shell the pipe's default permissions deny access and item 5's error appears. Both
  times nothing was sent and no second daemon started. Users lack the batch logon right on Windows 11, so the
  account's daemon was started with `Start-Process -Credential` from the console session, not a scheduled task.
- Item 7: `test_validate_command_accepts_windows_paths` (Windows only), and
  `stdio_mcp_server_serves_concurrent_daemon_calls_and_restarts_after_crash` now starts `node` by its absolute path
  in a folder named `My Tools (x86)` (`My Tools` on Unix).
- Item 6 and the job breakaway: checked by hand on the VM (install from a normal token, upgrade from an elevated
  shell, `-NoDaemonAtLogon`, `-Uninstall`). CI runs the installer too, but its runner is an administrator account,
  so it cannot show these.
