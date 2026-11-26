---
name: utcp-tools
description: >-
  Call tools from utcp manuals ({{REGISTERED_MANUALS_HINT}}). These tools are not in your own tool list or MCP
  servers; run them with the `{{BIN_NAME}}` CLI (`search`, `schema`, `call <manual.tool>`). Use this skill whenever
  a request names one of these manuals or needs one of these services, instead of `--help` probing or scratch files.
---

<!-- Written by utcp from its built-in template and your registered manuals; edits here are overwritten. Change
manuals and hints with `{{BIN_NAME}} register`, `promote`, `deregister` or `set-hint`, which update this file. -->

# Calling Tools with `{{BIN_NAME}}`

Find a tool, read its inputs, call it: `search` -> `schema` -> `call`. Every option is explained in
`{{BIN_NAME}} <command> --help`; read that only when this page is not enough.

## 1. Find a Tool

```sh
{{BIN_NAME}} search "<keywords>"
{{BIN_NAME}} list [<manual>]
{{BIN_NAME}} schema <manual.tool>
```

`search` prints the best matches, one line each; `list` the manuals with their hints, or one manual's tools;
`schema` one tool's inputs. A long schema comes shortened, with a note: each deeper object is a `collapsed` line
naming its fields; see one with `--path` (`--path body.process`).

Staged manuals are being tested and are not active. Use them only when the user asks you to test one, with
`--staged` on each command.

## 2. Call It

{{CALL_EXAMPLES}}

- **`--dry-run`** shows the request without sending it.
- **`--fields id,name,commit.message`** keeps only those keys. Paths go through lists; for a wrapped list
  (`{"total_count": N, "workflow_runs": [...]}`) name the item keys directly.
- **`--where 'path=a|b'`** keeps the list items that match, after the tool's own filter arguments (see `schema`).
  `~=` is contains, `^=` starts with, `!` before any of them keeps the others, and `>`, `>=`, `<`, `<=` compare
  numbers and dates (a date alone is that whole UTC day). Paths and values ignore case, and a missing field is
  `null`. Repeat it for more conditions. `total_count` still counts the whole list.
- **Notes** (`note: ...` on stderr, `note` in `batch`) explain what utcp did: why an answer is empty or
  unfiltered, which keys aren't in the schema, where a file went. When nothing matched, the note lists the values
  the field has: fix the value rather than conclude there are none. An answer with a note is still a success, so
  don't call again.
- **Pages:** a long list prints `{"result": ..., "next_page": {...}}`. For more, call again with your arguments
  plus those in `next_page`. Fetch more only when you need them, and say so when you stop early.
- **Files** (an archive, a log, an image): the answer is `{"type": "file", "path": ...}`, saved to Downloads, or
  to `--out <path>` (`--out -` writes the bytes to stdout). Open it only if you need its content. To upload, pass
  `{"file": "<path>"}` where the schema takes the body.

## 3. Several Calls at Once

Each item has `tool` and `args`, and optionally `id`, `fields`, `where` and `out`. Every answer or `error` is
printed, and the exit status is 2 when any item failed:

{{BATCH_EXAMPLE}}

## 4. When a Call Fails

- **"Nothing was sent":** the arguments don't fit the tool. Fix them from `schema`. A secret that isn't set is
  for the user to set.
- **The API said no:** the error has its status and answer. When rate-limited, wait as long as it says.
  `{{BIN_NAME}} log recent --failed` lists recent failures.
- **A size limit** (`max_upload_mb`, `max_download_mb`): tell the user; don't change it unless asked.
- **Adding manuals, hints or secrets, or a stuck daemon or MCP server:** use the utcp-admin skill.
