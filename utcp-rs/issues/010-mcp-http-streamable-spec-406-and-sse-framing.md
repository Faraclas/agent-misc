# Issue 010: `McpTransport` Fails on MCP `2025-03-26` Streamable HTTP Servers (`406` & SSE)

- **Upstream Crate:** `rs-utcp` `v0.3.2`
- **Upstream Files:** `src/transports/mcp/mod.rs`
- **Local Status:** Fixed in `utcp-rs/crates/utcp-core/src/transports/mcp/mod.rs`
- **Upstream Issue / PR:** Not filed yet

## Problem

Real-world remote HTTP MCP endpoints (such as Context7 `https://mcp.context7.com/mcp` and DeepWiki
`https://mcp.deepwiki.com/mcp`) implement the **MCP `2025-03-26` Streamable HTTP** specification. Upstream
`McpTransport::mcp_http_request` fails against them for three reasons:

1. **Missing `Accept: application/json, text/event-stream` header:** Streamable HTTP MCP servers reject `POST` requests
   that do not advertise both `application/json` and `text/event-stream` in `Accept` with `406 Not Acceptable`.
2. **Missing HTTP `initialize` + `Mcp-Session-Id` tracking:** Upstream sends `tools/list` / `tools/call` directly
   without performing `initialize` + `notifications/initialized` or echoing back the server's `Mcp-Session-Id` header.
3. **Unparsed SSE `data:` response bodies:** Streamable HTTP servers frequently reply to `POST` requests with
   `Content-Type: text/event-stream` (`event: message\ndata: {"jsonrpc":"2.0",...}\n\n`), causing
   `serde_json::from_slice(&body_bytes)` to fail with `expected value at line 1 column 1`.

## Local Fix in `utcp-rs`

- Added `Accept: application/json, text/event-stream` on HTTP MCP `POST` requests.
- Added per-URL `initialize` + `notifications/initialized` negotiation caching `Mcp-Session-Id` (with automatic
  one-shot re-initialization on `404` session expiry).
- Added dual JSON / SSE (`data: {...}`) body parsing in `McpTransport::parse_mcp_http_body`.
