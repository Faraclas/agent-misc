# Issue 009: `CliTransport` Unit Tests Hit Linux `ETXTBSY` Race on Multi-Core Hosts

- **Upstream Crate:** `rs-utcp` `v0.3.2`
- **Upstream Files:** `src/transports/cli/mod.rs`
- **Local Status:** Fixed in `utcp-rs/crates/utcp-core/src/transports/cli/mod.rs`
- **Upstream Issue / PR:** Not filed yet

## Problem

In `transports::cli::tests`, `write_mock_cli` and `write_cli_requiring_mode_flag` call `std::fs::write(&script_path,
...)` to write a `#!/usr/bin/env node` script and immediately `execve` that file path directly while dozens of sibling
tests in the same `cargo test` process are concurrently calling `Command::spawn()` (`fork()`). On multi-core Linux
kernels, a concurrent `fork()` briefly inherits the open write file descriptor before close, causing `execve` on the
script path to fail intermittently with `Text file busy (os error 26 / ETXTBSY)`.

## Local Fix in `utcp-rs`

Invoked `node <script_path>` directly in `register_and_call_tool_via_cli_transport` and
`call_tool_respects_base_command_args` (matching `call_tool_stream_not_supported`), so `execve` targets `/usr/bin/node`
rather than the freshly written temporary file.
