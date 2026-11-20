# Operating `utcp`

How to set up, change, and debug `utcp` on one machine. It is written for the person who runs the machine and for
agents: `utcp sync-ides` copies this file into the `utcp-admin` skill folder, next to its `SKILL.md`, so every agent
has the guide that matches the installed `utcp`.

Commands are shown for a POSIX shell on Linux. On Windows use `utcp.exe` and the Windows paths in the table below.
In PowerShell, single quotes also keep `${VAR}` as written. PowerShell before 7.3 (including the built-in Windows
PowerShell 5.1) drops double quotes inside an argument passed to a program, so there pass JSON arguments on stdin:
`'{"q": 1}' | utcp call <manual.tool> --args-stdin`.

## 1. What Runs Where

`utcp` is one program. It works on its own, and an optional per-user daemon makes it faster and keeps state between
calls. All agents of one user on one machine share the same manuals, secrets and daemon.

| What | Linux | Windows |
| :--- | :--- | :--- |
| Registry (which manuals, their sources, hints) | `~/.config/utcp/.utcp_config.json` | `%APPDATA%\utcp\config\.utcp_config.json` |
| Secrets (`NAME=value` lines) | `~/.config/utcp/.env` | `%APPDATA%\utcp\config\.env` |
| Settings (and the optional secrets command) | `~/.config/utcp/settings.toml` | `%APPDATA%\utcp\config\settings.toml` |
| Saved manuals and search index | `~/.cache/utcp/` | `%LOCALAPPDATA%\utcp\cache\` |
| Daemon socket | `$XDG_RUNTIME_DIR/utcp.sock` | `\\.\pipe\utcp-<USERNAME>` |
| Optional disk log | `~/.local/state/utcp/activity.jsonl` | `%LOCALAPPDATA%\utcp\data\activity.jsonl` |
| Daemon start errors and crashes | `~/.local/state/utcp/daemon.log` | `%LOCALAPPDATA%\utcp\data\daemon.log` |
| Images and files returned by tools | `$XDG_RUNTIME_DIR/utcp-media/` (no `XDG_RUNTIME_DIR`: `utcp-media-$USER/` in `$TMPDIR` or `/tmp`) | `%TEMP%\utcp-media\` |

`UTCP_MEDIA_DIR` or `media_dir` in `settings.toml` moves the media folder. On Linux it must be a folder you own that
only you can open; otherwise utcp leaves images inline in the answer.

`utcp doctor` prints the actual paths, whether the daemon runs, which secrets are set and where each comes from, and
whether the agent skills are current. Start there whenever something looks wrong.

## 2. First Setup

1. Put `utcp` on `PATH`:
   - **Gentoo:** `dev-util/utcp` from the [`adaptive-overlay`](https://github.com/Faraclas/adaptive-overlay)
     overlay installs `/usr/bin/utcp` and the systemd user unit.
   - **Other Linux:** the release archive from the project's GitHub Releases (tags `utcp-rs-v*`),
     `utcp-x86_64-unknown-linux-musl.tar.gz`, holds a static binary that runs on any x86_64 distribution, provided
     as-is; copy `utcp` to `~/.local/bin/`. Or build it with `cargo build --release`.
   - **Windows:** unzip the release and run `install.cmd` (double-click it, or `.\install.cmd` in a terminal), which
     runs `install.ps1`: it copies `utcp.exe` to `%LOCALAPPDATA%\Programs\utcp`, adds that to your user `PATH`, does
     step 2, and does step 3 through a logon scheduled task that also starts the daemon now (never as
     administrator). No admin rights needed. `-NoDaemonAtLogon` skips or removes the task. Running it again
     upgrades; `-Uninstall` removes it and keeps your manuals, secrets and skills.
2. Install the agent skills for Zed, VS Code, Copilot CLI, Antigravity (`agy`) and Hermes:

   ```sh
   utcp sync-ides --global
   ```

3. Start the daemon at login (Linux, `systemd --user`). A package installs the unit; otherwise copy
   `utcp-daemon.service` (in the Linux archive, or `packaging/systemd/` in the source tree) to
   `~/.config/systemd/user/` and point its `ExecStart` at your binary, for example
   `ExecStart=%h/.local/bin/utcp daemon run`. Then:

   ```sh
   systemctl --user daemon-reload
   systemctl --user enable --now utcp-daemon
   ```

4. Give utcp the secrets your manuals need (section 3, Secrets). A daemon started by systemd or at Windows logon
   does not see the variables your shell exports, so if you keep secrets in a shell file, set up a secrets command.

## 3. Adding Tools

`import` and `import-mcp` print `Manual`, `Tools`, and a `Secrets` line naming the variables the manual needs.
`register` prints `Registered '<name>' (N tools, ...)` and, when the manual needs secrets, a `Required credentials`
line. Always give a hint: a few words (at most 60 characters) that appear next to the manual's name in the skill every
agent loads, so agents know what an unclear name such as `context7` is for.

- **OpenAPI 2.0/3.x spec (JSON or YAML) or UTCP manual**, by URL or file:

  ```sh
  utcp register petstore https://petstore3.swagger.io/api/v3/openapi.json --hint 'pet store demo API'
  utcp register myapi ./openapi.yaml --base-url https://api.example.com -H 'Authorization: Bearer ${MYAPI_TOKEN}'
  ```

- **MCP server over HTTP:**

  ```sh
  utcp import-mcp context7 --url https://mcp.context7.com/mcp --register --hint 'library and framework docs'
  ```

- **MCP server started by a command.** `--command` is the program only; repeat `--arg` once per argument. A
  server's README usually shows an `mcpServers` JSON block: `command` becomes `--command`, each `args` entry an
  `--arg`, and each `env` entry an `-e`.

  ```sh
  utcp import-mcp everything --command npx --arg -y --arg @modelcontextprotocol/server-everything --register \
    --hint 'MCP test server'
  ```

  Discovery waits up to 60 s, so a first `npx` or `uvx` run has time to download the package.

- **Write a manual file without registering it** (to review or share it; `utcp register <name> <file>` later). With
  `--register`, `-o` writes a copy next to the registered manual:

  ```sh
  utcp import ./openapi.yaml -o myapi.utcp.json --name myapi
  utcp import-mcp everything --command npx --arg -y --arg @modelcontextprotocol/server-everything -o everything.utcp.json
  ```

- **Check the result:** `utcp list --manual <name>`, then `utcp search "<keywords>"`.

### Secrets

Write `${VAR}` placeholders in `-H` and `-e`, single-quoted so the shell keeps them, never the token itself. The
manual saves the placeholder; the value is looked up at call time. utcp keeps only the variables registered manuals
use and ignores all others. There are three places for a value:

- **`~/.config/utcp/.env`:** `NAME=value` lines. The daemon rereads it when it changes, so no restart is needed.
- **A secrets command** (`[secrets]` in `settings.toml`): a program that prints the values, so they stay where you
  already keep them (a shell file, `pass`, a keyring, a password manager) and are never copied into utcp's files.
  The daemon runs it when it starts, so it has the secrets after a reboot with no one logged in to a shell.
- **Your shell:** a variable exported in a shell reaches the daemon with `utcp daemon reload-env` from that shell,
  and a `utcp call` from a shell that has a variable the daemon lacks hands it over too. Both last until the daemon
  stops.

If a name has values in several places, the secrets command wins, then your shell, then `.env` and the daemon's own
environment. If a call or import fails because a variable is missing, the error names it. `utcp doctor` shows each
variable as `[SET]` (with where it came from) or `[MISSING]`.

Agents should ask the user to add a secret and should never ask for its value in the chat.

#### The Secrets Command

```toml
# ~/.config/utcp/settings.toml
[secrets]
command = ["/bin/bash", "~/scripts/utcp-secret-source.sh"]
timeout_secs = 10   # optional; it is stopped after this
```

- `command` is the program and its arguments, one list item each. It runs directly, not through a shell; a leading
  `~` is your home folder.
- utcp adds the names it wants as further arguments: every name a registered manual may read.
- It reads `NAME=value` lines from the program's stdout and keeps only the names it asked for. A value is taken as
  written up to the end of the line (no `$` expansion); a leading `export` and one pair of quotes around the value
  are removed. Blank lines and `#` comments are skipped.
