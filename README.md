# agent-misc

Agent infrastructure, cross-client tool runtimes, and companion utilities for AI coding assistants (Zed, VS Code,
Antigravity CLI `agy`, Hermes, and others). Each project is self-contained in its own folder, with its own docs, issue
log, packaging, CI workflow, and release tags (`<project>-vX.Y.Z`).

## Projects

### [`utcp-rs/`](./utcp-rs/)

A fast, cross-platform (Linux & Windows), direct **Universal Tool Calling Protocol (UTCP)** CLI (`utcp` /
`utcp.exe`), optional `--user` local IPC daemon (`utcp daemon`), and 6-tab companion Terminal UI (`utcp ui`) in Rust.

- **Zero-MCP Context Bloat:** Discovers and invokes REST APIs, WebSockets, GraphQL, SSE, local CLIs, OpenAPI specs, and
  even wrapped third-party MCP endpoints (`utcp import-mcp`) lazily on demand (`search` -> `schema` -> `call`), plus
  auto-compact JSON output, generic `--fields` response projection, and a self-cleaning media spool.
- **Dual-Mode Execution (Standalone Direct + Optional Local Daemon):** Runs standalone out of the box with `bincode`
  page-cache lookups (`< 1 ms`), or connects to a single per-user `utcp daemon` over a `0600` Unix socket / Windows
  Named Pipe (`< 0.2 ms`) for in-memory catalogs, warm `rustls` HTTP/2 connection pools, live `reload-env` sync (the
  daemon keeps only the env vars registered manuals use), and contention-free opt-in activity logging.
- **Secrets Stay Where You Keep Them:** manuals store `${VAR}` placeholders only; an optional secrets command prints
  the values from a shell file, `pass` or a keyring, so the daemon has them at login without copying them anywhere.
- **Cross-Client Agent Skills (`utcp sync-ides`):** Renders platform-native skills across Zed, VS Code, Copilot CLI,
  Antigravity CLI (`agy`), Hermes, and workspace `.agents/skills/`: `utcp-tools` for calling tools (listing
  registered manuals, each with an optional few-word hint such as `context7 (library and framework docs)`) and
  `utcp-admin` with the operating guide, so agents can manage utcp themselves.
- **Interactive 6-Tab Companion TUI (`utcp ui`):** Built-in `ratatui` interface for inspecting manuals/credentials,
  searching schemas in microseconds, trying a tool from its schema (`t`: dry run or real call), importing OpenAPI/UTCP
  specs and MCP servers (`?` explains how), editing manual hints (`h`), syncing IDE skills, tailing live activity (with
  the error text of failed calls, secrets redacted), and monitoring the `--user` daemon and media spool.

Install: `dev-util/utcp` from [`adaptive-overlay`](https://github.com/Faraclas/adaptive-overlay) on Gentoo, or the
Linux and Windows archives on the Releases page (tags `utcp-rs-v*`); see the
[installation section](./utcp-rs/README.md#installation).

Docs: [operating guide](./utcp-rs/docs/operating.md), [specification](./utcp-rs/docs/utcp-rs-spec.md) (`v1.11`),
[issue log](./utcp-rs/issues/README.md) (upstream `rs-utcp` bugs and Windows fixes),
[contributor rules](./utcp-rs/AGENTS.md).

### [`azdo-manuals/`](./azdo-manuals/)

A generator that turns Microsoft's Azure DevOps REST API specs (version 7.1) into six utcp HTTP manuals for one
organization (repos and pull requests, work items, pipelines, tests, wiki and search, projects and users), so agents
reach Azure DevOps through `utcp` instead of an MCP server.

- **Verified source:** the specs' GitHub repository is gone, so they are downloaded on demand from the Software
  Heritage archive and checked against pinned git hashes. Nothing from Microsoft is stored in this repository.
- **Least privilege:** all reads (GETs plus read-only queries and searches) and ten writes an agent needs for code
  review and tracking (pull requests, comment threads, work items, builds), each tested live in a sandbox
  organization; never DELETE; `--read-only` drops the writes. A scope report lists exactly the PAT scopes the
  generated tools need.
- **One static binary** (Rust), offline tests in CI, release archives for Linux and Windows.

Docs: [README](./azdo-manuals/README.md), [contributor rules](./azdo-manuals/AGENTS.md).

## Contributing

See [`AGENTS.md`](./AGENTS.md) for the repository layout rules; each project's own `AGENTS.md` has its specific rules.

## License

This project is licensed under the [MIT License](./LICENSE). `utcp-rs/crates/utcp-core` is derived from
[rs-utcp](https://github.com/universal-tool-calling-protocol/rs-utcp) 0.3.2 (`MIT OR Apache-2.0`); see
[`utcp-rs/NOTICE`](./utcp-rs/NOTICE). `azdo-manuals` reads Microsoft's MIT-licensed API specs at run time; see
[`azdo-manuals/NOTICE`](./azdo-manuals/NOTICE).
