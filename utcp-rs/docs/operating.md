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
| Staged manuals, and the versions `promote` replaced | `~/.cache/utcp/manuals/staged/`, `.../previous/` | `%LOCALAPPDATA%\utcp\cache\manuals\staged\`, `...\previous\` |
| Daemon socket | `$XDG_RUNTIME_DIR/utcp.sock` | `\\.\pipe\utcp-<USERNAME>` |
| Optional disk log | `~/.local/state/utcp/activity.jsonl` | `%LOCALAPPDATA%\utcp\data\activity.jsonl` |
| Daemon start errors and crashes | `~/.local/state/utcp/daemon.log` | `%LOCALAPPDATA%\utcp\data\daemon.log` |
| Images and audio from MCP servers | `$XDG_RUNTIME_DIR/utcp-media/` (no `XDG_RUNTIME_DIR`: `utcp-media-$USER/` in `$TMPDIR` or `/tmp`) | `%TEMP%\utcp-media\` |
| Files tools answer with (no `--out`) | your Downloads folder (`~/Downloads`, from `user-dirs.dirs`) | your Downloads folder |
| Answers too large to print, and downloads when there is no Downloads folder (deleted after a day) | `~/.cache/utcp/downloads/` | `%LOCALAPPDATA%\utcp\cache\downloads\` |

`UTCP_MEDIA_DIR` or `media_dir` in `settings.toml` moves the media folder. On Linux it must be a folder you own that
only you can open; otherwise utcp leaves images inline in the answer.

`UTCP_CONFIG_DIR` or `UTCP_CACHE_DIR` (or `--config`, `UTCP_CONFIG`) makes a separate setup, for example for tests;
set `UTCP_STATE_DIR` too to keep its logs apart. It has its own manuals and its own daemon endpoint (`utcp.sock` in
its cache folder; on Windows a pipe name with a hash of that folder), so it never uses or changes the normal setup's
daemon. `UTCP_SOCKET_PATH` sets the endpoint explicitly. It does not rewrite the agent skills the normal setup
installed, and `utcp daemon start`, `stop` and `restart` never touch the systemd unit for it.

A separate config without `UTCP_CACHE_DIR` gets its own cache folder, `setups/<hash>/` in the normal one
(`~/.cache/utcp/setups/<hash>/`), where the hash comes from the config file's path. So `--config` alone never shares
saved manuals or the search index with the normal setup, and the same config file always finds its cache again.
`utcp --config <path> doctor` prints that folder.

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
5. Check the setup:

   ```sh
   utcp doctor
   ```

   It always shows a `Secrets Command` line (`OK`, `FAILED`, or `not set`) and a `Secrets File` line (the `.env`
   path, and whether it exists), the settings file with any setting in it that has no effect, the upload and
   download limits and the downloads folder, then each secret your manuals need as `[SET]` or `[MISSING]`.

   In a terminal, `SET` and `UP TO DATE` are green, and `utcp list` and `utcp diff` show tool and manual names in
   blue. Set `NO_COLOR=1` to turn color off. Pipes and agents always get plain text.

### Settings (`settings.toml`)

The first `utcp` command (other than `utcp daemon run`, which systemd starts) creates `settings.toml` (path in the
table above) with every setting and a comment saying what it does. Settings with a good default are active lines; the
others are commented out (`#`) with an example value. A commented line is off, and utcp uses its built-in behavior
for it.

- To change a setting, remove the `#` and set your value. To undo, put the `#` back. Don't delete lines: utcp adds a
  missing setting back.
- When a newer utcp knows more settings, it adds each one that is missing (neither active nor commented) in its
  template state, keeps a copy of the old file as `settings.toml.bak`, and says what it added. It never changes a
  line that is there, even when a default changes.
- A commented section header (`# [secrets]`) turns the whole section off.
- An invalid file is left alone; utcp then uses its built-in settings and says so (`utcp doctor`, `daemon.log`).
- `utcp doctor` names any setting that has no effect, for example a misspelled one, or a key that ended up in the
  wrong section because its header was commented out.

