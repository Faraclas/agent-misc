# Issue 003: `HttpClientTransport` JSON-Quotes String Params and Misroutes `body_field`

- **Upstream Crate:** `rs-utcp` `v0.3.2`
- **Upstream Files:** `src/transports/http/mod.rs`
- **Local Status:** Fixed in `utcp-rs/crates/utcp-core/src/transports/http/mod.rs`
- **Upstream Issue / PR:** Not filed yet

## Problem

`HttpClientTransport::call_tool` in `rs-utcp v0.3.2` has four parameter-routing and response-parsing bugs:

1. **Literal JSON quotes in URL path and query parameters:** Upstream calls `value.to_string()` on `serde_json::Value`.
   When `value` is `Value::String("octocat")`, `value.to_string()` produces `"\"octocat\""` (including literal quote
   characters) without URL encoding, turning `/users/{id}` into `/users/"octocat"`.
2. **Path parameters leaked into query string or JSON body:** Consumed `{param}` placeholders are never removed from
   `args`, so `GET /users/{id}` also sends `?id="octocat"` and `POST /users/{id}` injects `"id"` into the JSON body.
3. **`header_fields` and `body_field` ignored:** Although `OpenApiConverter` populates `HttpProvider.header_fields` and
   `HttpProvider.body_field` (`"body"`), `HttpClientTransport` ignores both fields—sending header parameters in the body
   and wrapping `POST` bodies inside an extra `{"body": ...}` envelope.
4. **Fails on `204 No Content` or plain-text responses:** `serde_json::from_slice(&body_bytes)?` errors on empty or
   non-JSON `2xx` responses.

## Local Fix in `utcp-rs`

- Extracted raw string values (`as_str()`) and URL-encoded path placeholders via `urlencoding::encode`, removing
  consumed path keys from remaining arguments.
- Extracted `header_fields` into HTTP headers and unwrapped `body_field` cleanly (routing any remaining parameters on
  `POST`/`PUT`/`PATCH` to query parameters).
- Returned `Value::Null` on empty `2xx` bodies and `Value::String` when a `2xx` response is plain text rather than JSON.
