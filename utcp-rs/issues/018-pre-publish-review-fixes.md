# Issue 018: Bugs Found in the Pre-Publish Review (Hung CLI Tools, Panics, Secrets, Media Folder, Refresh)

- **Upstream Crate:** `rs-utcp` `v0.3.2` (items 1, 2 and 3); the rest is local to `utcp-rs`
- **Upstream Files:** `src/transports/cli/mod.rs`, `src/plugins/codemode/mod.rs` (`sprintf`),
  `src/transports/webrtc/mod.rs`
- **Local Files:** `crates/utcp-core/src/transports/cli/mod.rs`, `crates/utcp-core/src/plugins/codemode/mod.rs`,
  `crates/utcp-core/src/transports/webrtc/mod.rs`, `crates/utcp-core/src/openapi/mod.rs`, `Cargo.toml`,
  `crates/utcp-core/src/paths.rs`, `crates/utcp-core/src/catalog.rs`, `crates/utcp-core/src/caller.rs`,
  `crates/utcp-cli/src/main.rs`, `packaging/systemd/utcp-daemon.service`
- **Local Status:** Fixed in `utcp-rs`
- **Upstream Issue / PR:** Not filed yet

## How They Were Found

A code and documentation review before the repository was made public.

## Problems and Fixes

1. **A `cli` tool that hangs keeps running after its call timed out.** The call gave up after 30 s, but the child
   process was never stopped, and writing a large stdin to a tool that does not read it was outside the timeout.
   Every hung call left one more process behind, in the daemon for its whole life. **Fix:** `kill_on_drop(true)`,
   and one timeout (`CLI_TIMEOUT`) covers the stdin write and the wait. The error says the tool "did not finish
   within 30 s and was stopped". Test: `timed_out_tool_is_stopped` (fails without `kill_on_drop`).
2. **`codemode`'s `sprintf` panics on non-ASCII text.** Its length limits cut strings at a byte index
   (`&rendered[..1000]`, `truncate`), which panics inside a multi-byte character. **Fix:** cut at the nearest
   character boundary below the limit (`char_floor`). Test: `security_sprintf_truncates_non_ascii_without_panicking`.
3. **WebRTC signaling used a plain `reqwest::Client`**, without the redirect-SSRF policy and User-Agent of every
   other HTTP path (issues 005 and 008). **Fix:** it uses `build_secure_http_client()`.
4. **`serde_yml` is unsound and unmaintained** (RUSTSEC-2025-0068). **Fix:** YAML specs are parsed with
   `serde_norway`, a maintained fork of `serde_yaml` with the same API.
5. **The systemd unit loaded `~/.config/utcp/.env` into the daemon's environment** (`EnvironmentFile=`). utcp reads
   that file itself and keeps the values in its own map, so the line only handed every secret to every CLI tool and
   MCP server the daemon starts. **Fix:** the line is removed.
6. **The media folder could be a symlink or another user's folder.** On a shared machine without `XDG_RUNTIME_DIR`,
   the fallback is in `/tmp`, where another user could create it first and read the images. **Fix:** on Unix the
   folder must be a real directory (not a symlink) owned by the user; group and other permissions are removed. If
   not, the image stays inline in the answer and the error says to set `UTCP_MEDIA_DIR` or `media_dir`. Test:
   `media_dir_must_be_a_private_directory_owned_by_the_user`.
7. **`utcp refresh` did nothing for `import-mcp` manuals.** Their registered source is the saved copy, so refresh
   re-read the file it had just written. **Fix:** refresh recovers the MCP server (command, arguments and
   environment, or URL and headers, with their `${VAR}` placeholders) from the saved manual and asks it again.
   Existing registrations need no change. Test: `refresh_asks_the_mcp_server_again` (fails without the fix).
8. **`utcp import --register -o <file>` ignored `-o`.** **Fix:** it also writes a copy there, as Tab 3 of `utcp ui`
   does, and prints it as `Copy`.
9. **Caller names were matched by substring**, so a process named, for example, `encoder` was reported as `vscode`
   (`code`). **Fix:** exact names for Zed and VS Code builds and for `agy`; names containing `antigravity` or `hermes`
   still match. Test: `normalize_proc_name_matches_whole_names`.