- The program's stderr is discarded, and no value is ever shown or logged. If it fails, `utcp doctor` and
  `daemon.log` say how (exit code, timeout, a line that is not `NAME=value`); run it by hand to see its own errors.
- **When it runs:** the daemon runs it at start, when its config folder changes (`register`, `deregister`, an edit
  to `settings.toml`) and on `utcp daemon reload-env`, so after you change a secret, run `reload-env`. If a run
  fails, the daemon keeps the values of the last good run. Without the daemon, each `utcp` command runs it when it
  needs a secret (at most once a minute).

Two examples to copy and adjust ship in the release archives and in `examples/` in the source:

- `secret-source.sh` (Linux) sources a shell file such as `~/.bash_secrets` and prints only the names on its
  allowlist, so that file's other secrets never reach utcp.
- `secret-source.ps1` (Windows) reads each value from the PowerShell SecretManagement module.

The program prints secrets to its stdout, so do not run it in a terminal to test it: run `utcp doctor`, which names
the variables it printed.

### MCP Servers in Containers

Many MCP servers ship as container images. That lets you try one without installing its runtime. Use `podman` or
`docker` as the command:

```sh
podman pull ghcr.io/github/github-mcp-server
utcp import-mcp github-mcp --command podman \
  --arg run --arg -i --arg --rm --arg --init \
  --arg -e --arg GITHUB_PERSONAL_ACCESS_TOKEN \
  --arg ghcr.io/github/github-mcp-server \
  -e 'GITHUB_PERSONAL_ACCESS_TOKEN=${GITHUB_TOKEN}' \
  --register --hint 'GitHub MCP server (container)'
```

