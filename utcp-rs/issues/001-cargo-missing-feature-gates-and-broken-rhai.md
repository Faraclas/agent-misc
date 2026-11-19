# Issue 001: Missing Cargo Feature Gates, OpenSSL Lock-In, and Broken `rhai` Build

- **Upstream Crate:** `rs-utcp` `v0.3.2`
- **Upstream Files:** `Cargo.toml`, `src/lib.rs`, `src/transports/mod.rs`, `src/plugins/mod.rs`
- **Local Status:** Fixed in `utcp-rs/crates/utcp-core`
- **Upstream Issue / PR:** Not filed yet

## Problem

Adding `rs-utcp = "0.3.2"` directly from crates.io fails to compile on current Rust toolchains and pulls in massive
unneeded dependencies because upstream `Cargo.toml` defines zero Cargo `[features]`:

1. **Broken `rhai` transitive build:** `rs-utcp` unconditionally compiles `rhai = { version = "1.18", features =
   ["serde"] }` for `plugins/codemode`. Current Cargo resolution selects `rhai 1.27`, which fails to compile because
   `smartstring` is pulled without `smartstring/serde`.
2. **Unconditional `webrtc` (~150 crates) and `tonic`/`prost` (`protoc` build dependency):** Every consumer is forced to
   compile DTLS/SRTP/ICE/STUN stacks and run `tonic-build` (`build.rs`) even when only using HTTP, WebSocket, or CLI
   transports.
3. **System `OpenSSL` (`openssl-sys`) and deprecated `serde_yaml`:** `reqwest 0.11` and `tokio-tungstenite 0.21` use
   `native-tls` instead of pure-Rust `rustls`, and `serde_yaml 0.9` is deprecated upstream.

## Local Fix in `utcp-rs`

- Vendored `rs-utcp` into `utcp-core` and gated `grpc`, `webrtc`, and `codemode` (with explicit `smartstring/serde`)
  behind optional Cargo features.
- Upgraded to `reqwest 0.12` (`rustls-tls-native-roots`), `tokio-tungstenite 0.24` (`rustls-tls-native-roots`),
  `base64 0.22`, and `serde_yml`.
