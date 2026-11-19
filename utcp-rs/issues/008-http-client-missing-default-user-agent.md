# Issue 008: `HttpClientTransport` Omits Default `User-Agent` Header (`403` on GitHub API)

- **Upstream Crate:** `rs-utcp` `v0.3.2`
- **Upstream Files:** `src/transports/http/mod.rs`, `src/openapi/mod.rs`
- **Local Status:** Fixed in `utcp-rs/crates/utcp-core/src/security.rs`
- **Upstream Issue / PR:** Not filed yet

## Problem

Upstream `HttpClientTransport::new()` builds `reqwest::Client::builder()` without configuring a default `.user_agent(...)`.
`reqwest` sends **no** `User-Agent` header when `.user_agent(...)` is omitted. Major REST APIs—including GitHub's REST
API (`https://api.github.com`), crates.io, and Wikimedia/Nominatim—strictly reject requests lacking a `User-Agent`
header with `403 Forbidden`, causing OpenAPI-generated manuals to fail unless every tool manually hardcodes a custom
`User-Agent` header.

## Local Fix in `utcp-rs`

Configured `.user_agent(concat!("utcp-rs/", env!("CARGO_PKG_VERSION")))` on `build_secure_http_client()` (while still
allowing per-manual `headers` to override `User-Agent` when desired).