- **Pull first.** Discovery gives up after 60 s, and a first pull can take longer.
- **`-i` is required**: the server talks over stdin and stdout.
- **`--init`** makes the container stop when `utcp` stops it. Without it, a server that ignores `SIGTERM` keeps the
  container running.
- **Secrets:** `-e` sets the variable for the `podman` process; the bare `--arg -e --arg NAME` passes it into the
  container. The manual still stores only the placeholder.
- **Every use starts a container.** With the daemon running, the first call starts it and later calls reuse it until
  the daemon stops or you stop the server. Without the daemon, every single `utcp call` starts and stops its own
  container, which takes seconds. Run the daemon for container servers.

## 4. Changing and Removing Manuals

```sh
utcp refresh [<name>]               # fetch the manual again from its source, or ask its MCP server again
                                    # (picks up new endpoints or tools)
utcp set-hint <name> '<few words>'  # add or change a hint; --clear removes it
utcp get-hint [<name>]              # show hints
utcp deregister <name>              # take it out of search and calls; settings, hint and saved copy are kept
utcp register <name>                # bring a deregistered manual back, offline, with its old settings
utcp deregister <name> --purge-cache  # delete it for good (adding it back needs the original source)
```

Registering, deregistering and hint changes reload the daemon and rewrite the installed account-wide (`--global`)
agent skills, whose description lists the registered manuals. Agents see the new list in their next session. Project
copies made with `utcp sync-ides --workspace <dir>` are not rewritten; run that command again.

## 5. The Daemon

Without the daemon, every `utcp` command reads the saved files itself. That works, but each call sets up its own
connections, and each call to a command-started MCP server starts and stops that server. The daemon keeps the
catalog in memory, keeps HTTP connections and MCP servers warm, and keeps the last 500 calls for `utcp log recent`.

```sh
utcp daemon status        # running?, PID, manuals, calls served
utcp daemon start         # start it in the background (through systemd when the unit is installed)
utcp daemon stop
utcp daemon restart       # needed after installing a new utcp binary
utcp daemon reload-env    # send this shell's variables (only those manuals use) to the daemon, reread its
                          # environment and .env, and run the secrets command again
```

With the systemd unit, `systemctl --user restart utcp-daemon` does the same as `utcp daemon restart`.

Changes to the registry, saved manuals, `settings.toml` and `.env` reach a running daemon on their own. Restart it
only for a new `utcp` binary or to clear everything at once. A restart stops all of its MCP servers, so calls in
progress fail.

