# Issue 013: MCP Discovery Can Hang Forever, Hides Spawn Errors, and Sends Raw `${VAR}` Placeholders

- **Upstream Crate:** `rs-utcp` `v0.3.2`
- **Upstream Files:** `src/transports/mcp/mod.rs` (`McpStdioProcess::new`, `McpTransport::register_tool_provider`)
- **Local Status:** Fixed in `utcp-rs/crates/utcp-core/src/transports/mcp/mod.rs`,
  `utcp-rs/crates/utcp-core/src/catalog.rs` and `utcp-rs/crates/utcp-cli/src/main.rs`
- **Upstream Issue / PR:** Not filed yet

## Problem

Importing an MCP server with `utcp import-mcp` (or the TUI Import tab) failed in four confusing ways:

```text
$ utcp import-mcp probe --command /nonexistent-mcp
Error: No such file or directory (os error 2)

$ utcp import-mcp probe --command npx --arg -y --arg @scope/server
error: unexpected argument '-y' found

$ utcp import-mcp probe --command sleep --arg 600
(no output; hangs until killed)

$ utcp import-mcp gh --url https://example.com/mcp -H 'Authorization: Bearer ${GH_TOKEN}'
Error: ... HTTP 401   # the server received the literal text "${GH_TOKEN}"
```

## Root Cause

There are two upstream parts and two local parts.

1. **Upstream: bare spawn error.** `McpStdioProcess::new()` returns `cmd.spawn()?` unchanged, so the error does not
   say which program failed to start.
2. **Upstream: no timeout.** `register_tool_provider()` waits for `initialize` and `tools/list` without any time
   limit. A program that is not an MCP server, or one waiting for input or a login, blocks the caller forever.
3. **Local (`import-mcp`): placeholders not resolved for discovery.** `import_mcp_manual()` passed the raw `-e` / `-H`
   values to the transport. `${VAR}` placeholders are only resolved at call time (`interpolate_provider_vars`), so
   servers that need a token rejected discovery.
4. **Local (CLI): hyphenated `--arg` values rejected.** Clap treats `--arg -y` as an unknown flag unless the argument
   allows hyphen values; only `--arg=-y` worked. Passing the whole command line as `--command "npx -y pkg"` also
   failed with an unclear error, because the string is looked up as one program name.

## Local Fix in `utcp-rs`

- `McpStdioProcess::new()` reports `failed to start MCP server command '<cmd>': <os error>`.
- `catalog::discover_mcp_tools()` wraps discovery in `tokio::time::timeout` (`MCP_DISCOVERY_TIMEOUT`, 60 s, long
  enough for a first `npx`/`uvx` download) and always deregisters the provider; the stdio child uses
  `kill_on_drop(true)`, so a timed-out server is stopped.
- `import_mcp_manual()` resolves `${VAR}` placeholders on a copy of the provider for discovery (shell env,
  `~/.config/environment.d`, and the utcp `.env`). The saved manual keeps the placeholders. When discovery fails and
  some placeholders are unset, the error names them.
- `import_mcp_manual()` refuses a `--command` that contains spaces (and is not an existing path) with a hint
  that spells out the `--arg` form.
- `utcp import-mcp`: `--arg` accepts values starting with `-` (`allow_hyphen_values`), and the manual name is now
  optional (`catalog::infer_mcp_manual_name`: `mcp.context7.com` -> `context7`, `npx -y @upstash/context7-mcp` ->
  `context7`).

Regression tests:

- `catalog::mcp_import_tests::discovery_times_out_on_a_silent_server`
- `catalog::mcp_import_tests::discovery_names_a_command_that_cannot_start`
- `catalog::mcp_import_tests::infers_mcp_names_from_urls_and_launch_commands`
- `integration_dual_mode::import_mcp_resolves_placeholders_and_reports_clear_errors`
- `tui::tests::c_saves_an_mcp_import_to_the_imports_folder_with_placeholders_kept`

## Follow-up: Spec Downloads With `-H` (Fixed)

The first fix left one gap: `fetch_source_bytes()` (OpenAPI / UTCP `import`, `register` and `refresh` with `-H`)
sent `${VAR}` header placeholders literally when downloading the spec itself, so a spec behind auth answered 401.
Calls were not affected.

- `fetch_source_bytes()` now resolves the placeholders for the download request only
  (`catalog::resolve_fetch_headers`, same sources as MCP discovery). The registry entry and the saved manual keep
  `${VAR}`.
- A failed download (connection error or HTTP status) ends with the unset placeholder names, like MCP discovery
  (`catalog::unset_placeholders_note`, shared by both).
- `import_manual_to_file()` now takes `&UtcpPaths` and works out the manual name before fetching, so per-manual
  variables (`<manual>_<VAR>` in `.env`) resolve too.

Regression test: `integration_dual_mode::spec_fetch_resolves_header_placeholders` (401 naming the unset variable,
then success once it is in `.env`; no secret in the config, cache or manual files; `refresh` and `import -o` too).
