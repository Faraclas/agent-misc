# azdo-manuals specification

- **Version:** describes `azdo-manuals` 7.1.1 (REST API 7.1). Keep it in step with the code (see
  [`AGENTS.md`](../AGENTS.md), "Lockstep docs").
- **Audience:** maintainers and agents changing the generator. Users start with the [README](../README.md) and the
  [operating guide](./operating.md).

## 1. Purpose and boundaries

`azdo-manuals` turns Microsoft's Azure DevOps REST API descriptions (Swagger 2.0) into utcp HTTP manuals for one
organization, so agents reach Azure DevOps through utcp (`search`, `schema`, `call`) instead of an MCP server.

- It only writes files and, with `--stage`, hands them to `utcp stage`. It never registers, promotes or edits utcp's
  configuration itself. utcp owns staging, secrets, the daemon and the agent skills.
- It never stores Microsoft's files in the repository (they are downloaded and verified at run time) and never
  stores anything naming a real organization (the tests use `contoso`).
- It does not limit what agents may do beyond leaving out operations that cannot work or cannot be undone. The PAT's
  permissions decide the rest.

## 2. Pipeline

```text
pins.json (built in) -> archive download (cache, hash check) -> specs per area
config (src/config.rs, src/bodies.rs) ---------------------------> generate -> <manual>.utcp.json x 8
                                                                            -> report.json, scopes.md
                                                                            -> utcp stage (with --stage)
generated manuals -> check-routes (live route list)   read-test (utcp call)   write-test (sandbox only)
```

| Command | Network | Needs the PAT | Writes |
| :--- | :--- | :--- | :--- |
| `generate --org <org> [--out] [--cache] [--stage] [--utcp] [--read-only]` | archive, uncached files only | no | manuals folder; utcp staging with `--stage` |
| `check-routes --org <org> [--manuals]` | Azure DevOps, `OPTIONS` only | from the environment | nothing |
| `read-test (--org <org> or --manuals) --project [--team] [--seed]... [--only]... [--utcp]` | through utcp | utcp's | `readtest.json` |
| `write-test --org <sandbox> --manuals --create-and-delete-projects [--utcp]` | Azure DevOps, through an isolated utcp | from the environment | a throwaway project; `writetest.json` |
| `pins [--revision] [--api-version] [--fresh] [--pins]` (maintainers) | archive | no | the pins file |
| `help [<command>]` | none | no | nothing |

`azdo-manuals <command> help` is rewritten to `azdo-manuals help <command>` (no command takes a positional argument).
Errors print as `azdo-manuals: <message>`; exit code 2 for usage problems (missing PAT, no manuals, no utcp), 1 for
failures, 0 for success. `check-routes`, `read-test` and `write-test` exit with 1 when any tool failed.

## 3. Source and pinning

- **Source:** the Software Heritage archive's copy of the deleted GitHub repository `MicrosoftDocs/vsts-rest-api-specs`,
  revision `c7e66d98` (2026-09-02, "Update REST API specs from releases/M279"), folder `specification/<area>/7.1/`.
- **`pins.json`**, embedded with `include_str!`: `revision`, `revision_date`, `revision_message`,
  `specification_dir`, `api_version`, `archive_areas` (every area folder of the revision), `areas` (per area, each
  file's `file`, `sha1_git` and `length`) and `missing` (areas without a folder for `api_version`, with the versions
  they have). Rebuild the binary after changing it.
- **Download:** one request per pinned file not in the cache, by content hash (`/content/sha1_git:<hash>/raw/`). The
  content's git blob hash must equal the pin, else the run fails and nothing is cached. The cache is keyed by hash,
  so it never goes stale and is shared by all organizations.
- **Archive limits:** 120 requests per hour, and a 403 for requests without an identifying User-Agent (it sends
  `azdo-manuals/<version> (+https://github.com/Faraclas/agent-misc)`). A 429 is reported with the reset time. A cold
  `generate` costs 46 requests.