| Setting | Template | What it does |
| :--- | :--- | :--- |
| `cache_on_register` | `true` | Keep a copy of a manual's tools when it is registered (section 3). |
| `daemon_idle_timeout_mins` | `0` | Stop the daemon after this many idle minutes; `0` keeps it running. |
| `max_upload_mb` | `100` | Largest file a tool may upload, in MB. `0` turns uploads off; commented out, no limit. |
| `max_download_mb` | `# 500` | Largest file a tool may download, in MB. `0` turns downloads off; commented out, no limit. |
| `download_dir` | `# "~/Downloads"` | Where file answers go without `--out`. Commented out: your Downloads folder. |
| `media_dir` | `# <the default>` | Where images and audio from MCP servers go. `UTCP_MEDIA_DIR` overrides it. |
| `max_media_files` | `64` | How many of those media files to keep. |
| `media_ttl_mins` | `60` | Minutes to keep each media file. |
| `[secrets]` `command` | `# ["/bin/bash", "~/scripts/..."]` | The secrets command (section 3). |
| `[secrets]` `timeout_secs` | `# 10` | Seconds it may run. |

The TUI's `[c]` key changes `cache_on_register` by rewriting only that line.

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

  On Windows a bare name such as `npx` or `az` is looked up on `PATH` with the extensions in `PATHEXT`, as a shell
  does, so `npx` finds `npx.cmd`. If the manual sets its own `PATH` with `-e`, that `PATH` is searched. The same
  lookup applies to `cli` tools and to the secrets command.

- **Write a manual file without registering it** (to review or share it; `utcp register <name> <file>` later). With
  `--register`, `-o` writes a copy next to the registered manual:

  ```sh
  utcp import ./openapi.yaml -o myapi.utcp.json --name myapi
  utcp import-mcp everything --command npx --arg -y --arg @modelcontextprotocol/server-everything -o everything.utcp.json
  ```

- **Check the result:** `utcp list <name>`, then `utcp search "<keywords>"`. Search matches whole words, as written
  (`get_repo`, `getRepo` and `get-repo` are all `get repo`), with no word lists. A manual's own text links short and
  long forms: when `get_repo` says "Get a repository.", `repository` finds that manual's `repo` tools. It ranks a tool
  higher the more of its name and first sentence the query covers, and on equal scores puts tools that only read
  first (HTTP `GET` or `HEAD`, or MCP tools whose server marks them `readOnlyHint`).

### Describing Tools So Agents Find Them

For anyone who writes or generates manuals. Search has no word lists and no synonyms: an agent finds a tool only
through the words its manual uses. Write those words for people, not for one search engine:

- **Name:** a verb and the thing it acts on, in the API's own words (`get_repo`, `list_issues`). Agents remember
  names, so treat them as an interface and rename only on purpose.
- **First sentence:** what the tool does, short and plain ("Get a repository."). `utcp list` and `utcp search`
  show only this sentence, and search weighs it just below the name. Never a developer's note, a URL or an empty
  "Settings: Get".
- **Spell out short forms once:** a name may say `repo` when its first sentence says "repository"; search then
  treats the two as one word for that manual.
- **Add the words people use that the API's text lacks,** as a plain sentence at the end: the account a token
  belongs to is "who am I, my user, the current user, me"; creating an issue is also "opening" one. A sentence, not
  a list of keywords.
- **Mark tools that only read:** HTTP `GET`, or `readOnlyHint` in an MCP server's tool annotations.
- **Tags and the manual's hint:** a few words for the area (`issues`, `library docs`).

To see what an agent will find, stage the manual and try the searches an agent would type:
`utcp search --staged '<words>'`. There is no list to pass; a miss means either the manual does not say what people
call the thing, or a ranking bug to report.

### Staging: Try a Manual Before Agents See It

`register` makes a manual active at once: agents find it, and the skills list it. To review and test a manual first,
for example one made by a generator such as `azdo-manuals`, or a new version of one you already use, stage it:

