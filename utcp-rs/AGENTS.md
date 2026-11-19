# Agent Instructions (`utcp-rs`)

Paths below are relative to this `utcp-rs/` folder unless they start with `.github/` (repository root).

## Mandatory Lockstep Synchronization Rule

Whenever you add, modify, or remove any CLI command, flag, IPC message, transport behavior, file path, or configuration
option in `utcp-rs`, you **must** update all affected surfaces in the **same commit**. Never leave
documentation, embedded skills, or packaging behind the code:

1. **Rust Implementation & Automated Tests:**
   - `crates/utcp-core/` and `crates/utcp-cli/` (including TUI views in `crates/utcp-cli/src/tui/mod.rs` when
     relevant).
   - Every new feature, CLI flag, transport behavior, or bug fix (`issues/NNN-*.md`) must include offline automated
     unit and/or subprocess integration regression tests (using isolated `TempDir` paths and local mock endpoints,
     never live external services or host user directories).
   - Mock servers that must run as real processes may be Node scripts in `crates/utcp-core/tests/fixtures/`. Start them
     as `node <file>` (no shebang, so they run on Windows too) and keep them free of npm packages and network access.
     Tests that need an external program use `node` (already required), never Unix-only commands such as `sleep`.
   - Compiled test binaries can run without the source tree (for example on a Windows VM): `UTCP_TEST_BIN` overrides
     the `utcp` binary the integration tests run, and `UTCP_TEST_FIXTURES` the folder holding `mock_mcp_server.cjs`.
     New tests that start `utcp` or a fixture must go through `utcp_bin()` / `mock_mcp_script()` (or
     `node_mcp_provider` in `utcp-core`) so these overrides keep working.
   - Run `cargo test --workspace` and ensure all tests pass with 0 compiler warnings and 0 workspace `diagnostics`.
   - `tests/load/hammer.py` is an on-demand load test, not part of `cargo test` or CI. Keep it working whenever CLI
     output, flags, `register`/`import-mcp`/`daemon` behavior, or the MCP fixture change, and run it
     (`python3 tests/load/hammer.py`) after changes to the daemon, concurrency, or transports.
2. **Embedded Agent Skills (`SKILL.template.md`, `SKILL-admin.template.md`):**
   - Update `assets/SKILL.template.md` (calling tools), `assets/SKILL-admin.template.md` (changing the setup) and
     `crates/utcp-core/src/skills.rs` whenever agent-facing CLI workflows, flags, or target skill paths change.
3. **Documentation & Issue Log:**
   - Keep `docs/operating.md` (the operating guide, installed next to the admin skill), `docs/utcp-rs-spec.md`
     (including its rendered `SKILL.md` example and CLI contract table), `README.md`, and the repository root
     `README.md` (project index entry) accurate.
   - Log any newly discovered upstream `rs-utcp` bugs or patches as a numbered entry in `issues/` and `issues/README.md`.
4. **Packaging & CI Artifacts:**
   - Keep `packaging/systemd/`, `packaging/windows/` (`install.ps1`, `install.cmd`, `README.txt`) and
     `.github/workflows/utcp-rs.yml` aligned with CLI subcommands, flags and the files the release archives ship.
   - Only `systemd` is supported. Other init systems are welcome as contributions but are not maintained here.
   - **Versioning:** `version` in `Cargo.toml` (`[workspace.package]`) is the version of the whole `utcp-rs` release,
     not just the Rust code, and the only place it is set. Any change to what a release ships (Rust code, the
     systemd unit, `packaging/windows/` files, `operating.md`, the skill templates) needs a new version before it is
     released, even if no Rust code changed (for example `0.1.0` -> `0.1.1`). `utcp --version` reports it.
   - **Releasing:** set the new `version`, build so `Cargo.lock` follows, commit and push to `main`. Then run the
     workflow on `main` with `release` ticked (`gh workflow run utcp-rs.yml --ref main -f release=true`). CI reads
     the version from `Cargo.toml`, creates the tag `utcp-rs-vX.Y.Z` and the GitHub Release; never tag by hand. It
     refuses, before building, a version that is already released or lower than the latest release. Changes only to
     the Gentoo ebuild live in `adaptive-overlay` and use an ebuild revision (`-r1`) instead of a new version here.
