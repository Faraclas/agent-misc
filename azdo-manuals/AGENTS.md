# Agent Instructions (`azdo-manuals`)

Paths below are relative to this `azdo-manuals/` folder unless they start with `.github/` (repository root).

## What this project is

A generator, written in Rust, that turns Microsoft's Azure DevOps REST API specs (Swagger 2.0, from the Software
Heritage archive of the deleted `MicrosoftDocs/vsts-rest-api-specs` repository) into utcp HTTP manuals plus a PAT
scope report. With the PAT it also joins Microsoft's Go and Node client libraries (downloaded from GitHub) to the
organization's live route list, to add the endpoints the specs leave out, and writes an Analytics manual from the
Analytics service's OData `$metadata`. A standalone crate with its own `Cargo.toml` and `Cargo.lock`, not part of the
`utcp-rs` workspace.

- `src/config.rs`: manual grouping and everything left out, with reasons. `src/bodies.rs`: bodies of the tested writes.
- `src/generate.rs`: specs to manuals and report (no network).
- `src/archive.rs`: archive client, hash-verified cache, `pins` discovery. `pins.json` is embedded at build time.
- `src/sdk.rs` (library model, pinned tarball download), `src/sdk_go.rs`, `src/sdk_node.rs` (parsers),
  `src/live.rs` (`resourceAreas` and route lists), `src/sdkops.rs` (the join: library methods to one-operation
  specs), `src/analytics.rs` (`$metadata` to OData tools).
