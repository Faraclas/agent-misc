# Issue 002: Library `println!` and `eprintln!` Calls Corrupt CLI JSON Output

- **Upstream Crate:** `rs-utcp` `v0.3.2`
- **Upstream Files:** `src/lib.rs`, `src/loader.rs`, `src/security.rs`
- **Local Status:** Fixed in `utcp-rs/crates/utcp-core`
- **Upstream Issue / PR:** Not filed yet

## Problem

Upstream library functions write directly to `stdout` and `stderr`:

1. `UtcpClient::new` (`src/lib.rs`) executes `println!("✓ Loaded provider with {} tools", ...)` when loading providers,
   corrupting machine-parsed JSON output on `stdout`.
2. `validate_url_security` (`src/security.rs`) executes `eprintln!("Warning: URL uses localhost/127.0.0.1: ...")` on
   every single local HTTP call, spamming `stderr` whenever calling local services (such as an agent container on
   `127.0.0.1:8765`).
3. `parse_manual_tools_with_providers` (`src/loader.rs`) writes `eprintln!("Warning: Tool ... uses communication
   protocol ...")` directly to `stderr`.

## Local Fix in `utcp-rs`

Removed all `println!` and `eprintln!` side effects from `utcp-core` library code so `stdout` is strictly reserved for
structured CLI output and `stderr` is reserved for actionable CLI errors.