- **`pins`** lists the revision's folders and records the pins: about 3 requests plus 2 per area. It saves after
  every area and continues an existing pins file (unless `--fresh`), so a run cut off by the rate limit resumes.

## 4. Configuration

All decisions live in `src/config.rs` (`Config::real()`) and `src/bodies.rs`. Every operation named in the
configuration must exist in the specs (`generate` refuses a typo), and a test refuses an archive area that is neither
in a manual nor in `excluded_areas`.

| Field | Meaning |
| :--- | :--- |
| `manuals` | Name, areas (in order: the first area wins name clashes), title (the manual's `info.title`) and hint. |
| `excluded_areas` | Areas in no manual, each with a reason (`account`, `tokens`, `tokenAdmin`, `tokenAdministration`, `governance`). |
| `post_reads` | POSTs whose only effect is to return data (queries, batch reads, searches, lookups). They count as reads. |
| `tested_writes` | Writes covered by `write-test`; each needs a body in `bodies.rs` (a test enforces this). |
| `deletes` | The DELETEs that are generated (19 small ones an agent may need to take back). |
| `skip_gets` | GETs left out, with the service's answer as the reason (deprecated, or no scope a read PAT can have). |
| `required_params` | Parameters the spec calls optional but the service requires. |
| `names` | Tool names set by hand, for clashes or names that say too little. |
| `drop_optional_path_params` | Optional path parameters removed with their segment (`team` in `wit`). |
| `body_overrides` | Request body schemas replacing the spec's (`bodies::real()`). |
| `host_overrides` | Hosts replacing the spec's, per area (`profile` -> `vssps.dev.azure.com`; the spec's host refuses an organization's PAT). A test refuses an area no manual includes. |
| `read_only` | Set by `--read-only`: no writes, and no reads that only a write-level scope allows. |

`API_VERSION` (`7.1`) and `ARCHIVE_REVISION` are constants there too.

## 5. Selecting operations

For each operation of each area of a manual, in this order:

1. HEAD and OPTIONS: left out (`HEAD never included`; the GET of the same thing is there).
2. A GET in `skip_gets`: left out (`skipped: <reason>`).
3. A GET or a `post_reads` POST: included as a read.
4. With `read_only`: every other operation is left out (`write (--read-only)`).
5. A DELETE not in `deletes`: left out (`DELETE not selected`).
6. Everything else (POST, PUT, PATCH, the chosen DELETEs): included as a write.
7. With `read_only`, a read whose scopes are all write-level scopes (`_write`, `_manage`, `_full`, `_execute`) is
   left out too (`read needs a write scope: ...`).

Every left-out operation goes into `report.json` `left_out` with its manual, area, method, operation id and reason.

## 6. Building a tool

### Name

- From the operation's `x-ms-vss-method` (Microsoft's client method, `GetTeamDaysOff` -> `get_team_days_off`), else
  from the operation id: the part after `_` in snake case; a bare verb gets the resource from the part before `_`
  (`Teams_List` -> `list_teams`, `Teams_Get` -> `get_team`, singular for everything but `list`).
- `names` overrides both.
- Clashes within a manual: the operation of the manual's first area keeps the name when that area has only one of
  them. The others get a prefix: the area in snake case (`tfvc_get_branches`), or, when one area has several with
  that name, the operation id's group (`nuget_...`, `npm_...`). A remaining clash fails `generate` with a message
  asking for a `names` entry.
- Tools are sorted by name in the manual.

### URL

`https://<host><basePath><path>?api-version=<version>`:

- `host` is the spec's (`dev.azure.com`, `vssps.dev.azure.com`, `vsrm.dev.azure.com`, ...). In a parameterized host
  (`x-ms-parameterized-host`) the optional parameters are removed (`{service}dev.azure.com` -> `dev.azure.com`); a
  required host parameter fails `generate`. An area in `host_overrides` uses that host instead; a path without
  `{organization}` then gets it as its first segment, and the tool is sent the PAT.
- `{organization}` is replaced with `--org`. The name must match `^[A-Za-z0-9][A-Za-z0-9-]{0,48}[A-Za-z0-9]?$`, so it
  needs no encoding. No tool takes the organization as a parameter.