- `src/routes.rs`, `src/readtest.rs`, `src/writetest.rs`: the live checks; `src/utcp.rs` runs utcp for them
  (`read-test` and `generate --stage` use the user's real utcp; only `write-test` uses an isolated setup).
- `src/cli.rs`: commands (including `route-gaps` and `pins-sdk`). `src/pat.rs`: scope ids as the New Token page names
  them. `src/tests.rs` and `src/tests_sdk.rs`: the offline tests.
- `docs/operating.md` (for users: PAT, upgrades, test results, troubleshooting) and `docs/azdo-manuals-spec.md` (how
  it works inside, file formats, decisions).

## Rules

1. **Never commit generated manuals or anything naming a real organization.** Generated manuals contain the org name
   in every URL. They belong in a private place chosen by the user, never in this repository. Tests use the made-up
   org `contoso`. Live test output (route checks, response samples) stays in gitignored scratch folders.
2. **Never vendor Microsoft's files.** Spec files are downloaded on demand from the archive and verified by git blob
   hash against `pins.json`; client library files the same way from GitHub's tarballs (`pins.json` `sdk`). Hand-made
   fixtures in `tests/fixtures/` (in the shape of the real files, with made-up content) are fine; copies of real spec
   or library files are not. No hand-written operation files either: what the specs lack comes from the libraries
   and the live route list, read at run time.
3. **The archive allows 120 requests per hour** and refuses requests without an identifying User-Agent. A normal
   `generate` costs one request per pinned file not yet cached (46 files), plus one GitHub tarball per client library
   whose files are not cached. `pins` costs 3 plus 2 per area and continues where it stopped. Do not add code paths
   that list folders on every run.
4. **Changing what is generated** (`src/config.rs`, `src/generate.rs`, `src/sdkops.rs`, the parsers,
   `src/analytics.rs`): run the offline tests, rewrite the expected files with `UPDATE_EXPECTED=1 cargo test`, and
   review that diff before committing. Then regenerate the real manuals with the PAT and check them in utcp:
   `generate --stage`, `utcp diff 'azdo_*'` (what changes; spec tools must not change unless that was the point),
   `check-routes`, `utcp schema --staged` and `utcp call --staged --dry-run`, and `read-test` (one manual per run);
   `utcp promote 'azdo_*'` only once they are right (`utcp discard 'azdo_*'` otherwise; never a bare or `--all`
   promote, which would also take other staged manuals).
   Never point `UTCP_CONFIG_DIR` or `HOME` at a scratch folder to try manuals: stage them instead.
5. **Coverage: everything, unless there is a stated reason.** Every area of the spec folder goes into a manual or into
   `excluded_areas` with its reason (a test enforces this), and every operation of an included area becomes a tool
   unless it cannot work or cannot be undone: HEAD, operations found broken
   (`skip_gets`, with what the service answered), and DELETEs not listed in `deletes`. The same holds for
   client-library operations: every live endpoint form the spec lacks becomes a tool, unless its route area is in
   `excluded_route_areas` (with the reason), it starts after the manuals' API version, or a rule above applies.
   `route-gaps` lists what is left. File answers and file uploads are included (utcp saves and reads the files). Do
   not leave tools out to
   limit what agents may do; the PAT's permissions do that, and `generate` prints them for every tool and for reads
   only. `generate` prints every exclusion; keep its reasons short and true.
6. **Writes:** all POST, PUT and PATCH operations are generated; those not in `tested_writes` start their description
   with `(untested)` and carry the tag `untested`. A write moves to `tested_writes` only after it passed `write-test`
   in a sandbox organization (a free personal organization, never the user's work organization); it then gets a
   hand-written body in `src/bodies.rs` (a test enforces this) and a `write-test` step that sends every field of that
   body and checks the answer shows it. A DELETE is added to `deletes` only when it removes something small an agent
   added and may need to take back (a comment, a label, a tag), never a project, repository, pipeline, feed, package,
   user or permission.
7. **Request-side fixes** borrowed from Microsoft's `vsts-api-patcher` (MIT) are credited in `NOTICE`.
8. **HTTP behaves like utcp's.** This crate uses `ureq` (blocking, no async runtime) while utcp uses `reqwest` on
   tokio; that is deliberate, a short-lived CLI does not need tokio. The two libraries have different defaults, so
   check network behavior against utcp's whenever `src/http.rs` or the `ureq` features change: trusted CAs come from
   the system store (`platform-verifier`, so a CA installed for TLS inspection works), proxies come from
   `HTTPS_PROXY`/`ALL_PROXY` and the Windows system proxy is read by neither. Change both projects together, or say
   why they differ.

## Testing

- Offline (CI, `.github/workflows/azdo-manuals.yml`): `cargo fmt --check`,
  `cargo clippy --all-targets --locked -- -D warnings` and `cargo test --locked`. Set `UTCP_BIN` to a utcp binary
  (0.2.4 or newer; an older one is skipped) to include the load test; CI uses the latest `utcp-rs` release on Linux
  and sets `REQUIRE_UTCP=1` so a missing binary fails instead of skipping.
- On a machine without the source tree (for example a Windows test machine), copy the compiled test binary with
  `tests/fixtures/` and set `AZDO_TEST_FIXTURES` to that copy.
- Live checks against a real organization are run by hand: `check-routes` (every tool's URL against the
  organization's route list), `route-gaps` (live endpoints without a tool) and `read-test` (every read tool called
  through the user's utcp, `--staged` for staged manuals) are read-only. `write-test` (every tested
  write tool, in a throwaway project it creates and deletes) is for the sandbox organization only. Their output
  names the organization's data: keep it next to the generated manuals, never in the repository. When a live check
  shows the spec is wrong, fix it in `src/config.rs` (`required_params`, `skip_gets`) or `src/bodies.rs` with a
  comment saying what the service answered, rather than in the test.
- Operations whose spec lists no scope may still be refused by a scoped PAT. Check new ones with a PAT limited to
  the scopes in the report, not only with a full-access PAT. Client-library operations have inferred scopes; a read
  that answers 401 to such a PAT goes to `skip_gets` with that answer.

## Lockstep docs

Keep `README.md` (installation, usage, manual list, scope report), `docs/operating.md`, `docs/azdo-manuals-spec.md`,
this file, and the repository root `README.md` index entry in step with the code in the same commit. Update the
expected files in `tests/fixtures/expected/` whenever the output format changes. Keep
`.github/workflows/azdo-manuals.yml` aligned with the files the release archives ship (the binary, `README.md`,
`docs/operating.md`, `NOTICE`, `LICENSE`).

## Versioning and releases

- `version` in `Cargo.toml` is the version of an `azdo-manuals` release and the only place it is set. It is also sent
  in the archive User-Agent and reported by `azdo-manuals --version`.
- **The version is the REST API version plus a counter:** `7.1.0`, `7.1.1`, ... for manuals of API 7.1
  (`config::API_VERSION`; a test enforces that the version starts with it). Never let them drift: moving to API 7.2
  means `7.2.0`. Several series may be maintained side by side later (for example a stable 7.1.x and a newer
  7.2.x); CI does not support that yet, decide how when it is needed.
- Any change to what a release ships needs a new version before it is released (for example `7.1.0` -> `7.1.1`).
  Changing `pins.json` (including its `sdk` part) or `src/config.rs` changes the generated manuals, so it needs a new
  version too.
- The manuals need utcp 0.2.4 or newer (`MIN_UTCP` in `src/cli.rs`, checked by `generate --stage`, `read-test` and
  `write-test`): it leaves out the segment of an optional path parameter that is not passed, and has the
  `utcp diff`/`promote 'azdo_*'` the docs use. Keep the Gentoo ebuild's dependency in step (0.2.4 since 7.1.3).
- **Releasing:** set the new `version`, build so `Cargo.lock` follows, commit and push to `main`. Then run the
  workflow on `main` with `release` ticked (`gh workflow run azdo-manuals.yml --ref main -f release=true`). CI reads
  the version from `Cargo.toml`, creates the tag `azdo-manuals-vX.Y.Z` and the GitHub Release; never tag by hand. It
  refuses, before building, a version that is already released or lower than the latest release. Changes only to a
  Gentoo ebuild live in `adaptive-overlay` and use an ebuild revision (`-r1`) instead of a new version here.
