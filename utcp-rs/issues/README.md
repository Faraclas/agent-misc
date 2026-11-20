# Upstream & Vendored Issue Tracker (`utcp-rs` / `rs-utcp`)

This directory tracks every bug, missing feature, security gap, or behavioral divergence discovered in upstream
[`universal-tool-calling-protocol/rs-utcp`](https://github.com/universal-tool-calling-protocol/rs-utcp) (`v0.3.2`) and
patched inside `utcp-rs`, plus bugs in `utcp-rs`'s own code found the same way (mostly Windows, 016-017, and the
pre-publish review, 018). Each entry says which of its items are upstream.

Maintaining one document per issue serves two purposes:

1. **Upgrade Safety:** If upstream releases a newer version, we have an exact checklist of local fixes that must not be
   regressed.
2. **Upstream Contribution Ready:** Each file captures the reproduction, root cause, affected upstream paths, and our
   implemented fix so we can file upstream issues or submit pull requests anytime.

## Tracked Issues

| ID | Title | Upstream Version | Local Status | Upstream PR / Issue |
| :--- | :--- | :--- | :--- | :--- |
| [001](./001-cargo-missing-feature-gates-and-broken-rhai.md) | Missing Cargo feature gates, `OpenSSL` lock-in, and broken `rhai` build | `0.3.2` | Fixed in `utcp-core` | Not filed yet |
| [002](./002-library-stdout-stderr-pollution.md) | Library `println!` and `eprintln!` calls corrupt CLI JSON `stdout`/`stderr` | `0.3.2` | Fixed in `utcp-core` | Not filed yet |
| [003](./003-http-transport-json-quoted-params-and-body-routing.md) | `HttpClientTransport` JSON-quotes string path/query params and misroutes `body_field` | `0.3.2` | Fixed in `utcp-core` | Not filed yet |
| [004](./004-openapi-converter-drops-spec-url-port.md) | `OpenApiConverter::base_url()` drops custom port numbers from `spec_url` | `0.3.2` | Fixed in `utcp-core` | Not filed yet |
| [005](./005-ssrf-missing-metadata-and-redirect-protection.md) | Missing cloud metadata (`169.254.169.254`) and redirect-SSRF protection | `0.3.2` | Fixed in `utcp-core` | Not filed yet |
| [006](./006-mcp-transport-missing-initialize-and-inputschema.md) | `McpTransport` omits `initialize` handshake, ignores `inputSchema`, and leaks prefix | `0.3.2` | Fixed in `utcp-core` | Not filed yet |
| [007](./007-cli-transport-hardcoded-subcommand-and-exit-codes.md) | `CliTransport` hardcodes `call <provider> <tool>` and ignores non-zero exit codes | `0.3.2` | Fixed in `utcp-core` | Not filed yet |
| [008](./008-http-client-missing-default-user-agent.md) | `HttpClientTransport` omits default `User-Agent` header (`403` on GitHub API) | `0.3.2` | Fixed in `utcp-core` | Not filed yet |
| [009](./009-cli-test-etxtbsy-race-on-many-core-linux.md) | `CliTransport` unit tests hit Linux `ETXTBSY` race on multi-core hosts | `0.3.2` | Fixed in `utcp-core` | Not filed yet |
| [010](./010-mcp-http-streamable-spec-406-and-sse-framing.md) | `McpTransport` fails on MCP `2025-03-26` Streamable HTTP servers (`406` & SSE) | `0.3.2` | Fixed in `utcp-core` | Not filed yet |
| [011](./011-openapi-converter-relative-server-url.md) | `OpenApiConverter::base_url()` copies relative `servers[0].url` verbatim (no host) | `0.3.2` | Fixed in `utcp-core` | Not filed yet |
| [012](./012-loader-template-missing-call-template-type.md) | `tool_to_provider()` rejects legacy `provider` / `import-mcp` templates (no `call_template_type`) | `0.3.2` | Fixed in `utcp-core` | Not filed yet |
| [013](./013-mcp-discovery-timeout-spawn-error-and-placeholders.md) | MCP discovery hangs without a timeout, hides spawn errors, and sends raw `${VAR}` placeholders | `0.3.2` | Fixed in `utcp-core` | Not filed yet |
| [014](./014-mcp-stdio-concurrent-calls-block-lose-answers-and-hang.md) | Stdio MCP calls block each other, lose out-of-order answers, and hang forever | `0.3.2` | Fixed in `utcp-core` | Not filed yet |
| [015](./015-mcp-stdio-server-tree-leaks-after-discovery-deregister-and-stop.md) | Stdio MCP servers behind `npx`/`uvx` keep running after discovery, deregister, and daemon stop | `0.3.2` | Fixed in `utcp-core` | Not filed yet |
| [016](./016-windows-daemon-and-process-bugs-found-by-local-windows-testing.md) | Windows: `daemon start` holds the caller's output, console windows, pipe busy/owner, shutdown reply race | `0.3.2` | Fixed in `utcp-rs` | Not filed yet |
| [017](./017-windows-bugs-found-on-real-windows.md) | Real Windows: missing profile, quoted `cli` paths, UDP reset, job-killed daemon, elevated daemon, logon task user, absolute `--command` paths | `0.3.2` | Fixed in `utcp-rs` | Not filed yet |
| [018](./018-pre-publish-review-fixes.md) | Pre-publish review: hung `cli` tools not stopped, `sprintf` UTF-8 panic, WebRTC client, `serde_yml`, unit leaks `.env`, media folder ownership, MCP `refresh`, `import -o`, caller names | `0.3.2` | Fixed in `utcp-rs` | Not filed yet |
| [019](./019-daemon-secrets-at-startup-and-doctor.md) | Daemon has no secrets after a reboot (new secrets command), `doctor` checks the caller instead of the daemon, no startup warnings | none | Fixed in `utcp-rs` 0.1.1 | Not applicable |
