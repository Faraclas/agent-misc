# Issue 014: Stdio MCP Calls Block Each Other, Lose Out-of-Order Answers, and Hang Forever

- **Upstream Crate:** `rs-utcp` `v0.3.2`
- **Upstream Files:** `src/transports/mcp/mod.rs` (`McpStdioProcess::send_request`, `McpTransport::mcp_stdio_stream`,
  `McpTransport::get_or_create_stdio_process`, the HTTP request builders)
- **Local Status:** Fixed in `utcp-rs/crates/utcp-core/src/transports/mcp/mod.rs`
- **Upstream Issue / PR:** Not filed yet

## Problem

One stdio MCP server process is shared by every call to that manual. In the daemon, many agents call at the same
time, so several requests to the same server are often in flight together. That broke in three ways:

1. **Calls wait in line.** Each call holds the `stdout` lock while it waits for its own answer, so a slow call blocks
   every other call to that server.
2. **Out-of-order answers are lost, and the caller hangs forever.** MCP servers may answer in any order. The call
   reading `stdout` throws away lines whose `id` is not its own (the fix for issue 006 skips them; upstream treats the
   next line as the answer). The skipped answer belonged to another caller, who then waits for a line that never
   comes. Stdio requests have no timeout, so that caller hangs until the daemon restarts.
3. **A crashed server stays broken.** If the server process exits, it stays cached in `stdio_processes`, and every
   later call fails with "MCP process closed connection" until the daemon restarts.

Smaller problems in the same file:

- `mcp_stdio_stream` takes *any* line from `stdout` as its answer, whatever its `id`, and stops only on a
  nonstandard `"final"` flag. It can steal another caller's answer.
- The HTTP path sends fixed JSON-RPC ids (`initialize` and `mcp_http_stream` use `1`, every request uses `2`), so
  concurrent requests share an id.
- Requests the server sends to the client (such as `ping`) are never answered.

## Root Cause

Reading responses is done by each caller, under a lock, instead of by one reader that hands each answer to the
caller waiting for its `id`. There is also no time limit on a stdio request and no check that the cached process is
still running.

## Local Fix in `utcp-rs`

- `McpStdioProcess` now has **one reader task** per server. It reads every line, skips blank lines, non-JSON output
  and notifications, and sends each response to the caller waiting on that `id` (a table of `oneshot` senders).
  Calls write their request under a short writer lock and then wait only for their own answer, so calls to one
  server run concurrently and answers may come back in any order.
- Each request has a **timeout** (60 s, the same as the HTTP client and `MCP_DISCOVERY_TIMEOUT`). The error names
  the server, the method and the time. A timed-out or cancelled call removes its entry, so abandoned ids never pile
  up.
- When the server exits (end of output or read error), the process is marked dead first and every waiting call
  fails at once with "MCP server '<name>' closed its output". `get_or_create_stdio_process` starts a fresh server on
  the next call. Each manual has its own start-up slot, so a slow start (an `npx` download) does not block calls to
  other servers.
- The 10 MB response limit (`MAX_RESPONSE_SIZE`) is checked per answer and reported to that caller only.
- Server-to-client requests are answered: `ping` with `{}`, anything else with JSON-RPC error `-32601`.
- `mcp_stdio_stream` sends through the same router and returns the single answer as a one-item stream (standard
  MCP sends one response per request).
- HTTP MCP requests take their ids from a per-transport counter (`next_http_id`).

Regression tests (offline; an in-memory mock server answers out of order and prints noise):

- `transports::mcp::tests::stdio_concurrent_calls_each_get_their_own_answer_out_of_order` (50 calls in reverse
  order, server `ping` answered, no leftover ids)
- `transports::mcp::tests::stdio_unanswered_call_times_out_without_blocking_others`
- `transports::mcp::tests::stdio_server_exit_fails_waiting_calls_at_once`
- `transports::mcp::tests::stdio_dead_server_is_replaced_and_live_one_reused` (also the stdio stream path)
- `transports::mcp::tests::http_mcp_requests_use_unique_ids`

Real-process tests run the Node fixture `crates/utcp-core/tests/fixtures/mock_mcp_server.cjs` as `node <file>` (no
shebang, so they also run on Windows). It answers in random order and prints the same noise:

- `transports::mcp::tests::node_stdio_server_answers_concurrent_calls_through_the_transport` (50 calls through
  `McpTransport`, one shared process, every server `ping` answered)
- `transports::mcp::tests::node_stdio_unanswered_call_times_out_and_server_keeps_working`
- `transports::mcp::tests::node_stdio_server_crash_or_kill_fails_waiting_calls_and_next_call_restarts_it` (exit
  while calls wait, then an outside `kill -9` / `taskkill /F`)
- `stdio_mcp_server_serves_concurrent_daemon_calls_and_restarts_after_crash` in
  `crates/utcp-cli/tests/integration_dual_mode.rs` (30 concurrent `utcp call` processes through the daemon after
  `import-mcp --command node`)

Compatibility check by hand (needs the network on first use, so it is not part of `cargo test`):
`@modelcontextprotocol/server-everything` through `npx`, behind a daemon: 40 concurrent `echo` calls, `get-sum`,
log messages, a 2 s progress operation, an image saved to the media spool, and a restart after killing the server.

Live check on a Gentoo desktop, against an isolated daemon: three agents ran at the same time for 60 s, 20 calls each in
parallel, every call a separate `utcp` process with a unique input checked against its own expected answer.
`search` 83,763 calls, `d2.format` 32,038 calls, and a stdio MCP echo server that answers in random order 42,625
calls: 0 errors and 0 wrong answers. Killing the MCP server process mid-run made the next call start a new one.
