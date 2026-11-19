# Issue 005: Missing Cloud Metadata (`169.254.169.254`) and Redirect-SSRF Protection

- **Upstream Crate:** `rs-utcp` `v0.3.2`
- **Upstream Files:** `src/security.rs`, `src/transports/http/mod.rs`, `src/openapi/mod.rs`
- **Local Status:** Fixed in `utcp-rs/crates/utcp-core/src/security.rs`
- **Upstream Issue / PR:** Not filed yet

## Problem

Upstream `validate_url_security(url, false)` only checks the URL scheme when `require_tls == true`. It does not block:

1. Cloud instance metadata addresses (`169.254.169.254`, `169.254.0.0/16`, `fd00:ec2::254`,
   `metadata.google.internal`).
2. HTTP `30x` redirects from a public remote server to `127.0.0.1` or RFC1918 private LAN addresses (redirect SSRF).

## Local Fix in `utcp-rs`

- Added `is_blocked_metadata_host` to `validate_url_security` so metadata IPs/hostnames are unconditionally rejected.
- Added `build_secure_http_client()` with a custom `reqwest::redirect::Policy` that blocks redirects targeting metadata
  endpoints or crossing from a public host into private/loopback/local hosts, while still allowing direct user-configured
  `localhost` and LAN (`nas`, `media-server`) endpoints.