- Segments of `drop_optional_path_params` are removed. A literal `$` before a path parameter becomes `%24`, because
  utcp reads `${NAME}` as a variable.
- `version` is `7.1` plus the operation's preview suffix from its `api-Version-preview.N` parameter reference
  (`7.1-preview.1`). An inline `api-version` parameter, an unknown suffix or no version fails `generate`.

### Inputs

- Path parameters are always required. Query and header parameters are required when the spec says so or
  `required_params` lists them.
- `organization` and `api-version` are not inputs.
- Spec formats `array (...)` become a comma-separated string; other known formats (`int32`, `date-time`, `uuid`, ...)
  are kept; `enum` and `default` are kept.
- Header parameters whose spec name is not the real header are renamed: `Version` -> `If-Match`,
  `contentRangeHeader` -> `Content-Range`, `continuationToken` -> `x-ms-continuationtoken`. They are listed in the
  template's `header_fields`. `If-Match` is never required (a create has no version to match).
- A parameter named `body` in a path, query or header fails `generate` (it would clash with the body field).

### Body

The body is the input `body` (`"body_field": "body"`), with the first `consumes` type as `Content-Type` (default
`application/json`):

1. A `body_overrides` schema when there is one.
2. Else, for a file upload (every `consumes` type is `application/zip`, `application/octet-stream` or `image/*`):
   `bodies::file_upload()`, the utcp form `{"file": "<path>", "offset", "length"}`, and `"body_type": "file"`.
3. Else, for `application/json-patch+json`: `bodies::json_patch_document()`, an array of operations (the spec
   describes an object).
4. Else the spec's schema, with `$ref` and `allOf` inlined to three levels (deeper or cyclic references become
   `{"type": "object", "description": ...}`), without `readOnly` properties, `_links` and `url`.

### Template and answer

- `Accept` is `application/json` when the operation produces it, else its first produced type.
- An operation whose produced types are all binary (as for uploads) gets `"response_type": "file"`; its output
  description says utcp saves the file and prints `{"type": "file", "path", "mime_type", "bytes", "filename"}`.
  `text/*` answers have a string output; the rest an object.
- **Auth:** only operations whose path contains `{organization}` get
  `{"auth_type": "basic", "username": "pat", "password": "${AZURE_DEVOPS_EXT_PAT}"}`. The one operation outside any
  organization (`status.dev.azure.com`, the public service health) gets none: it would answer 401 to an
  organization-scoped PAT, and must not be sent one anyway.
- **Description:** the spec's, on one line (else the operation id). Writes not in `tested_writes` start with
  `(untested)`.
- **Tags:** `azure-devops`, the area, the operation's first spec tag in snake case, and `untested` for untested
  writes.

### Manual

```json
{"manual_version": "1.0.0", "utcp_version": "1.0.0",
 "info": {"title": "...", "version": "7.1", "description": "... for the '<org>' organization (REST API 7.1). ..."},
 "tools": [...]}
```

Written pretty-printed. `report.json` sizes and token estimates use the compact ASCII form (`bytes / 4`).

### Hints

`manual_hints` appends `, org <org>` to each manual's hint. utcp refuses hints over 60 characters
(`MAX_MANUAL_HINT_CHARS` in utcp-core); when any hint would be longer, every manual keeps its plain hint, so all eight
look alike. The longest plain hint is 44 characters, so organization names of up to 10 characters fit.

## 7. PAT scopes

- An operation's scopes come from its `security` (else the spec's). Several scopes on one operation mean any one of
  them is enough.
- `covering_scopes` picks a small set so every tool has one of its scopes: sets are handled smallest first, and an
  uncovered set adds its best member (a read scope before a write scope, then by name).
- `minimal_scopes` keeps only the broadest level of each family, by suffix rank: none, `_read`, `_use`, `_query`,
  `_write`, `_execute`, `_manage`, `_full` (`vso.work_write` covers `vso.work`).
- `scopes` covers every tool; `read_scopes` covers only the reads that a read scope allows. Both are also computed per
  manual.
