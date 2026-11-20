# Issue 020: On Windows, `--command npx` Fails With "program not found"

- **Upstream Crate:** none; local to `utcp-rs`
- **Local Files:** `crates/utcp-core/src/program.rs` (new), `crates/utcp-core/src/transports/mcp/mod.rs`,
  `crates/utcp-core/src/transports/cli/mod.rs`, `crates/utcp-core/src/secrets.rs`
- **Local Status:** Fixed in `utcp-rs` 0.1.3
- **Upstream Issue / PR:** Not applicable

## How It Was Found

While moving a Windows machine's MCP servers to utcp, the documented example failed:

```text
> utcp import-mcp probe --command npx --arg -y --arg @scope/server
Error: failed to start MCP server command 'npx': program not found
```

`--command npx.cmd` worked. Node.js installs `npx` as `npx.cmd`, and the Azure CLI installs `az` as `az.cmd`.

## Problem

Rust's `std::process::Command` searches `PATH` for a bare name plus `.exe` only. It never tries the other extensions
in `PATHEXT`, as `cmd.exe` and PowerShell do. Every launcher that is a batch file (`npx`, `npm`, `az`, many Python
tool shims) therefore failed when named the way the docs, the skill and server READMEs name it. The same applied to a
`cli` tool's program and to the `[secrets]` command.

## Fix

`utcp_core::program::resolve` (and `resolve_with_env`) look a bare name up on `PATH` with the `PATHEXT` extensions
Windows can start without a file association (`.com`, `.exe`, `.bat`, `.cmd`): the first folder that has any of them
wins, and `PATHEXT` order decides within a folder. A name with a folder (`\`, `/`, `:`) or one of those extensions,
or one that is not found, is passed on unchanged, so the error stays "program not found". When a manual sets `PATH`
in its environment (`-e PATH=...`), that `PATH` is searched, as `Command` does for a child with its own `PATH`. Other
systems are unchanged.

Rust starts the `.cmd` and `.bat` files it gets through `cmd.exe` with its own argument escaping, and the existing
Job Object still stops the whole tree (`cmd.exe`, then `node.exe`).

Used by the stdio MCP transport, the `cli` transport and the secrets command. Text-transport scripts (`node`,
`bash`, `python3`) are left as they are.

## Tests

- `program.rs` unit tests (run on every system): `PATH` order, `PATHEXT` order within a folder, the default order
  when `PATHEXT` is missing or lists nothing startable, and names left alone (folders, drive prefixes, existing
  extensions in any case, unknown names, empty names, no `PATH`). On Windows, `cmd` resolves to `...\cmd.exe`.
- `bare_command_name_finds_cmd_launcher_on_windows_path` (integration, Windows only): a `mocknode.cmd` on utcp's
  `PATH` and an `ownnode.cmd` only on the manual's own `-e PATH=...`, both imported with a bare `--command` and
  called in direct mode and through the daemon.
- Run on Windows 11 with the cross-built test binaries; `cargo clippy` for `x86_64-pc-windows-gnu` is clean.