```sh
utcp stage <name> <url_or_file_path> --hint '<few words>'   # same source, --base-url and -H as register
utcp diff [<name>...]                     # new manual, or what changes: tools added (+), removed (-), changed (~)
utcp diff <manual>.<tool>...              # one changed tool: old (-) against new (+) for each part that changed
utcp list [<name>] --staged               # the staged manuals (or one manual's tools)
utcp search --staged '<keywords>'         # will agents find it? active and staged manuals together
utcp call --staged <manual.tool> '<json>' # call one (also schema --staged, batch --staged)
utcp promote <name>... [--no-keep]        # make them active (--all: everything staged; --no-keep: no rollback)
utcp discard <name>...                    # drop them instead (--all: everything staged)
utcp rollback <name>                      # back to the version the last promote replaced; again to undo
utcp rollback --forget <name>...          # drop kept versions you would never go back to (active ones unchanged)
```

- `diff` says what changed in each changed tool: `~ get_user: description; inputs added: since`. To review a
  change, `utcp diff github_repos.get_content` prints each changed part of that tool (`description`, `input <name>`,
  `method`, `url`, `auth`, `headers`) with the old value after `-` and the new one after `+` (`--json`: a list of
  `{manual, tool, status, parts: [{part, old, new}]}`). A long value (over 120 characters, or several lines, such as
  most descriptions) shows as one word diff instead, `[-removed-]{+added+}` (red and green in a terminal), with the
  unchanged text far from a change cut to `...`. Secrets never show: `${VAR}` placeholders stay, other header and
  auth values print as `***`. Names are either manuals or `<manual>.<tool>`, not both. `search --staged` ranks a
  staged manual against the active ones (it replaces its own active version) and marks its results `[staged]`, so
  you can check that the questions agents ask still find the right tool.
- Without `--staged`, a staged manual is not in `search` or the skills, `call` does not reach it, and `list` shows it
  only under Staged.
  `call --staged` works through the daemon (or `--direct`) with the same secrets as active manuals: the secrets
  command is also asked for the names staged manuals read, and `utcp doctor` lists their secrets, marked `(staged)`.
- There is one staging area, with at most one staged copy per name; staging a name again replaces its staged copy.
  `doctor` shows how many manuals are staged.
- `diff`, `promote`, `discard`, `rollback --forget` and `refresh` take several names, and a name ending in `*` takes
  every manual it starts: `utcp promote 'azdo_*'` (quote it for the shell). A name or pattern that matches nothing is
  an error and nothing changes. `diff` ends with the promote and discard commands for exactly the manuals it showed.
- `promote` is the only step that changes what agents see: it rebuilds search, reloads the daemon and rewrites the
  installed skills. It keeps the version it replaced (one per name), which `rollback` brings back.
- Rollback is for going back to the last known-good version after a promote turned out wrong. Use `promote --no-keep`
  when the replaced version was only a test or pre-release: it keeps nothing and also drops an older kept version of
  that name, so `rollback` never brings back something older. Use `rollback --forget` to drop a kept version you
  would never go back to: it deletes only utcp's own copy in the cache's `manuals/previous/` folder and leaves the
  active manual, search and the skills as they are.
- Without `--hint`, a staged manual keeps the hint of the manual it replaces.

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
- utcp adds the names it wants as further arguments: every name a registered or staged manual may read.
- It reads `NAME=value` lines from the program's stdout and keeps only the names it asked for. A value is taken as
  written up to the end of the line (no `$` expansion); a leading `export` and one pair of quotes around the value
  are removed. Blank lines and `#` comments are skipped.
- The program's stderr is discarded, and no value is ever shown or logged. If it fails, `utcp doctor` and
  `daemon.log` say how (exit code, timeout, a line that is not `NAME=value`); run it by hand to see its own errors.
- **When it runs:** the daemon runs it at start, when its config folder changes (`register`, `stage`, `deregister`, an
  edit to `settings.toml`) and on `utcp daemon reload-env`, so after you change a secret, run `reload-env`. If a run
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

### Files Tools Send and Receive

The `utcp` process that makes a call reads the files a tool uploads and writes the files it downloads, with the
permissions of whoever runs it (an agent in a sandbox stays in its sandbox). The daemon only passes the bytes along;
it never opens your files.

