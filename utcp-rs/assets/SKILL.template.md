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

- **List the manuals (name, tool count, hint), or one manual's tools:**

  ```sh
  {{BIN_NAME}} list
  {{BIN_NAME}} list <manual>
  ```

- **Staged manuals** (added with `{{BIN_NAME}} stage` for testing, not yet promoted) are not in `search`, and `list`
  shows them under Staged. Reach them only when the user asks you to test one, with `--staged` on `search`, `list`,
  `schema`, `call` and `batch`:

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
printed; when any item failed, `{{BIN_NAME}}` then exits with status 2:

{{BATCH_EXAMPLE}}

## 4. When a Call Fails

- **Nothing was sent:** an error that says so (a required argument missing, an argument or body field name the
  tool doesn't have, a wrong type or value, or a secret placeholder that isn't set) is about your call, not the API.
  Fix the arguments from `{{BIN_NAME}} schema <manual.tool>`; an unset placeholder is for the user to set (see
  the utcp-admin skill). A `note` that keys were "sent as given, but not in the tool's schema" means the API may
  ignore them: if the result shows no change, check those keys.
- **The API said no:** the error has its status and answer. A rate-limited API says when to retry: wait that long
  instead of retrying at once. `{{BIN_NAME}} log recent --failed` lists recent failed calls with their full errors.
- **Adding or changing manuals, hints and secrets, or the daemon and its local MCP servers** (a hung server, calls
  that time out): use the utcp-admin skill.
