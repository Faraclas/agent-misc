---
name: utcp-admin
description: >-
  Change this machine's utcp setup: add, stage, promote, refresh or remove APIs and MCP servers (register, stage,
  import-mcp, deregister), set manual hints and secrets, restart the utcp daemon or a stuck local MCP server, and find
  out why utcp calls fail. To find and call tools, use the utcp-tools skill instead.
---

<!-- Written by utcp from its built-in template; edits here are overwritten. Rewrite it with
`{{BIN_NAME}} sync-ides --global`. -->

# Managing utcp (`{{BIN_NAME}}`)

`{{BIN_NAME}}` is shared: every agent of this user on this machine uses the same manuals, secrets, daemon and local
MCP servers. A change you make here changes them for all agents.

## Rules

- Add, stage, promote, roll back, refresh or remove manuals only when the user asks for it.
- Ask the user before `{{BIN_NAME}} deregister` (with or without `--purge-cache`) and before
  `{{BIN_NAME}} daemon stop` or `daemon restart`: they affect every agent, and a daemon restart fails other agents'
  calls in progress. Restarting one hung local MCP server (`daemon restart-server`) needs no permission.
- Secrets: write `${VAR}` placeholders in `-H` and `-e`, single-quoted. Ask the user to add `VAR=value` to
  `{{ENV_PATH}}`, or to the source their secrets command reads (`[secrets]` in `settings.toml`, `operating.md`
  section 3). Never ask for a secret value in the chat, never print `.env`, and never run the secrets command
  yourself: it prints the values. `{{BIN_NAME}} doctor` shows which secrets are set and where they come from.
- Always pass `--hint` with a few words (60 characters at most) saying what the manual is for. Agents see it next to
  the manual's name in the utcp-tools skill.
- After a change, check it: `{{BIN_NAME}} list <name>`, `{{BIN_NAME}} search "<keywords>"`, or
  `{{BIN_NAME}} doctor`.
- Settings are in `settings.toml` (`{{BIN_NAME}} doctor` shows where, and the upload and download limits). It is the
  user's file: change it only when the user asks, by editing the one line (remove or add the `#`), never by
  deleting lines. `operating.md` section 2 lists every setting.

## Key Commands

```sh
{{BIN_NAME}} register <name> <url_or_file> --hint '<few words>'           # OpenAPI 2.0/3.x spec or UTCP manual
{{BIN_NAME}} stage <name> <url_or_file> --hint '<few words>'              # the same, staged for review first
{{BIN_NAME}} diff [<name>...]                    # staged vs active: tools added, removed, and what changed
{{BIN_NAME}} diff <manual>.<tool>                # one changed tool: old against new, part by part
{{BIN_NAME}} search --staged "<keywords>"        # will agents find the staged tools?
{{BIN_NAME}} call --staged <manual.tool> '<json>' # test a staged manual
{{BIN_NAME}} promote <name>...                   # make staged manuals active (--all: every staged one)
{{BIN_NAME}} promote <name>... --no-keep         # the same, keeping nothing for rollback (test or pre-release)
{{BIN_NAME}} discard <name>...                   # drop staged manuals (--all: every staged one)
{{BIN_NAME}} rollback <name>                     # back to the last known-good version the last promote replaced
{{BIN_NAME}} rollback --forget <name>...         # drop kept versions never to go back to (active ones unchanged)
{{BIN_NAME}} refresh <name>...                   # read the manual's source again (--all: every manual)
{{BIN_NAME}} import-mcp <name> --url <url> --register --hint '<few words>' # MCP server over HTTP
{{BIN_NAME}} import-mcp <name> --command <program> --arg <a> --arg <b> --register --hint '<few words>'
{{BIN_NAME}} import <url_or_file> -o <file.utcp.json> # write a manual file without registering it
{{BIN_NAME}} set-hint <name> '<few words>'       # add or change a hint (get-hint [<name>] shows them)
{{BIN_NAME}} list                                # the manuals, their tool counts and hints
{{BIN_NAME}} log recent --failed                 # recent failed calls with their full errors
{{BIN_NAME}} daemon servers                      # local MCP servers: state, PID, uptime, restarts
{{BIN_NAME}} daemon restart-server <name>        # fix a hung local MCP server
{{BIN_NAME}} daemon stop-server <name>           # stop it until the next call starts it again
{{BIN_NAME}} daemon status                       # version (WARNING: needs a restart); stopped: names daemon.log
{{BIN_NAME}} daemon reload-env                   # pick up changed secrets (shell, .env, secrets command)
{{BIN_NAME}} doctor                              # daemon, paths, secrets and their source, skill status
```

- `promote`, `discard` and `refresh` need names or `--all`; `--all` changes every manual for every agent. A name
  ending in `*` takes every manual it starts (`'azdo_*'`, quoted); so does `rollback --forget`.
- Rollback is for going back to the last known-good version after a promote turned out wrong. Use `promote --no-keep`
  when the replaced version was only a test or pre-release, and `rollback --forget` to drop a kept version the user
  would never go back to.
- `import-mcp --command` takes the program only (`--command "npx -y pkg"` fails): pass each argument with its own
  `--arg` and each environment variable with `-e 'KEY=${VAR}'`. A server's `mcpServers` JSON (from its README) maps
  directly: `command` to `--command`, each `args` entry to `--arg`, each `env` entry to `-e`. Remote servers take
  auth in `-H`. Discovery waits up to 60 s, since a first `npx` or `uvx` run downloads the package. The name is
  optional (taken from the URL host or package).
- Local MCP servers (imported with `--command`) run inside the daemon, start on first use and are shared by every
  agent. A restart or stop fails the calls waiting on that server at once ("was stopped on request"), including
  other agents' calls. To keep a server off for good, ask the user before `{{BIN_NAME}} deregister <name>`.

## Full Guide

`operating.md`, next to this file, covers each task step by step. Read only the section you need:

1. What Runs Where (files and paths)
2. First Setup (and every setting in `settings.toml`)
3. Adding Tools (staging, secrets and the secrets command, MCP servers in containers, files tools send and receive)
4. Changing and Removing Manuals
5. The Daemon
6. Local MCP Servers
7. When Calls Fail (error table and how to narrow a problem down)
8. Agent Skills
9. Working on `utcp-rs` Itself
