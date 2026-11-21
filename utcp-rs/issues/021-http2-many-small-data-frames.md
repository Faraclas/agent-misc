# Issue 021: Large HTTP/2 Responses Fail With "too_many_data_frames"

- **Upstream Crate:** none; the limit is `h2`'s (0.4.17 and later), the setting that triggered it was ours
- **Local Files:** `crates/utcp-core/src/security.rs`, `crates/utcp-core/Cargo.toml` (test-only dependencies)
- **Local Status:** Fixed in `utcp-rs` 0.1.3
- **Upstream Issue / PR:** Not applicable

## How It Was Found

While testing generated Azure DevOps manuals, a 1.5 MB read (`wit/reporting/workitemrevisions`) failed once and
worked on other runs:

```text
Error: error decoding response body: request or response body error: error reading a body from connection:
connection error detected: detected excessive load generating behavior (b"too_many_data_frames")
```

## Problem

The message is `h2`'s own: a GOAWAY with `ENHANCE_YOUR_CALM` that the client library sends when too many small DATA
frames (under 256 bytes) wait unread. That guard (added in `h2` 0.4.17, August 2026) allows half the connection
window's worth of them. The shared client used `http2_adaptive_window(true)`, which starts at the protocol minimum
of 64 KiB, so about 128 tiny frames arriving faster than they are read closed the connection.

Azure DevOps sends large responses in many frames: the response above came as 2,248 DATA frames, 260 of them under
256 bytes (checked with curl's HTTP/2 frame trace). Whether a burst outran the reader depended on timing, hence the
intermittent failure.

## Fix

The client now uses fixed HTTP/2 windows, 8 MiB for the connection and 4 MiB per stream, instead of the adaptive
window. `h2` keeps its guard, now at 4 MiB, and a response up to utcp's 10 MB cap needs only a few window updates.
The HTTP/2 settings moved into `http2_settings`, so the test uses exactly what every transport uses (all of them go
through `build_secure_http_client`).

## Tests

- `http2_client_reads_a_body_of_many_tiny_data_frames`: a local `h2` server streams 20,000 one-byte DATA frames.
  With the old setting it failed with the same `GoAway(b"too_many_data_frames", ENHANCE_YOUR_CALM)`; with the fix
  the whole body arrives (5 runs in a row). `h2` and `http` are dev-dependencies, already in `Cargo.lock` through
  `reqwest`.
- The Azure DevOps read test that found it was re-run with the fixed binary.
