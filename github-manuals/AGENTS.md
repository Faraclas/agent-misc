# Agent Instructions (`github-manuals`)

Paths below are relative to this `github-manuals/` folder unless they start with `.github/` (repository root).

## What this project is

A generator, written in Rust, that turns GitHub's OpenAPI description of the REST API (`github/rest-api-description`,
one file per API version) into utcp HTTP manuals plus a report. A standalone crate with its own `Cargo.toml` and
`Cargo.lock`, not part of the `utcp-rs` workspace.

- `src/config.rs`: the manuals, which category goes where, every rule that leaves operations out (with its reason),
  and the naming tables. `src/names.rs`: tool names. `src/generate.rs`: description to manuals and report (no network).
- `src/source.rs`: the pinned description (`pins.json`, embedded at build time), its hash-checked download and cache,
  the default folders, and `pins`. `src/http.rs`: the blocking HTTP layer. `src/utcp.rs`: runs utcp for `--stage`
  and `search-test`. `src/search_test.rs`: `search-test`, with its cases in `tests/search-checks.txt`.
- `src/cli.rs`: commands. `src/tests.rs`: the offline tests, with the hand-made description in `tests/fixtures/`.
- `docs/operating.md` (for users: token, upgrades, troubleshooting) and `docs/github-manuals-spec.md` (how it works
  inside, decisions).

## Rules

1. **Never vendor GitHub's description.** It is downloaded on demand and verified against `pins.json`. Hand-made
   fixtures in `tests/fixtures/` are fine; copies of the real file are not. Generated manuals are not committed
   either (they are build output, and change with every pin).
2. **Coverage: everything, unless a rule gives a reason.** Every operation becomes a tool unless a rule in
   `Config::rules` (or the deprecation, HEAD and utcp-limit checks in `generate.rs`) leaves it out. The rules record a
   review with the user: organization and enterprise administration, areas not used, and dangerous operations
   (access grants, repository settings, protection, security settings, secrets, webhooks). Do not add a rule to limit
   what agents may do in general; the token does that. A new rule needs a short, true reason, and a matching row in
   the README's "What is left out" table. Every included category must have a manual: a category GitHub adds makes
   `generate` fail until it is placed or left out.
3. **Changing what is generated** (`src/config.rs`, `src/names.rs`, `src/generate.rs`): run the offline tests,
   rewrite the expected files with `UPDATE_EXPECTED=1 cargo test`, and review that diff before committing. Then
   regenerate the real manuals and check them in utcp: `generate --stage`, `utcp diff`, `search-test --staged`,
   `utcp schema --staged` and `utcp call --staged --dry-run`. A `search-test` failure is fixed with the words people
   use (`SUMMARIES`, `NOTES`) or reported to utcp-rs as a ranking bug, never with odd wording to pass. Never point
   `UTCP_CONFIG_DIR` or `HOME` at a scratch folder to try manuals: stage them instead. Staging is shared: promote or
   discard by name (`utcp discard github_repos`), never everything.
4. **Tool names are an interface.** Renaming a tool breaks agents that learned it. Change the naming rules or
   `Config::names` only on purpose, and list renamed tools in the release notes. A clash is an error, never resolved
   silently.
5. **Writes:** all included writes are generated; those not in `config::TESTED_WRITES` start their description with
   `(untested)` and carry the tag `untested`. A write moves to `TESTED_WRITES` only after it worked live against a
   throwaway repository. Every DELETE's description has `generate::DELETE_NOTE` right after its summary.
6. **utcp limits are not worked around here.** When utcp cannot send what an operation needs (a body on DELETE, a
   plain-text body), the operation is left out with a reason naming the limit, and the fix is a utcp-rs change.
7. **HTTP behaves like utcp's.** `src/http.rs` uses `ureq` with `platform-verifier` (system CA store), like
   azdo-manuals; keep the two in step.

## Testing

- Offline (CI, `.github/workflows/github-manuals.yml`): `cargo fmt --check`,
  `cargo clippy --all-targets --locked -- -D warnings` and `cargo test --locked`. Set `UTCP_BIN` to a utcp binary
  (0.2.4 or newer) to include the load and `search-test` tests; CI uses the latest `utcp-rs` release on Linux and sets
  `REQUIRE_UTCP=1`. `search-test` itself needs the real manuals, so it is run by hand (rule 3), not in CI.
- On a machine without the source tree (a Windows test machine), copy the compiled test binary with
  `tests/fixtures/` and set `GITHUB_MANUALS_TEST_FIXTURES` to that copy.
- Live checks use a real token and real repositories: reads against public repositories (any owner) are fine; writes
  only in a throwaway repository of the token's owner. Their output stays out of the repository.

## Lockstep docs

Keep `README.md` (installation, manuals, token, what is left out, old -> new names), `docs/operating.md`,
`docs/github-manuals-spec.md`, this file, and the repository root `README.md` index entry in step with the code in
the same commit. Update the expected files in `tests/fixtures/expected/` whenever the output changes. Keep
`.github/workflows/github-manuals.yml` aligned with the files the release archives ship (the binary, `README.md`,
`docs/operating.md`, `NOTICE`, `LICENSE`).

## Versioning and releases

- `version` in `Cargo.toml` is the version of a `github-manuals` release and the only place it is set. It is an
  ordinary semantic version, independent of GitHub's API version; the manuals state the API version they target
  (`config::API_VERSION`, checked against `pins.json` by a test).
- Any change to what a release ships needs a new version. Changing `pins.json`, `src/config.rs` or the naming rules
  changes the generated manuals, so it needs a new version too (a minor version when tool names change).
- **Releasing:** set the new `version`, build so `Cargo.lock` follows, commit and push to `main`. Then run the
  workflow on `main` with `release` ticked (`gh workflow run github-manuals.yml --ref main -f release=true`). CI reads
  the version from `Cargo.toml`, creates the tag `github-manuals-vX.Y.Z` and the GitHub Release with Linux and Windows
  archives; never tag by hand. Gentoo packaging lives in `adaptive-overlay`.
