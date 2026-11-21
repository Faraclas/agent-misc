# Agent Instructions (`azdo-manuals`)

Paths below are relative to this `azdo-manuals/` folder unless they start with `.github/` (repository root).

## What this project is

A generator that turns Microsoft's Azure DevOps REST API specs (Swagger 2.0, from the Software Heritage archive of
the deleted `MicrosoftDocs/vsts-rest-api-specs` repository) into utcp HTTP manuals plus a PAT scope report. Python 3
standard library only: no third-party packages, so it runs anywhere Python 3.9+ does.

## Rules

1. **Never commit generated manuals or anything naming a real organization.** Generated manuals contain the org name
   in every URL. They belong in a private place chosen by the user, never in this repository. Tests use the made-up
   org `contoso`. Live test output (route checks, response samples) stays in gitignored scratch folders.
2. **Never vendor Microsoft's spec files.** They are downloaded on demand from the archive and verified by git blob
   hash against `pins.json`. Hand-made fixtures in `tests/fixtures/` are fine; copies of real spec files are not.
3. **The archive allows 120 requests per hour** and refuses requests without an identifying User-Agent. A normal
   `generate` costs one request per pinned file not yet cached (21 files). `pins` costs 4 plus 2 per area and
   continues where it stopped. Do not add code paths that list folders on every run.
4. **Changing what is generated** (`config.py`, `generate.py`): run the offline tests, rewrite the expected files with
   `UPDATE_EXPECTED=1`, and review that diff before committing. Then regenerate the real manuals and check them in
   utcp (`register`, `schema`, `call --dry-run`) in an isolated config (`UTCP_CONFIG_DIR`, `UTCP_CACHE_DIR`,
   `UTCP_STATE_DIR` pointing at a scratch folder).
5. **Writes** (`config.WRITES`) are added only after they passed in the sandbox organization (a free personal
   organization, never the user's work organization). DELETE and HEAD are never generated.
6. **Request-side fixes** borrowed from Microsoft's `vsts-api-patcher` (MIT) are credited in `NOTICE`.

## Testing

- Offline (CI, `.github/workflows/azdo-manuals.yml`): `python3 -m unittest discover -s tests -v`. Set `UTCP_BIN` to a
  utcp binary to include the load test; CI uses the latest `utcp-rs` release on Linux.
- Live checks against a real organization are run by hand: `check-routes` (every tool's URL against the
  organization's route list) and `read-test` (every read tool called through utcp). Both are read-only. Their output
  names the organization's data: keep it next to the generated manuals, never in the repository. When a live check
  shows the spec is wrong, fix it in `config.py` (`REQUIRED_PARAMS`, `SKIP_GETS`) with a comment saying what the
  service answered, rather than in the read test.

## Lockstep docs

Keep `README.md` (usage, manual list, scope report), this file, and the repository root `README.md` index entry in
step with the code in the same commit. Update the expected files in `tests/fixtures/expected/` whenever the output
format changes.

## Versioning

`__version__` in `azdo_manuals/__init__.py` (also sent in the archive User-Agent). No releases or tags yet: users run
it from a checkout.
