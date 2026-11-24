---
name: utcp-tools
description: >-
  Call tools from utcp manuals ({{REGISTERED_MANUALS_HINT}}): REST and OpenAPI APIs, CLI tools, WebSockets and
  imported MCP servers. These tools are not in your own tool list or MCP servers; run them with the
  `{{BIN_NAME}}` CLI (`{{BIN_NAME}} search`, `schema`, `call <manual.tool>`). Use this skill whenever a request names
  one of these manuals, instead of ad-hoc `--help` probing or scratch files.
---

<!-- Written by utcp from its built-in template and your registered manuals; edits here are overwritten. Change
manuals and hints with `{{BIN_NAME}} register`, `promote`, `deregister` or `set-hint`, which update this file. -->

# UTCP Direct Tool Calling (`{{BIN_NAME}}`)

Use the native `{{BIN_NAME}}` CLI to discover, inspect, and invoke external APIs and local tools lazily on demand so
your context window stays clean and you avoid wasting turns on `--help` probing or temporary files. Follow the
progressive disclosure workflow (`search` -> `schema` -> `call`):

## 1. Discover & Inspect Tools Lazily

- **Step 1: Search tools by keyword (compact summary):**

  ```sh
  {{BIN_NAME}} search "<keywords>"
  ```

- **Step 2: Fetch the exact JSON input schema for only the tool you need:**

  ```sh
  {{BIN_NAME}} schema <manual.tool>
  ```

- **List all registered manuals/tools (compact summary):**

  ```sh
  {{BIN_NAME}} list
  ```

- **Staged manuals** (added with `{{BIN_NAME}} stage` for testing, not yet promoted) are not in `search` or `list`.
  Reach them only when the user asks you to test one, with `--staged` on `list`, `schema`, `call` and `batch`:

  ```sh
  {{BIN_NAME}} list --staged
  {{BIN_NAME}} call --staged <manual.tool> '{"param": "value"}'
  ```

## 2. Call a Tool

{{CALL_EXAMPLES}}

- **Filter Verbose API Responses (`--fields`):** For noisy APIs (such as GitHub commits, issues, or repos), pass
  `--fields key1,nested.key2` (for example, `--fields sha,commit.message,commit.author,html_url`) to strip unneeded
  boilerplate keys while keeping full fidelity when omitted. Dotted paths pass through lists (`assets.name`), and
  for a wrapped list such as `{"total_count": N, "workflow_runs": [...]}` you can name the item keys directly
  (`--fields id,name,conclusion`).
- **Long lists come in pages:** when a list has more, utcp prints `{"result": <answer>, "next_page": {...}}`. For
  the next page, call the same tool again with your arguments plus the ones in `next_page` (such as
  `{"page": "2"}` or `{"continuationToken": "..."}`). The last page prints the answer alone. `--fields` and `--out`
  apply to `result`; in `batch`, `next_page` is next to the item's `result`. Fetch more pages only when you need
  them, and say so when you stop early.
- **Images and Binary Media:** If a tool writes an image or binary payload to a file path on disk (for example,
  `"type": "media_file"` with a `"path"`), pass that file path to your multimodal file-reading/vision tool only when
  visual inspection is needed.
