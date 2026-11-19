# Issue 007: `CliTransport` Hardcodes `call <provider> <tool>` and Ignores Exit Codes

- **Upstream Crate:** `rs-utcp` `v0.3.2`
- **Upstream Files:** `src/providers/cli/mod.rs`, `src/transports/cli/mod.rs`
- **Local Status:** Fixed in `utcp-rs/crates/utcp-core`
- **Upstream Issue / PR:** Not filed yet

## Problem

Upstream `CliTransport::call_tool` assumes every CLI executable implements a custom UTCP subcommand interface:

```rust
cmd_args.extend(["call".to_string(), cli_prov.base.name.clone(), tool_name.to_string()]);
```

and pipes the JSON-serialized `args` map to `stdin`. Real system binaries (such as `/usr/bin/d2`, `git`, or a
container management script) reject the injected `call <provider> <tool>` arguments. In addition, when a command exits
with a non-zero status code, upstream returns `Ok(Value::String(stderr))` instead of returning an `Err`.

## Local Fix in `utcp-rs`

Added opt-in `direct_exec` mode to `CliProvider` (`direct_exec`, `args_template`, `stdin_field`, `append_extra_flags`)
while preserving legacy fallback behavior when unset:

- Substitutes `{param}` placeholders per argument token without invoking a shell, automatically omitting tokens whose
  optional `{param}` was not supplied.
- Pipes raw string content from `stdin_field` to `stdin` when configured (for example, `d2 fmt -` and `d2 validate -`).
- Returns a structured `Err` whenever `exit_code != 0`.