- **Downloads.** An answer that is not JSON or text (an archive, an image, a PDF), one the server sends as an
  attachment (`Content-Disposition: attachment`), or one the manual marks as a file, is saved instead of printed,
  and the call prints `{"type": "file", "path", "mime_type", "bytes", "filename"}`. `mime_type` is the media type
  only, lowercased and without parameters: a server's `application/zip; api-version=7.1` is reported as
  `application/zip`. Without `--out` it goes to `download_dir`, else your Downloads folder, else
  `~/.cache/utcp/downloads/`, under the server's file name (or `<tool>.<ext>`), adding a number (`a (1).zip`) instead of
  overwriting. `--out <path>` saves it there and overwrites, like `curl -o` and `cp` (into a folder: the server's
  name); `--out -` writes the bytes to stdout. A file arrives as a `.part` file that is renamed when complete and
  deleted if the transfer fails. Each batch item can have its own `"out"`.
- **Uploads.** A tool that uploads takes `{"file": "<path>"}` in its body argument, optionally with `"offset"` and
  `"length"` in bytes to send one part (chunked uploads). A relative path is relative to the caller's folder.
- **Answers too large to print.** A JSON or text answer over 10 MB (after `--where` and `--fields`) is saved to
  `~/.cache/utcp/downloads/` and the call prints its reference with a `note`. Files there are deleted after a day.
  With `--out`, a JSON answer goes to that file; if the file can't be written, the answer prints instead, with a
  note, since the call itself succeeded and may have been a write.
- **Limits.** `max_upload_mb` and `max_download_mb` (Settings, section 2). A refused transfer says it failed only
  because of the limit, names the setting and the file, and keeps nothing. JSON answers are never limited.
- **Slow transfers.** A call fails only when the server sends nothing for 60 s (`did not answer for 60 s`, or `sent
  no data for 60 s in the middle of its answer`). A transfer that keeps moving can take as long as it needs.
- In a manual, an HTTP tool's `tool_call_template` decides where the arguments go. Path parameters fill the URL,
  `header_fields` become headers, and the rest are query parameters, except for the body:
  - POST, PUT and PATCH send a body. With `body_field`, that argument is the body and the others stay query
    parameters; without it, all arguments form one JSON object.
  - DELETE sends a body only when the template sets `body_field` and the call passes that argument (as for POST).
    Otherwise every argument is a query parameter. GET and HEAD never send a body.
  - `"body_type"` says what the body is: `"json"` (the default), `"text"` (the `body_field` argument, a string,
    sent as is; the `Content-Type` header sets the type, default `text/plain; charset=utf-8`) or `"file"` (an
    upload: `body_field` names the argument and the `Content-Type` header sets the type). An upload needs POST, PUT
    or PATCH, and a text body POST, PUT, PATCH or DELETE. Calling a tool that breaks these rules, or has another
    `body_type`, fails with an error that says to fix the manual, instead of sending the request without its body.
  - `"response_type": "file"` forces a file answer.
  - A URL placeholder the tool's schema does not require (Azure DevOps `project` and `team`, a build's
    `timelineId`) can be left out or passed as `""`: its whole path segment is then left out of the URL. A required
    one may not be empty, since that would call another URL.
  - A list of plain values for a query parameter is sent once, joined with commas (`"ids": [1, 2]` is `ids=1,2`),
    also when the schema declares it a string, as Azure DevOps and GitHub declare their list filters
    (`"definitions": [13766, 42]`).
- Before sending, `call` and every `batch` item check the arguments against the tool's input schema, inside a JSON
  body too, and say `Nothing was sent` when something is wrong (section 7). Where the fix is certain, they fix it
  instead: an argument whose name differs only in case from the schema's (`Fields`) is renamed, a value from a
  tool's list of values gets the list's spelling (`Closed` is `closed`), and `"30"` for a number or boolean input
  is sent as `30`. A body key the schema doesn't declare and that is not a near-typo of one it does is sent anyway
  (a body is often an answer read back, with keys the schema leaves out), and a note names it.

### Long Lists (Paging)