If the daemon fails to start or crashes, the reason is in `daemon.log` in the state folder (`~/.local/state/utcp/` on
Linux, `%LOCALAPPDATA%\utcp\data\` on Windows; `utcp doctor` prints the folder). `utcp daemon start` shows what it
logged during that attempt, and `utcp daemon status` names the file when the daemon is stopped. The file also gets
what the daemon finds wrong when it starts (an invalid `settings.toml`, a failed secrets command, required
variables that are still not set) and a secrets command that starts or stops failing later. It never gets calls
or secret values, and costs nothing while the daemon works. Under systemd the journal has the same lines
(`journalctl --user -u utcp-daemon`).

## 6. Local MCP Servers

A manual imported with `import-mcp --command` runs a local server. The daemon starts it on the first call and shares
it between all agents on the machine:

- each call gets its own answer, even when many agents call at once;
- a call with no answer after 60 s fails with `did not answer 'tools/call' within 60s`;
- a crashed server starts again on the next call.

```sh
utcp daemon servers                  # state, PID, uptime, restart count of each server
utcp daemon restart-server <name>    # stop it and start it again now
utcp daemon stop-server <name>       # stop it now; the next call starts it again
```

Restarting or stopping a server makes the calls waiting on it fail at once with `was stopped on request`, including
other agents' calls. To keep a server off, deregister its manual (agents: ask the user first).

The server's own error output is not kept. If a server fails to start, run its command by hand in a terminal (for
example `npx -y @modelcontextprotocol/server-everything`) to see why.

## 7. When Calls Fail

1. **See what happened.** The daemon keeps the last 500 calls in memory, with the full error text of failures
   (secrets replaced by `***REDACTED:<NAME>***`). This works with disk logging off:

   ```sh
   utcp log recent --failed
   utcp log recent --limit 50 --json
   ```

   Without a daemon, `log recent` reads the disk log, which exists only if logging was turned on.

2. **Look up the error:**

   | Error says | Meaning | Fix |
   | :--- | :--- | :--- |
   | `rate limited by <host> (HTTP 429)` or `(HTTP 403)` ... `retry after N s` | The API's limit is used up. All agents using the same token share it. | Wait the time it names. Retrying sooner fails again. |
   | `these placeholders were not set: NAME` | A `${NAME}` the manual needs has no value. | Give it a value (section 3, Secrets): `.env`, the secrets command, or export it and run `utcp daemon reload-env`. |
   | `Secrets command failed: ...` (`reload-env`, `doctor`, `daemon.log`) | The `[secrets]` command did not run cleanly; the daemon keeps the values of its last good run. | Run the command by hand to see its own error output (it prints secrets, so not where others can see), fix it, then `utcp daemon reload-env`. |
   | `Tool 'x.y' not found` | No such manual or tool is registered. | `utcp search` or `utcp list` for the right name. |
   | `MCP server 'x' did not answer ... within 60s` | The local server is stuck or very slow. | `utcp daemon restart-server x`. |
   | `MCP server 'x' was stopped on request` | Someone ran `stop-server` or `restart-server` during the call. | Call again. |
   | `utcp daemon exited before listening` or `did not start listening within N seconds` | `utcp daemon start` failed. | Read the lines it quotes from `daemon.log`; `utcp daemon status` names the file. |
   | `utcp daemon is not running` or `is not currently running` | From `daemon servers`, `stop-server`, `restart-server` and `reload-env` (`daemon stop` prints it as information). | `utcp daemon start`. Calls work without it. |
   | `Cannot use the utcp daemon at ...: access denied` (Windows) | The daemon was started from an administrator (elevated) shell, so normal shells may not use it. | `utcp daemon stop` in an elevated shell, then `utcp daemon start` in a normal one. `utcp --direct` works meanwhile. |
   | `Cannot use the utcp daemon at ...: permission denied` (Linux) | Another account started it, for example root through `sudo`. | Stop it as that account, start it as yourself. |

3. **Narrow it down:**
   - `utcp call <manual.tool> '<json>' --dry-run` shows the request it would send (URL, method, headers, with
     secrets redacted) without sending it.
   - `utcp --direct call ...` skips the daemon. If that works and the normal call fails, the daemon has stale
     state: `utcp daemon reload-env` for secrets, `utcp daemon restart` for everything else.
   - `utcp doctor` shows `[MISSING]` for every secret a manual needs and cannot find, and where each `[SET]` one comes
     from. With the daemon running it shows the daemon's view, which is what calls use.

4. **Disk logging, only while debugging.** It is off by default and costs nothing while off. `utcp call ... --log`
   logs one command; `utcp log on` logs everything until `utcp log off`. The log keeps about 10,000 calls in two
   files of at most 1 MB. `utcp log status` shows its size, `utcp log clear` deletes it.

## 8. Agent Skills

`utcp sync-ides --global` writes two skills for each agent client:

- `utcp-tools`: finding and calling tools. Its description lists the registered manuals and their hints.
- `utcp-admin`: changing the setup, with this guide next to it as `operating.md`.

| Client | Skill folder (Linux; Windows uses `%USERPROFILE%` for `~`) |
| :--- | :--- |
| Zed (Copilot CLI also reads it) | `~/.agents/skills/` |
| VS Code (Copilot Chat), Copilot CLI | `~/.copilot/skills/` |
| Antigravity (`agy`) | `~/.gemini/antigravity-cli/skills/` |
| Hermes (only if `~/.hermes/skills` exists) | `~/.hermes/skills/software-development/` |
| One project only (Zed, Copilot CLI, `agy`) | `<project>/.agents/skills/` (`utcp sync-ides --workspace <dir>`) |

VS Code's Copilot Chat does not read `~/.agents/skills/`, so it uses the account-wide copy in `~/.copilot/skills/`.
For a project, its documented folder is `.github/skills/`, which utcp does not write. A `.agents/skills/` at the
workspace root appears to load in VS Code too, but the VS Code docs do not list it. Copilot CLI reads both account
folders and lists a skill found in both only once.

`utcp doctor` or `utcp sync-ides --status` shows each copy as `UP TO DATE`, `OUTDATED` or `NOT INSTALLED`. After
installing a new `utcp`, run `utcp sync-ides --global` again so the skills match it.

## 9. Working on `utcp-rs` Itself

The source is at <https://github.com/Faraclas/agent-misc> in the `utcp-rs/` folder.

```sh
cargo test --workspace            # offline tests; needs Node.js for the mock MCP servers
python3 tests/load/hammer.py      # on-demand load test: many agents calling one daemon at once
```

Before changing code, read `utcp-rs/AGENTS.md`: a change to a command, flag, path or behavior must update the
tests, the skill templates, the spec, the README and this guide in the same commit.