- `src/pat.rs` maps scope ids to the New Token page's permission and level (`vso.code_write` -> `Code: Read & write`).
  Entries marked "not checked" follow Microsoft's documentation but were not compared with the page. An unknown id is
  shown as itself.
- Operations without any scope are listed per manual in `tools_without_scope`. Some of them need a full-access PAT;
  the live read test with a PAT limited to the listed scopes finds those (they go to `skip_gets`).

## 8. Output files

### `report.json`

```json
{
  "api_version": "7.1",
  "source": {"revision": "...", "revision_date": "...", "revision_message": "..."},
  "manuals": {"azdo_git": {...}, "azdo_work": {...}},
  "excluded_areas": [{"area": "tokens", "reason": "creates and revokes PATs"}],
  "left_out": [{"manual": "...", "area": "...", "method": "DELETE", "operation_id": "...", "reason": "..."}],
  "scopes": [{"scope": "vso.code_manage", "description": "..."}],
  "read_scopes": [{"scope": "vso.code", "description": "..."}],
  "scopes_all": ["every scope any tool lists"]
}
```

Each manual's entry:

| Field | Meaning |
| :--- | :--- |
| `hint` | The hint `generate --stage` passes to `utcp stage` (section 6, Hints). |
| `areas`, `areas_without_spec` | The manual's areas, and those without a 7.1 description. |
| `tools`, `reads`, `writes`, `untested_writes` | Tool counts. |
| `bytes`, `approx_tokens` | Size of the compact manual, and that divided by 4. |
| `scopes` | The scopes that cover the manual's tools (section 7). |
| `tools_without_scope` | Tools whose operation lists no scope. |

`generate --stage` stages each manual with its `hint` from here.

### `scopes.md`

A heading naming the organization, the API version and archive revision, the token page URL, a table "Permission on
the token page | Scope | What it allows" for `scopes`, a sentence with the `read_scopes` permissions when they differ,
and a table "Manual | Tools | Approx. tokens if fully loaded | Permissions".

### What `generate` prints

The manuals with tool counts and hints; "Left out" (areas without a 7.1 folder, excluded areas, counts per reason,
and each `skipped` operation); with `--stage`, `Staged the 8 manuals in utcp.`; the permissions as a table with
"All tools" and "Reads only" columns (one column with `--read-only`); and numbered next steps (stage if not staged,
make the PAT, give it to utcp, `utcp doctor`, `utcp diff`, `read-test`, `utcp promote`).

### `readtest.json`

An object keyed `<manual>.<tool>`. Each value has `status` (`ok`, `http NNN`, `error`, `not exercised`) and, for calls,
`seconds`, `args`, and `bytes` (`ok`), `file: true` (file answers) or `error` (the first 500 characters of utcp's
error output). Not-exercised tools have a `reason`.

### `writetest.json`

An object keyed by a label (`<manual>.<tool>`, with a note in brackets for repeated or follow-up checks). Each value
has `tool`, `status` (`ok`, `mismatch`, `http NNN`, `error`, `not run`), `args`, the answer's checked values, and
`mismatches` (`path: found`) when the answer did not show what was sent.

## 9. Live checks

### `check-routes`

- For each distinct base (`https://<host>/<org>`), `OPTIONS <base>/_apis` with the PAT returns the route list:
  templates such as `{project}/_apis/{area}/{resource}/{id}` with `minVersion`, `maxVersion` and `releasedVersion`.
- A tool's path (after the organization) matches a route when every segment matches: literal segments compare
  case-insensitively, and a route segment with a parameter matches any tool segment that starts with its literal
  prefix. Route parameter segments before `_apis` (such as an optional `{project}`) and at the end may be absent.
- Its `api-version` must not be below the route's `minVersion`. A preview version must not be above `maxVersion`,
  and its `preview.N` not above the route's `resourceVersion`; a released version must not be above
  `releasedVersion` (else it is reported as needing `-preview`).
