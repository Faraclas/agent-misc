# Issue 012: `tool_to_provider()` Rejects Templates Without `call_template_type`

- **Upstream Crate:** `rs-utcp` `v0.3.2`
- **Upstream Files:** `src/loader.rs` (`tool_to_provider`, around L300-317)
- **Local Status:** Fixed in `utcp-rs/crates/utcp-core/src/loader.rs` and `utcp-rs/crates/utcp-core/src/catalog.rs`
- **Upstream Issue / PR:** Not filed yet

## Problem

Registering a cached manual as a new source fails for MCP manuals:

```text
$ utcp register ctx_copy ~/.cache/utcp/manuals/context7.utcp.json
Error: Missing call_template_type
```

The same command works for cached HTTP manuals (d2, github).

## Root Cause

There are two parts, one upstream and one local.

1. **Upstream.** `tool_to_provider()` reads either `tool_call_template` or the legacy v0.1 `provider` field and passes
   it to `call_template_to_provider()`. That function requires `call_template_type`. A legacy `provider` object only
   has `provider_type`, so the legacy fallback can never succeed. Upstream's own `migration.rs` renames
   `provider_type` to `call_template_type` for this conversion, but the loader does not use it.
2. **Local (`utcp import-mcp`).** `import_mcp_manual()` stored the serialized `McpProvider` as `tool_call_template`.
   That JSON has `provider_type: "mcp"` but no `call_template_type`, so every manual written by `import-mcp` hit the
   error above when used as a `register` source. Registering via `import-mcp --register` worked because it does not
   re-read the template through `tool_to_provider()`.

## Local Fix in `utcp-rs`

- New `loader::with_call_template_type()` fills `call_template_type` from `provider_type` (or `type`) when it is
  missing, and leaves an explicit value alone. `tool_to_provider()` applies it to both the `tool_call_template` and the
  legacy `provider` branch.
- `import_mcp_manual()` now writes a `tool_call_template` that includes `call_template_type`.
- `normalize_utcp_manual_json()` stores the repaired template, so registering an old file also produces a correct
  cached copy. Running `utcp refresh <name>` repairs an existing cached manual in place.

Regression tests:

- `loader::tests::tool_to_provider_accepts_legacy_provider_without_call_template_type`
- `loader::tests::tool_to_provider_accepts_call_template_without_call_template_type`
- `loader::tests::with_call_template_type_keeps_an_explicit_value`
- `catalog::field_projection_tests::converts_cached_mcp_manual_whose_template_lacks_call_template_type`
- `integration_dual_mode::import_mcp_media_spool_and_daemon_reload_env` (registers both the cached `import-mcp` output
  and a legacy-shaped copy of it)