APIs return long lists one page at a time and say in a response header that there is more. utcp prints such a page
as `{"result": <answer>, "next_page": {...}}`; calling the same tool again with the same arguments plus the ones in
`next_page` gets the next page. The last page prints the answer alone, as before. `--where`, `--fields` and `--out`
apply to `result`, so a page whose items all fail `--where` still has `next_page` (and a note on stderr), and a
batch item has `next_page` next to its `result`.

| Header | `next_page` |
| --- | --- |
| `x-ms-continuationtoken` (Azure DevOps) | `{"continuationToken": "<token>"}`, or `{"x-ms-continuationtoken": "<token>"}` when the tool's `header_fields` declares that header |
| `Link` with `rel="next"` (GitHub and other RFC 8288 APIs) | the query parameters of the next URL that differ from the ones this call sent, such as `{"page": "2"}` or a cursor `{"after": "..."}` |

An API that pages another way (a token in the body, such as `nextLink`) is printed unchanged; its own answer says
how to ask for more. File answers ignore paging headers.

### Smaller JSON Answers (`--fields`, `--where`)

`--fields a,b.c` keeps only those keys; dotted paths pass through lists. Each path is resolved on its own: one that
names a top-level key keeps that key, and for a wrapped list such as `{"count": N, "value": [...]}` the item keys
alone are enough, so `--fields count,id,name` keeps `count` and each item's `id` and `name`. A key that itself has
dots, as in an Azure DevOps work item's `fields`, is one name: `--fields fields.System.State` and
`--where 'fields.System.State=Active'` work (the longest key that matches wins). A path part that matches no key
exactly matches the one key that differs only in case (`Commit.Author.Name` reads `commit.author.name`). `--where`
keeps only the list items that match, before `--fields`. Use the tool's own filter arguments first (see `schema`):
they make the server send less, while `--where` filters what came back.

```sh
utcp call <tool> '{...}' --where 'records.result=failed|canceled' --fields records.name
utcp call <tool> '{...}' --where 'state!=closed' --where 'labels.name=bug'   # every condition must hold
utcp call <tool> '{...}' --where 'records.name~=sign' --where 'name!^=tmp-'
utcp call <tool> '{...}' --where 'completed_at>=2026-11-24' --where 'run_attempt>1'
```

- `path=a|b` keeps the items whose `path` is `a` or `b`, `path~=a|b` those containing `a` or `b`, `path^=a|b`
  those starting with one. A `!` before the operator (`!=`, `!~=`, `!^=`) keeps the others. Quote the condition,
  since the shell reads `|`, `!`, `<` and `>`. Matching ignores case in values and paths and compares text, so `null`
  and `true` work; with `=`, numbers compare as numbers (`RemainingWork=0` matches the `0.0` Azure DevOps sends).
- `path>v`, `path>=v`, `path<v` and `path<=v` compare numbers or ISO dates, with one value: `'changes>=200'`,
  `'completed_at<2026-11-24T10:00:00Z'` (a time without a zone is UTC). A date alone compares the field's day in UTC,
  so `'closedDate<=2026-11-24'` includes that whole day, and `'completed_at=2026-11-24'` (or `!=`) means any time
  on that day. Numbers sent as strings count. An item whose field is not
  a number (or not a date) is left out, and a note counts those. There is no `!` for these: the opposite of `>` is
  `<=`. For a range, give two conditions.
- A missing field counts as `null`: `pull_request=null` keeps the items without one, `pull_request!=null` those
  with one. An object never matches; name a field inside it (`user.login=octocat`).
- `\|` is a `|` inside a value (`\\|` inside batch JSON). A comma is an ordinary character, so matrix job names
  need no escape (`'name=test (ubuntu-latest, stable)'`). An empty value is refused for `~=` and `^=`, since it
  would match everything (a stray `|`); `path=` matches the empty string. `==` and empty path parts
  (`labels..name`) are errors.
- The list: the root array; a top-level list the path starts with (`records.result`); the `value`, `items`,
  `results` or `data` list; or the answer's only list of objects. An empty list counts only on an empty page, where
  every other top-level value is plain (`{"total_count": 0, "workflow_runs": []}`); a repository's empty `topics`
  does not make it a list.