- Routes carry no HTTP method, so only paths and versions are checked.
- Hosts in `NO_ROUTE_LIST` (`https://status.dev.azure.com`, which belongs to no organization and refuses the
  request) are not checked; `read-test` calls those tools instead.
- Output: the route lists and their sizes, `N of M tools match`, then `no route:`, `version:` and `not checked:` lines.
  Exit 0 only when every tool matched or is on an unchecked host.

### `read-test`

- **Which tools:** every GET, and the read-only POSTs that have a sample body (`POST_BODIES`). Writes are recorded as
  `not exercised` (`not a read`).
- **How:** `utcp call <manual>.<tool> --args-stdin`, with `--staged` for manuals that `utcp list --json --staged`
  shows. Manuals in neither the staged nor the active list stop the test. File answers go to a temporary `--out` file
  that is deleted after counting. A JSON answer too large to print (utcp's reference with a `note`) is read from the
  saved file, which is then deleted.
- **Order:** rounds over the pending tools, sorted by the number of `/` in the URL, then by name, so list tools run
  before the tools that need their ids. A tool whose arguments cannot be filled waits for the next round; the test
  ends when a round calls nothing. 100 ms pause between calls.
- **Filling a URL parameter**, first hit wins:
  1. a seed (`--project`, `--team`, `--seed name=value`) or a `SOURCES` rule for this tool (`tool.param`);
  2. the URL chain: the first item of the answer of the GET whose URL template is the part before this parameter
     (`.../pullrequests` for `.../pullrequests/{pullRequestId}`). `first_item` takes the first element of `value`,
     `items`, `results` or `comments`, else the object itself;
  3. a general `SOURCES` rule (`param`).
- **`value_for`** picks the value from that item: the key with the parameter's own name; then, for a parameter ending
  in `refname`, `referenceName`; for one starting with `process`, `typeId`; then `id`, `name`, `descriptor`, `key`,
  `objectId`, `originId`. Keys compare case-insensitively; only non-empty strings, integers and booleans count.
- **`SOURCES` values:** a literal; `@tool:dotted.path` (from that tool's first successful answer; numeric parts index
  lists; `[key=value]` picks the first list item whose `key` is the string `value`); `@seed:name`; or a string with
  `{@tool:path}` parts. A reference to a tool that has not answered, or a path that is missing or empty, makes the
  tool wait.
- Required query parameters come from seeds and `SOURCES`. Tool-specific `SOURCES` also fill optional parameters.
  `$top`, `top` and `searchCriteria.$top` are set to 5 when the tool has them.
- Printed output has tool names, statuses and sizes only. `readtest.json` has the arguments, which come from the
  organization.

### `write-test` (sandbox organization only)

- Refuses to start without `--create-and-delete-projects`. Uses an isolated utcp (`UTCP_CONFIG_DIR`,
  `UTCP_CACHE_DIR` and `UTCP_STATE_DIR` in a temporary folder, `--direct`), registers the manuals there, and takes
  the PAT from the environment.
- **Fixtures** (direct REST calls, not tools): delete projects named `azdo-manuals-test-*` left by earlier runs, then
  create `azdo-manuals-test-<UTC time>` with a Git repository, `main` and two feature branches, and a YAML pipeline on
  the self-hosted `Default` pool (no agents in a new organization, so builds wait and can be cancelled for free).
- **Steps,** in order: builds (queue on a branch and commit with a runtime parameter, cancel, retry, cancel the
  retry), work items (create; update with `rev`, replace, remove and a link; an update with a stale `rev` must be
  refused), attachments (upload a 300 KB file with `create_attachment`, read it back with `get_attachment`, the bytes
  must be equal), pull requests (create as a draft with a reviewer, a label and a work item; edit and publish;
  threads: a general one and one on lines of a file, a reply, an edit, resolve, reopen and close; abandon and
  reactivate; auto-complete with completion options; a second, minimal pull request completed at once).
- **Checks:** every write's answer must show each field that was sent; effects that happen later (completion,
  cancellation) are polled. Some steps send what the service must refuse and expect a 4xx. A tested write the run did
  not call is recorded as `not run`.
