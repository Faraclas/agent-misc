# Agent Instructions (`azdo-manuals`)

Paths below are relative to this `azdo-manuals/` folder unless they start with `.github/` (repository root).

## What this project is

A generator, written in Rust, that turns Microsoft's Azure DevOps REST API specs (Swagger 2.0, from the Software
Heritage archive of the deleted `MicrosoftDocs/vsts-rest-api-specs` repository) into utcp HTTP manuals plus a PAT
scope report. A standalone crate with its own `Cargo.toml` and `Cargo.lock`, not part of the `utcp-rs` workspace.

- `src/config.rs`: manual grouping and every inclusion decision. `src/bodies.rs`: hand-written write bodies.
- `src/generate.rs`: specs to manuals and report (no network).
- `src/archive.rs`: archive client, hash-verified cache, `pins` discovery. `pins.json` is embedded at build time.
- `src/routes.rs`, `src/readtest.rs`, `src/writetest.rs`: the live checks; `src/utcp.rs` runs utcp for them.
- `src/cli.rs`: commands. `src/tests.rs`: the offline tests.

## Rules

1. **Never commit generated manuals or anything naming a real organization.** Generated manuals contain the org name
   in every URL. They belong in a private place chosen by the user, never in this repository. Tests use the made-up
   org `contoso`. Live test output (route checks, response samples) stays in gitignored scratch folders.
2. **Never vendor Microsoft's spec files.** They are downloaded on demand from the archive and verified by git blob
   hash against `pins.json`. Hand-made fixtures in `tests/fixtures/` are fine; copies of real spec files are not.
3. **The archive allows 120 requests per hour** and refuses requests without an identifying User-Agent. A normal
   `generate` costs one request per pinned file not yet cached (21 files). `pins` costs 3 plus 2 per area and
   continues where it stopped. Do not add code paths that list folders on every run.
4. **Changing what is generated** (`src/config.rs`, `src/generate.rs`): run the offline tests, rewrite the expected
   files with `UPDATE_EXPECTED=1 cargo test`, and review that diff before committing. Then regenerate the real
   manuals and check them in utcp (`register`, `schema`, `call --dry-run`) in an isolated config (`UTCP_CONFIG_DIR`,
   `UTCP_CACHE_DIR`, `UTCP_STATE_DIR` pointing at a scratch folder).
5. **Writes** (`writes` in `src/config.rs`) are added only after they passed `write-test` in a sandbox organization
   (a free personal organization, never the user's work organization). Each write gets a hand-written body in
   `src/bodies.rs` (a test enforces this), and `write-test` gets a step that sends every field of that body and checks
   the answer shows it; a write tool without a step is reported as not run. DELETE and HEAD are never generated.
6. **Request-side fixes** borrowed from Microsoft's `vsts-api-patcher` (MIT) are credited in `NOTICE`.
7. **HTTP behaves like utcp's.** This crate uses `ureq` (blocking, no async runtime) while utcp uses `reqwest` on
   tokio; that is deliberate, a short-lived CLI does not need tokio. The two libraries have different defaults, so
   check network behavior against utcp's whenever `src/http.rs` or the `ureq` features change: trusted CAs come from
   the system store (`platform-verifier`, so a CA installed for TLS inspection works), proxies come from
   `HTTPS_PROXY`/`ALL_PROXY` and the Windows system proxy is read by neither. Change both projects together, or say
   why they differ.

## Testing

- Offline (CI, `.github/workflows/azdo-manuals.yml`): `cargo fmt --check`,
  `cargo clippy --all-targets --locked -- -D warnings` and `cargo test --locked`. Set `UTCP_BIN` to a utcp binary to
  include the load test; CI uses the latest `utcp-rs` release on Linux and sets `REQUIRE_UTCP=1` so a missing binary
  fails instead of skipping.
- Live checks against a real organization are run by hand: `check-routes` (every tool's URL against the
  organization's route list) and `read-test` (every read tool called through utcp) are read-only. `write-test` (every
  write tool, in a throwaway project it creates and deletes) is for the sandbox organization only. Their output
  names the organization's data: keep it next to the generated manuals, never in the repository. When a live check
  shows the spec is wrong, fix it in `src/config.rs` (`required_params`, `skip_gets`) or `src/bodies.rs` with a
  comment saying what the service answered, rather than in the test.
- Operations whose spec lists no scope may still be refused by a scoped PAT. Check new ones with a PAT limited to
  the scopes in the report, not only with a full-access PAT.

## Lockstep docs

Keep `README.md` (installation, usage, manual list, scope report), this file, and the repository root `README.md`
index entry in step with the code in the same commit. Update the expected files in `tests/fixtures/expected/`
whenever the output format changes. Keep `.github/workflows/azdo-manuals.yml` aligned with the files the release
archives ship.

## Versioning and releases

- `version` in `Cargo.toml` is the version of an `azdo-manuals` release and the only place it is set. It is also sent
  in the archive User-Agent and reported by `azdo-manuals --version`. Any change to what a release ships needs a new
  version before it is released (for example `0.1.0` -> `0.1.1`). Changing `pins.json` or `src/config.rs` changes the
  generated manuals, so it needs a new version too.
- **Releasing:** set the new `version`, build so `Cargo.lock` follows, commit and push to `main`. Then run the
  workflow on `main` with `release` ticked (`gh workflow run azdo-manuals.yml --ref main -f release=true`). CI reads
  the version from `Cargo.toml`, creates the tag `azdo-manuals-vX.Y.Z` and the GitHub Release; never tag by hand. It
  refuses, before building, a version that is already released or lower than the latest release. Changes only to a
  Gentoo ebuild live in `adaptive-overlay` and use an ebuild revision (`-r1`) instead of a new version here.