- When `--where` can't apply to the answer, the call still succeeds (exit 0): it was already sent, and may have been
  a write, so calling again could repeat it. The whole answer prints unfiltered (`--fields` still applies), with a
  note on stderr: `note: the call succeeded; --where was not applied, so no items were left out: <reason>`. The
  reasons, whatever the data: a path that starts with another top-level field of the answer (`tag_name` on a
  release), since that field is the answer's own, not its items'; a path that is only a list's name (`topics=rust`;
  write `value.value=x` for the items' own `value` field); a list of plain values, such as log lines or `topics`;
  an answer with several lists and no prefix, where the reason names the lists whose items have the path's first
  field (`files.filename` on a compare); an answer with no list at all (one object); and an answer that is not JSON.
  A file answer is saved as usual, with a note that `--where` and `--fields` were not applied. Mistakes in the
  condition itself (`==`, a missing operator, `x>abc`) are errors before anything is sent.
- A path through a list (`labels.name`) matches when any of its elements does; the inner list stays whole (trim it
  with `--fields`). So `jobs.steps.conclusion=failure` keeps the jobs with any failed step, with all their steps.
- Conditions on different lists each filter their own: `--where 'files.filename^=src/' --where 'commits.sha^=edc'`
  on a compare keeps the matching files and the matching commits.
- Everything else in the answer stays as the server sent it: a `count` or `total_count` still counts the
  unfiltered items.
- When a condition keeps none of a non-empty list, `call` prints a note on stderr naming the values that field has
  on this page (at most 10), such as `note: --where 'conclusion=failed' kept none of the 4 items in jobs
  (conclusion here: "success", "skipped")`. The answer on stdout is unchanged. A wrong value (`failed` where GitHub
  says `failure`) then shows instead of reading as "there are none". A path that no item has at all (often a typo)
  counts as `null` in each, and a note says so, since `!=` would otherwise keep everything quietly. A `--fields`
  path that matches nothing gets a note too, listing the keys there (`--fields nmae`).
- In `batch`, an item's `"where": ["path=value", ...]` (or one condition as a string) applies to it; `batch --where`
  to the items without their own. `"fields"` takes a list or one comma-separated string. A note sits next to the
  item's `result` as `note`, with any note about its arguments.
- A daemon older than 0.2.4 does not know `--where`. The CLI then refuses before calling and says to restart the
  daemon (section 5) or to call with `--direct`.

### Long Schemas (`utcp schema`)

`utcp schema <manual.tool>` prints compact JSON for agents and pipes (`--pretty` for people). A few tools have
huge inputs: Azure DevOps `update_definition` took 81 KB, about 20,000 tokens. Since 0.2.6, a schema longer than
about 4,000 characters keeps the deepest levels that fit, and each object below them becomes one line, such as
`"collapsed":"36 fields: authoredBy, badgeEnabled, ...; see with --path body"`; a map of named objects says
`see one with --path X.*`. A note on stderr says the schema was shortened. Of 1,993 tools, 78 are shortened, and the
longest now prints 5.7 KB.

- `--path body.process` prints that part alone. A path goes through lists, `oneOf`, `anyOf` and `allOf`; `*` is a
  map's values; a key that itself has dots (`System.State`) is one name. A wrong path lists the fields there.
- `--full` prints the whole schema.

## 4. Changing and Removing Manuals

```sh
utcp refresh <name>...              # fetch the manual again from its source, or ask its MCP server again
                                    # (picks up new endpoints or tools; --all: every manual)
utcp set-hint <name> '<few words>'  # add or change a hint; --clear removes it
utcp get-hint [<name>]              # show hints
utcp deregister <name>              # take it out of search and calls; settings, hint and saved copy are kept
utcp register <name>                # bring a deregistered manual back, offline, with its old settings
utcp deregister <name> --purge-cache  # delete it for good, with any staged copy and previous version
                                      # (adding it back needs the original source)
```

Registering, deregistering, promoting, rolling back and hint changes reload the daemon and rewrite the installed
account-wide (`--global`) agent skills, whose description lists the registered manuals. Agents see the new list in
their next session. Project copies made with `utcp sync-ides --workspace <dir>` are not rewritten; run that command
again. To update a manual you depend on, consider `utcp stage` (section 3) instead of `refresh` or `register`, so
you can check what changes first.

