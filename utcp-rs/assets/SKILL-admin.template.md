---
name: utcp-admin
description: >-
  Change this machine's utcp setup: add, refresh or remove APIs and MCP servers (register, import-mcp, deregister),
  set manual hints and secrets, restart the utcp daemon or a stuck local MCP server, and find out why utcp calls
  fail. To find and call tools, use the utcp-tools skill instead.
---

<!-- Written by utcp from its built-in template; edits here are overwritten. Rewrite it with
`{{BIN_NAME}} sync-ides --global`. -->

# Managing utcp (`{{BIN_NAME}}`)

`{{BIN_NAME}}` is shared: every agent of this user on this machine uses the same manuals, secrets, daemon and local
MCP servers. A change you make here changes them for all agents.

## Rules

- Add, refresh or remove manuals only when the user asks for it.
- Ask the user before `{{BIN_NAME}} deregister` (with or without `--purge-cache`) and before
  `{{BIN_NAME}} daemon stop` or `daemon restart`: they affect every agent, and a daemon restart fails other agents'
  calls in progress. Restarting one hung local MCP server (`daemon restart-server`) needs no permission.
- Secrets: write `${VAR}` placeholders in `-H` and `-e`, single-quoted. Ask the user to add `VAR=value` to
  `{{ENV_PATH}}`. Never ask for a secret value in the chat, and never print `.env`.
- Always pass `--hint` with a few words (60 characters at most) saying what the manual is for. Agents see it next to
  the manual's name in the utcp-tools skill.
- After a change, check it: `{{BIN_NAME}} list --manual <name>`, `{{BIN_NAME}} search "<keywords>"`, or
  `{{BIN_NAME}} doctor`.

## Key Commands

```sh
{{BIN_NAME}} register <name> <url_or_file> --hint '<few words>'           # OpenAPI spec or UTCP manual
{{BIN_NAME}} import-mcp <name> --url <url> --register --hint '<few words>' # MCP server over HTTP
{{BIN_NAME}} import-mcp <name> --command <program> --arg <a> --arg <b> --register --hint '<few words>'
{{BIN_NAME}} set-hint <name> '<few words>'       # add or change a hint
{{BIN_NAME}} log recent --failed                 # recent failed calls with their full errors
{{BIN_NAME}} daemon servers                      # local MCP servers: state, PID, restarts
{{BIN_NAME}} daemon restart-server <name>        # fix a hung local MCP server
{{BIN_NAME}} daemon status                       # stopped: names daemon.log with the last start error or crash
{{BIN_NAME}} doctor                              # daemon, paths, missing secrets, skill status
```

## Full Guide

`operating.md`, next to this file, covers each task step by step. Read only the section you need:

1. What Runs Where (files and paths)
2. First Setup
3. Adding Tools (secrets, MCP servers in containers)
4. Changing and Removing Manuals
5. The Daemon
6. Local MCP Servers
7. When Calls Fail (error table and how to narrow a problem down)
8. Agent Skills
9. Working on `utcp-rs` Itself
