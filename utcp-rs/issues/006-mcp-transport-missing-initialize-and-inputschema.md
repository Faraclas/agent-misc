# Issue 006: `McpTransport` Omits `initialize`, Ignores `inputSchema`, and Leaks Prefix

- **Upstream Crate:** `rs-utcp` `v0.3.2`
- **Upstream Files:** `src/transports/mcp/mod.rs`
- **Local Status:** Fixed in `utcp-rs/crates/utcp-core/src/transports/mcp/mod.rs`
- **Upstream Issue / PR:** Not filed yet

## Problem

Upstream `McpTransport` fails against real MCP servers (`2024-11-05` / `2025-*` spec) due to four issues:

1. **Missing `initialize` handshake:** `McpStdioProcess::new` spawns the server and immediately sends `tools/list` or
   `tools/call` without sending JSON-RPC `initialize` + `notifications/initialized`. Compliant MCP servers reject
   pre-initialization requests.
2. **Asynchronous notifications break response reading:** `send_request` reads only a single line from `stdout` and
   assumes it is the response, failing whenever a server emits `notifications/message` or `notifications/tools/*` first.
3. **Wrong schema property (`inputs` vs `inputSchema`):** `register_tool_provider` deserializes `tools/list` items
   directly into UTCP `Tool` (which requires `"inputs"`), whereas the MCP specification uses `"inputSchema"`, causing
   all standard MCP tools to be silently skipped.
4. **Unstripped provider prefix & leaked child processes:** `call_tool` passes `<provider>.<tool>` instead of `<tool>`
   in `params.name`, and `Command` does not set `kill_on_drop(true)`.

## Local Fix in `utcp-rs`

- Added MCP `initialize` + `notifications/initialized` handshake on stdio startup and skipped non-matching notification
  frames until the matching JSON-RPC `id` arrives.
- Parsed MCP `inputSchema` (and paginated `nextCursor`) in `register_tool_provider`.
- Stripped `<provider>.` prefix in `call_tool` / `call_tool_stream` and enabled `kill_on_drop(true)`.