- **Files a tool answers with** (an archive, a log, a PDF): utcp saves the file and prints
  `{"type": "file", "path": ..., "mime_type": ..., "bytes": ..., "filename": ...}` instead of its content
  (`mime_type` is the bare media type, such as `application/zip`, without parameters). Without `--out` it goes to
  the user's Downloads folder (`a.zip` becomes `a (1).zip` if it exists; nothing is overwritten). To choose the
  place, pass `--out <path>` (overwrites, like `curl -o`; a folder keeps the server's file name), or `--out -` to get
  the raw bytes on stdout for a pipe. Open the file only if you need its content.
- **Answers too large to print:** a JSON or text answer over 10 MB is saved to a file the same way, with a `note`.
  Read the parts you need (for example with `jq`), or call again with `--fields` to get less. That folder is
  emptied after a day, so use `--out` for a file you want to keep.
- **Uploading a file:** a tool that uploads takes the file in its body argument (see `schema`) as
  `{"file": "<path>"}`; add `"offset"` and `"length"` (bytes) to send one part. A relative path is relative to your
  working folder, and utcp reads the file with your permissions.
- **Size limits:** when an upload or download fails "only because of this limit", tell the user: the error names
  the setting (`max_upload_mb` or `max_download_mb` in utcp's `settings.toml`) and how to change it. Do not change
  it yourself unless the user asks.

## 3. Execute Multiple Calls in Parallel (`batch`)

Each item in `batch` also accepts an optional `"fields": ["key1", "nested.key2"]` array, and `"out": "<path>"` to
save that item's answer to a file:

{{BATCH_EXAMPLE}}

## 4. Register, Import, Refresh & Manage Daemon

Add a new API or MCP server only when the user asks for it. `import` and `import-mcp` print `Manual`, `Tools` and a
`Secrets` line naming the variables the manual needs; `register` prints `Registered '<name>' (N tools, ...)` and, if
it needs secrets, a `Required credentials` line.

- **Register an OpenAPI 2.0/3.x spec (JSON/YAML) or a UTCP manual** (`-H` headers are stored and sent on every
  call; `--base-url` overrides the spec's server URL):

  ```sh
  {{BIN_NAME}} register <name> <url_or_file_path> --hint '<few words>' [-H 'Authorization: Bearer ${API_TOKEN}'] [--base-url <url>]
  ```

- **Or stage it first** (same arguments as `register`) to review and test it before agents see it: `diff` shows what
  changes, `call --staged` tests it, `promote` makes it active (`discard` drops it, `rollback` undoes a promote):

  ```sh
  {{BIN_NAME}} stage <name> <url_or_file_path> --hint '<few words>'
  {{BIN_NAME}} diff [<name>]
  {{BIN_NAME}} promote [<name>]
  ```

- **Import an MCP server** (Model Context Protocol). `import-mcp` asks the server once for its tool list and saves
  it as a manual; afterwards its tools are used like any other (`search` -> `schema` -> `call`). The name is
  optional (inferred from the URL host or package, e.g. `context7`).

  ```sh
  # Remote server (HTTP URL); auth goes in -H headers
  {{BIN_NAME}} import-mcp <name> --url https://mcp.example.com/mcp -H 'Authorization: Bearer ${EXAMPLE_TOKEN}' --register --hint '<few words>'
  # Local server started by a command: --command is the program ONLY; repeat --arg once per argument
  {{BIN_NAME}} import-mcp <name> --command npx --arg -y --arg @scope/some-mcp-server -e 'API_KEY=${SOME_API_KEY}' --register --hint '<few words>'
  ```

  - Never put the whole command line in `--command` (`--command "npx -y pkg"` fails); split it into `--arg`s.
  - Take the URL or command from the server's own README (its Claude Desktop / Cursor `mcpServers` JSON maps
    directly: `command` -> `--command`, each `args` entry -> `--arg`, each `env` entry -> `-e KEY=...`).
  - Discovery waits up to 60 s (first `npx`/`uvx` runs download the package).
- **Hints:** always pass `--hint` with a few words (60 characters at most) saying what the manual is for, such as
  `--hint 'library and framework docs'`. The hint appears next to the manual's name in this skill's description,
  which is how agents find the right manual when its name alone (such as `context7`) says nothing. Add or fix a hint
  later, or check the current ones:

  ```sh
  {{BIN_NAME}} set-hint <name> '<few words>'
  {{BIN_NAME}} get-hint [<name>]
  ```

- **Secrets:** always write `${VAR}` placeholders in `-H` / `-e` (single-quoted so your shell keeps them), never
  literal tokens: the manual stores the placeholder and the value is resolved at call time. Ask the user to put the
  value in `{{ENV_PATH}}` as `VAR=value`, or where their secrets command (`[secrets]` in `settings.toml`) reads it
  from, or to export it in the shell; then run `{{BIN_NAME}} daemon reload-env`. Do not ask the user to paste secret
  values into the chat, and never run the secrets command yourself (it prints the values). If discovery fails, the
  error names any unset placeholder.
- **Verify an import:**

  ```sh
  {{BIN_NAME}} list <name>
  {{BIN_NAME}} search "<keywords>"
  ```

- **Write a standalone `.utcp.json` without registering** (to review or share; `register <name> <file>` later):

  ```sh
  {{BIN_NAME}} import <url_or_file_path> -o <output.utcp.json> [--name <name>]
  {{BIN_NAME}} import-mcp <name> --url <url> -o <output.utcp.json>
  ```

- **Refresh one or all registered manuals** (from their source URL/file; `import-mcp` manuals ask their MCP server
  again):

  ```sh
  {{BIN_NAME}} refresh [<name>]
  ```

- **Check status (`{{ENV_PATH}}`, `{{SOCKET_PATH}}`, where each secret comes from) or make a running daemon pick up
  changed secrets (this shell's variables, `.env`, the secrets command):**

  ```sh
  {{BIN_NAME}} doctor
  {{BIN_NAME}} daemon reload-env
  ```

- **When calls fail, see why:** recent calls, newest first, with the full error of each failure (works with
  logging off). A rate-limited API says when to retry: wait that long instead of retrying at once.

  ```sh
  {{BIN_NAME}} log recent --failed
  ```

- **Local MCP servers** (manuals imported with `import-mcp --command`) run inside the daemon, started on first use
  and shared by every agent on this machine. If one hangs (calls time out) or misbehaves, check it and restart it.
  A restart or stop makes calls waiting on that server fail at once ("was stopped on request"), including other
  agents' calls. `stop-server` only stops it until the next call; to keep a server off, ask the user before running
  `{{BIN_NAME}} deregister <name>`.

  ```sh
  {{BIN_NAME}} daemon servers                # state, PID, uptime, restart count
  {{BIN_NAME}} daemon restart-server <name>  # stop it and start it again now
  {{BIN_NAME}} daemon stop-server <name>     # stop it now; the next call starts it again
  ```
