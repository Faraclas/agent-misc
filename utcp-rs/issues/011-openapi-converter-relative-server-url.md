# Issue 011: `OpenApiConverter::base_url()` Copies Relative `servers[0].url` Verbatim

- **Upstream Crate:** `rs-utcp` `v0.3.2`
- **Upstream Files:** `src/openapi/mod.rs`
- **Local Status:** Fixed in `utcp-rs/crates/utcp-core/src/openapi/mod.rs`
- **Upstream Issue / PR:** Not filed yet

## Problem

OpenAPI 3.x allows `servers[].url` to be relative. The spec says a relative server URL is resolved against the URL of
the document that defines it. The official Swagger Petstore (`https://petstore3.swagger.io/api/v3/openapi.json`)
declares:

```json
"servers": [{ "url": "/api/v3" }]
```

Upstream `base_url()` returns the first server URL unchanged, so every generated tool gets the URL `/api/v3/pet/...`
with no scheme or host. Registration and `search` succeed, but every `call` fails with:

```text
Error: builder error: relative URL without a base
```

The `spec_url` fallback (see [004](./004-openapi-converter-drops-spec-url-port.md)) only runs when `servers` is missing,
so it never helps here.

## Local Fix in `utcp-rs`

`base_url()` now passes `servers[0].url` through `resolve_server_url()`. Absolute URLs are returned unchanged. Relative
URLs are joined onto `spec_url` with `Url::join` (RFC 3986 rules, so `/api/v3` replaces the path and `v2/` is relative
to the spec's directory), and the trailing slash is trimmed. Without a `spec_url` (a local spec file) there is no origin,
so the value is kept as-is; `--base-url` remains the way to fix that case.

Regression test: `openapi::tests::relative_server_url_resolves_against_spec_url`.