- The project is deleted at the end; a project that could not be deleted is removed by the next run.

## 10. HTTP

- `ureq` 3, blocking, no async runtime. utcp uses `reqwest` on tokio; a short-lived CLI does not need tokio. The two
  must behave the same on the network (`AGENTS.md`, rule 8).
- TLS through rustls with `rustls-platform-verifier`: the operating system's certificate store, so a company CA for
  TLS inspection works. No OpenSSL.
- Proxies from `HTTPS_PROXY` / `ALL_PROXY`. The Windows system proxy is read by neither tool.
- 60 s timeout per request; answers are limited to 256 MB as a guard against a runaway body. Any HTTP status is
  returned to the caller, which decides.
- User agents: `azdo-manuals/<version> (+https://github.com/Faraclas/agent-misc)` (archive),
  `azdo-manuals/<version> route-check`, `azdo-manuals/<version> write-test`.
- `check-routes` and the write-test fixtures send basic auth with user `pat`. The tools themselves are called by utcp.

## 11. Tests

- `cargo test` is offline: hand-made specs in `tests/fixtures/` (`alpha.json`, `beta.json`) go in, the exact
  manual and report in `tests/fixtures/expected/` must come out (byte for byte). `UPDATE_EXPECTED=1` rewrites the
  expected files; run it only for `manuals_match_expected_bytes`, since it races with other tests, and review the diff.
- With `UTCP_BIN` (utcp 0.2.0 or newer), a load test stages the expected manuals in a throwaway utcp, checks
  `schema --staged` and the requests `call --staged --dry-run` builds, and promotes them. Without a binary it is
  skipped, unless `REQUIRE_UTCP=1` (CI on Linux).
- `AZDO_TEST_FIXTURES` points a copied test binary at a copy of `tests/fixtures/` (Windows test machines).
- The live checks (section 9) are run by hand; their output stays private.

## 12. Versions and releases

The version is the REST API version plus a counter: `7.1.0`, `7.1.1`, ... for API 7.1 (a test enforces the prefix).
Changing `pins.json` or the configuration changes the manuals, so it needs a new version. Releases are made by CI
(`gh workflow run azdo-manuals.yml --ref main -f release=true`), which creates the tag `azdo-manuals-vX.Y.Z`. Details
in [`AGENTS.md`](../AGENTS.md).

## 13. Decisions

- **Software Heritage instead of vendoring:** Microsoft deleted the spec repository; vendoring its files would copy
  Microsoft's content into this repository. Hash pins make the archive download exactly as trustworthy as a vendored
  copy.
- **API 7.1, not 7.2:** the service lists 7.1 as released and 7.2 as preview. A 7.2 call must send
  `7.2-preview.N`, which Microsoft can change or retire. Areas with only a 7.2 description (`advancedSecurity`,
  `delegatedAuth`, `environments`, `resourceUsage`) stay in their manuals' area lists and come in with a 7.2 series
  (`azdo-manuals` 7.2.x), which the version scheme allows next to 7.1.x.
- **The PAT decides:** every operation that can work is generated, writes included; leaving tools out only spares
  agents operations that cannot be undone (most DELETEs) or cannot work (`skip_gets`). `--read-only` exists for
  read-only PATs, so agents do not see tools that would fail.
- **Organization fixed at generation time:** URLs carry the organization; no tool takes it as a parameter. Hints name
  it, and one utcp setup holds one organization's manuals (the manual names do not contain it).
- **Staging, not registering:** `generate --stage` lets people review (`utcp diff`) and test (`read-test`) before
  agents see a new version, and `utcp rollback` undoes a bad promote. It replaced `generate --register` in 7.1.0.
- **`ureq` instead of `reqwest`:** no tokio in a small blocking CLI, at the cost of keeping network behavior aligned
  by hand (rule 8).
- **Request-side fixes from Microsoft's `vsts-api-patcher` (MIT):** slim bodies for the tested writes, JSON Patch
  bodies as arrays; credited in [`NOTICE`](../NOTICE).
