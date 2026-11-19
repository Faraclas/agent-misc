# Issue 004: `OpenApiConverter::base_url()` Drops Custom Port Numbers from `spec_url`

- **Upstream Crate:** `rs-utcp` `v0.3.2`
- **Upstream Files:** `src/openapi/mod.rs`
- **Local Status:** Fixed in `utcp-rs/crates/utcp-core/src/openapi/mod.rs`
- **Upstream Issue / PR:** Not filed yet

## Problem

When an OpenAPI spec omits a top-level `servers` block (or uses relative server paths) and `OpenApiConverter::base_url`
falls back to inferring the origin from `spec_url`, upstream writes:

```rust
if let Some(host) = parsed.host_str() {
    return format!("{}://{}", parsed.scheme(), host);
}
```

This silently drops `parsed.port()`, turning `http://127.0.0.1:8765/openapi.json` into `http://127.0.0.1` (port 80) and
breaking all generated tool URLs for services running on non-default ports.

## Local Fix in `utcp-rs`

Preserved `parsed.port()` when constructing the fallback base URL (`format!("{}://{}:{}", parsed.scheme(), host, port)`).