## 5. The Daemon

Without the daemon, every `utcp` command reads the saved files itself. That works, but each call sets up its own
connections, and each call to a command-started MCP server starts and stops that server. The daemon keeps the
catalog in memory, keeps HTTP connections and MCP servers warm, and keeps the last 500 calls for `utcp log recent`.

```sh
utcp daemon status        # running?, version, PID, uptime, manuals, calls served
utcp daemon start         # start it in the background (through systemd when the unit is installed)
utcp daemon stop
utcp daemon restart       # needed after installing a new utcp binary
utcp daemon reload-env    # send this shell's variables (only those manuals use) to the daemon, reread its
                          # environment and .env, and run the secrets command again
```

With the systemd unit, `systemctl --user restart utcp-daemon` does the same as `utcp daemon restart`.

Changes to the registry, saved manuals, `settings.toml` and `.env` reach a running daemon on their own. Restart it
only for a new `utcp` binary or to clear everything at once. A restart stops all of its MCP servers, so calls in
progress fail. After installing a new `utcp`, `utcp daemon status` and `utcp doctor` show a `WARNING` while the
daemon still runs the old version.

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

   Times are local, to the second; `--json` keeps the stored UTC timestamps. Without a daemon, `log recent` reads the
   disk log, which exists only if logging was turned on.

