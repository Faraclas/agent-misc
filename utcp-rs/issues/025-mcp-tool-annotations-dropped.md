# Issue 025: MCP Tool `annotations` Are Dropped on Import

- **Upstream Crate:** `rs-utcp` `v0.3.2`
- **Upstream Files:** `src/tools/mod.rs` (`Tool`), `src/transports/mcp/mod.rs` (`register_tool_provider`)
- **Local Status:** Fixed in `utcp-rs/crates/utcp-core/src/tools/mod.rs`, `.../transports/mcp/mod.rs` and
  `.../catalog.rs` (`utcp-rs` 0.2.8)
- **Upstream Issue / PR:** Not filed yet

## Problem

Since MCP `2025-03-26`, a `tools/list` tool may carry `annotations`: `title`, `readOnlyHint`, `destructiveHint`,
`idempotentHint` and `openWorldHint`. Upstream's `Tool` mirrors the UTCP tool model (`name`, `description`, `inputs`,
`outputs`, `tags`, `average_response_size`, `tool_provider`) and has no field for them, so
`register_tool_provider` drops them in both of its branches (a tool that deserializes as a UTCP `Tool`, and the MCP
`inputSchema` shape built by hand, issue 006). `utcp import-mcp` and `utcp refresh` then save manuals without them.

This was an accident of the data model, not a decision. Its visible effect in `utcp-rs`: `utcp search` ranks tools
that only read first on equal scores, but could only tell that from an HTTP method (`GET` / `HEAD`), so an MCP tool
never counted as a read even when its server said `readOnlyHint: true`.

## Local Fix in `utcp-rs`

1. **`Tool` and `CachedToolEntry` (the saved manual's tool) have an optional `annotations`** (`serde(default)`,
   left out of the JSON when absent, so manuals saved before still load and others are written as before).
2. **`register_tool_provider` keeps a tool's `annotations` in both branches**, the whole object as the server sent it,
   so hints added to MCP later survive too. A value that is not a JSON object is dropped (`object_annotations()`).
3. **`discover_mcp_manual` (`import-mcp` and `refresh` of an `import-mcp` manual) writes them into the manual**, and
   registering a copy of such a manual (`normalize_utcp_manual_json`, the issue 012 path) keeps them.
4. **`build_index` marks a tool as only reading** when `annotations.readOnlyHint` is `true` (any protocol), besides
   HTTP `GET` / `HEAD`. `false`, a non-boolean or a missing hint do not.

## Tests

- `crates/utcp-core/src/transports/mcp/mod.rs`: `tools_list_keeps_annotation_objects` (both `tools/list` shapes,
  unknown hints kept, non-object values dropped).
- `crates/utcp-core/src/catalog.rs`: `index_marks_mcp_read_only_hint_as_read` (`true`, `false`, `"true"`, no hint,
  no annotations) and `converts_saved_mcp_manual_keeping_annotation_objects`.
- `crates/utcp-cli/tests/integration_dual_mode.rs`: `refresh_asks_the_mcp_server_again` checks the saved manual's
  `annotations` after `import-mcp` and after `refresh` (the Node fixture marks `stats` `readOnlyHint`) and that
  `utcp search tool` lists `stats` before `echo`.

## Re-import After Upgrading

Manuals imported with `import-mcp` before this fix have no `annotations` until `utcp refresh <name>` asks the server
again. Tool names, inputs and calls do not change; the manual's `sha256` changes when the server sends annotations.
