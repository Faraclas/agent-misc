# Issue 019: The Daemon Has No Secrets After a Reboot, and `doctor` Shows the Wrong Ones

- **Upstream Crate:** none; local to `utcp-rs`
- **Local Files:** `crates/utcp-core/src/secrets.rs` (new), `crates/utcp-core/src/paths.rs`,
  `crates/utcp-core/src/catalog.rs`, `crates/utcp-core/src/ipc.rs`, `crates/utcp-core/src/daemon_log.rs`,
  `crates/utcp-cli/src/main.rs`, `crates/utcp-cli/src/daemon.rs`, `crates/utcp-cli/src/tui/mod.rs`,
  `examples/secret-source.sh`, `examples/secret-source.ps1`
- **Local Status:** Fixed in `utcp-rs` 0.1.1; the Windows example's case-insensitive allowlist (below) fixed after
  0.1.1
- **Upstream Issue / PR:** Not applicable

## How It Was Found

After installing 0.1.0 from the Gentoo package, `systemd --user` started the daemon at login. The user keeps tokens
in a shell file that `~/.bashrc` sources. systemd never reads `.bashrc`, so the daemon started without
`GITHUB_TOKEN`.

## Problems

1. **No secrets after a reboot.** The only ways in were `.env` (a second copy of every secret, which the user
   rejected), `utcp daemon reload-env` from a shell after every reboot (also rejected), or a caller's shell handing
   the variable over on its first call. Agents' calls worked only because of the last one, which hid the problem.
2. **`utcp doctor` checked the wrong process.** It resolved credentials from the shell it ran in, so it said `[SET]`
   while the daemon, which makes the calls, did not have the value.
3. **Nothing said so.** The daemon logged nothing about missing variables at startup, and an invalid
   `settings.toml` was silently replaced by the defaults.

## Fix

1. **Secrets command** (`[secrets]` in `settings.toml`, the same model as the Hermes gateway's `secrets.command`):
   `command` is a list (no shell; a leading `~` is the home folder). utcp appends the names registered manuals may
   read and keeps the wanted, non-empty `NAME=value` lines from stdout (values literal, no `$` expansion; a leading
   `export` and one pair of quotes allowed). stdin is closed, stderr discarded, `timeout_secs` (default 10)
   enforced, output capped at 1 MiB; errors never contain output.
   - The daemon runs it at startup (before it listens), on every reload of its config folder and on `reload-env`,
     outside the catalog lock. A failed run keeps the last good values. Its values win over a caller's shell, which
     wins over the startup environment and `.env`.
   - Without the daemon, a process runs it when it needs secrets, at most once a minute for the same names
     (`call` asks only for that manual's names; `register` and `import-mcp` add the placeholders of the manual being
     registered).
   - `utcp daemon start` waits 3 s plus `timeout_secs` when a command is set.
   - Example commands to copy: `examples/secret-source.sh` (sources a shell file, prints an allowlist) and
     `examples/secret-source.ps1` (PowerShell SecretManagement). Both ship in the release archives.
2. **`doctor` shows the daemon's view** when the daemon runs ("as the running daemon sees them"), with the source of
   each value: `secrets command`, `caller shell`, `daemon environment` (or `environment` without a daemon). It also
   shows the secrets command's last run and an invalid `settings.toml`. `daemon status`, `reload-env` and the TUI's
   Daemon Monitor (new Source column and Secrets Command line) show the command's result too.
3. **Startup warnings:** the daemon writes an invalid `settings.toml`, a failed secrets command and every required
   variable still unset (with its manual) to stderr (the journal) and `daemon.log`. A secrets command that starts or
   stops failing later is logged once, not on every reload.

## Tests

- `secrets.rs` unit tests: literal parsing and unwanted names dropped, errors without content, empty and missing
  programs, `~` expansion, a real run through the Node fixture `tests/fixtures/secret_source.cjs` (it always prints
  an unrequested `UNASKED_SECRET`), failures that never show stdout or stderr, the timeout, and the direct-mode
  cache.
- `secrets_command_supplies_credentials_to_the_daemon_and_direct_mode` (integration): direct mode, a daemon started
  without the variable in its environment, only allowed names held and asked for, `reload-env` rotating a value, a
  failing run keeping the last good value, a daemon that still starts with a failing command (`doctor` and
  `daemon.log` say why), and `doctor` reporting an invalid `settings.toml`.
- `spec_fetch_resolves_header_placeholders` step 5: `register` asks the command for the placeholders of a manual not
  yet in the index.

## Follow-Up: Windows Example Matched Names in Any Case

Found on a Windows test machine after 0.1.1: `doctor` said the command "printed 2 of the wanted names" for a single
`github` manual that needs only `GITHUB_TOKEN`. utcp also asks for alias spellings of each variable (here
`github_TOKEN`, from `secret_var_candidates`). `examples/secret-source.ps1` checked its allowlist with `-notcontains`,
which ignores case, so `github_TOKEN` passed as `GITHUB_TOKEN`; Windows environment lookups ignore case too, so the
value was printed under both names. Harmless for one manual, but a different manual whose variable is a case variant
of an allowlisted name would receive that secret, which defeats the allowlist. The bash example compares exactly and
was not affected.

**Fix:** the example uses `-cnotcontains` (exact case). Checked in Windows PowerShell 5.1 and PowerShell 7: only the
exact name passes, `github_TOKEN` and `Github_Token` are skipped, and the script still parses. On the test machine
the count dropped to 1 and `github` calls still authenticated.