2. **Look up the error:**

   | Error says | Meaning | Fix |
   | :--- | :--- | :--- |
   | `rate limited by <host> (HTTP 429)` or `(HTTP 403)` ... `retry after N s` | The API's limit is used up. All agents using the same token share it. | Wait the time it names. Retrying sooner fails again. |
   | `these placeholders were not set: NAME` ... `Nothing was sent` | A `${NAME}` the manual needs has no value, so the call was not sent. `--dry-run` shows it as `***UNSET:NAME***`. | Give it a value (section 3, Secrets): `.env`, the secrets command, or export it and run `utcp daemon reload-env`. |
   | `missing required argument`, `unknown argument 'x' (did you mean 'y'?)`, `'body.x' is not a field here (did you mean 'y'?)`, `this tool takes no arguments`, a wrong type or a value not in the list ... `Nothing was sent` | The arguments, or the fields of a JSON body, don't fit the tool's input schema. All problems are listed at once. | Fix the call from `utcp schema <manual.tool>`. |
   | `note: sent as given, but not in the tool's schema` ... | The call was sent, with body keys the schema doesn't declare (often read-only keys of an answer sent back, which do no harm). | If the result shows no change, check the keys it names. |
   | `HTTP request failed with status <code>: <body>` | The API refused the call; the body is its own answer, on one line. | Read the API's message. A 404 on an existing resource is often a permission the token lacks. |
   | `Manual 'x' is not registered` or `is staged, not active yet` | The manual is deregistered, or only staged. Direct mode refuses it like the daemon, even when an old saved copy is still in the cache. | `utcp list`; `utcp register x` brings a deregistered manual back, `--staged` calls a staged one. |
   | `batch items ... have the same id`, `unknown field` in a batch item | The batch is refused before any call. | Give each item its own `id` (or none: the position is used); use `tool`, `args`, `id`, `fields`, `where`, `out`. |
   | `utcp: k of n batch items failed` (exit status 2) | Some items failed; every result, with each failure's `error`, was printed. | Read the failed items' `error`. |
   | `Secrets command failed: ...` (`reload-env`, `doctor`, `daemon.log`) | The `[secrets]` command did not run cleanly; the daemon keeps the values of its last good run. | Run the command by hand to see its own error output (it prints secrets, so not where others can see), fix it, then `utcp daemon reload-env`. |
   | `Tool 'x.y' not found` | No such manual or tool is registered. | `utcp search` or `utcp list` for the right name. If `x` is staged, the error says so and gives the command with `--staged`. |
   | `The manual and the tool are joined by a dot` | `utcp call x y` put the tool where the JSON arguments go. | Run the command it suggests: `utcp call x.y` (with `--staged` if `x` is staged). |
   | `'x' is not staged` | `--staged`, `diff`, `promote` or `discard` named a manual that is not in the staging area. | `utcp list --staged`; stage it with `utcp stage x <source>`. |
   | `MCP server 'x' did not answer ... within 60s` | The local server is stuck or very slow. | `utcp daemon restart-server x`. |
   | `failed to start MCP server command 'x': program not found` | `x` is not on the `PATH` of the process that starts it (the daemon, or your shell with `--direct`). | Give the full path to `--command`, or put its folder on `PATH` and restart the daemon. Before 0.1.3, Windows also needed `npx.cmd` instead of `npx`. |
   | `MCP server 'x' was stopped on request` | Someone ran `stop-server` or `restart-server` during the call. | Call again. |
   | `upload refused` or `download stopped` ... `only because of this limit` | The file is larger than `max_upload_mb` or `max_download_mb`, or the setting is `0` (turned off). Nothing was sent or kept. | Agents: tell the user. The user raises the setting in `settings.toml`, or comments it out for no limit (section 2). |
   | `cannot upload <path>` | The file named in `{"file": ...}` does not exist, is not a file, or `offset`/`length` go past its end. | Fix the path (relative paths are relative to the caller's folder). |
   | `<host> did not answer for 60 s` or `sent no data for 60 s in the middle of its answer` | The connection stalled. | Try again later. A request that changes something may have been carried out; check first. |
   | `utcp daemon exited before listening` or `did not start listening within N seconds` | `utcp daemon start` failed. | Read the lines it quotes from `daemon.log`; `utcp daemon status` names the file. |
   | `utcp daemon is not running` or `is not currently running` | From `daemon servers`, `stop-server`, `restart-server` and `reload-env` (`daemon stop` prints it as information). | `utcp daemon start`. Calls work without it. |
   | `Cannot use the utcp daemon at ...: access denied` (Windows) | The daemon was started from an administrator (elevated) shell, so normal shells may not use it. | `utcp daemon stop` in an elevated shell, then `utcp daemon start` in a normal one. `utcp --direct` works meanwhile. |
   | `Cannot use the utcp daemon at ...: permission denied` (Linux) | Another account started it, for example root through `sudo`. | Stop it as that account, start it as yourself. |

3. **Narrow it down:**
   - `utcp call <manual.tool> '<json>' --dry-run` shows the request it would send (URL, method, headers, query
     parameters and body, with secrets redacted) without sending it. It runs the same argument checks, and its
     output has a `note` when the call has one or when `--where` or `--fields` were given (they shape an answer,
     and a dry run has none).
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
installing a new `utcp`, run `utcp sync-ides --global` again so the skills match it. A separate setup compares the
skills with its own manuals, so `doctor` there shows your normal skills as `OUTDATED` and says so; don't run
`sync-ides --global` there unless that setup should own them.

The description groups manuals that share a `prefix_`: `azdo_* (Azure DevOps, org ni: admin, git, ...)`, with the
words all their hints share written once and the members by name; `utcp list` shows each member's own hint.
`utcp sync-ides --print-skill` shows it. A description may have at most 1,024 characters, so with many manuals the
longest labels are shortened to fit: a manual loses its hint, a group its member names, then its shared words.
Agents still see every hint with `utcp list`.

The `utcp-tools` body (about 4 KB, 1,000 tokens) stays in an agent's context once loaded, so it covers the usual
call and points to `utcp <command> --help` for every option.

Each skills folder records which registry wrote it (`utcp-tools/.utcp-registry`). Only that registry's changes
rewrite it, so registering in a separate setup (section 1) leaves your skills alone. `sync-ides` always writes and
takes the folder over.

## 9. Working on `utcp-rs` Itself

The source is at <https://github.com/Faraclas/agent-misc> in the `utcp-rs/` folder.

```sh
cargo test --workspace            # offline tests; needs Node.js for the mock MCP servers
python3 tests/load/hammer.py      # on-demand load test: many agents calling one daemon at once
```

Before changing code, read `utcp-rs/AGENTS.md`: a change to a command, flag, path or behavior must update the
tests, the skill templates, the spec, the README and this guide in the same commit.
