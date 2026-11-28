# azdo-manuals specification

- **Version:** describes `azdo-manuals` 7.1.5 (REST API 7.1). Keep it in step with the code (see
  [`AGENTS.md`](../AGENTS.md), "Lockstep docs").
- **Audience:** maintainers and agents changing the generator. Users start with the [README](../README.md) and the
  [operating guide](./operating.md).

## 1. Purpose and boundaries

`azdo-manuals` turns Microsoft's description of the Azure DevOps REST API into utcp HTTP manuals for one
organization, so agents reach Azure DevOps through utcp (`search`, `schema`, `call`) instead of an MCP server. It
reads four inputs:

- **The REST API specs** (Swagger 2.0): the base. They have request body schemas, descriptions and OAuth scopes.
- **Microsoft's client libraries** for Go and Node: generated from the service's own route definitions, so they have
  endpoints the specs leave out. Each method names its endpoint by a location GUID, not a URL.
- **The organization's live route list** (`OPTIONS /_apis` on every host): turns those GUIDs into URL templates,
  hosts and versions. Needs the PAT.
- **The Analytics OData `$metadata`**: the Analytics service is OData, not REST, and describes itself. Needs the PAT.

Without a PAT, `generate` uses the specs only (section 5a and the Analytics manual need the live inputs).

- It only writes files and, with `--stage`, hands them to `utcp stage`. It never registers, promotes or edits utcp's
  configuration itself. utcp owns staging, secrets, the daemon and the agent skills.
- It never stores Microsoft's files in the repository (specs and client libraries are downloaded and verified at
  run time) and never stores anything naming a real organization (the tests use `contoso`).
- It does not limit what agents may do beyond leaving out operations that cannot work or cannot be undone. The PAT's
  permissions decide the rest.

## 2. Pipeline

```text
pins.json (built in) -> archive download (cache, hash check) -> specs per area
                     -> GitHub tarballs (cache, hash check) -> Go and Node methods and types  -.
PAT: resourceAreas + OPTIONS /_apis per host -> live routes -----------------------------------+-> join (sdkops)
                                                                       specs ------------------'        |
PAT: Analytics $metadata -> OData tools ---------------------------------------------.         one-operation specs
config (src/config.rs, src/bodies.rs) ------------------------------------------------+-> generate -> <manual>.utcp.json x 9
                                                                                              -> report.json, scopes.md
                                                                                              -> utcp stage (with --stage)
generated manuals -> check-routes (live route list)   read-test (utcp call)   write-test (sandbox only)
staged or active manuals + tests/search-checks.txt (built in) -> search-test (utcp search)
live routes + join + generate -> route-gaps (endpoints without a tool)
```

| Command | Network | Needs the PAT | Writes |
| :--- | :--- | :--- | :--- |
| `generate --org <org> [--out] [--cache] [--stage] [--utcp] [--read-only] [--spec-only]` | archive and GitHub (uncached files only); with the PAT, Azure DevOps (`GET resourceAreas`, `OPTIONS`, Analytics `$metadata`) | optional, from the environment | manuals folder; utcp staging with `--stage` |
| `check-routes --org <org> [--manuals]` | Azure DevOps, `OPTIONS` only | from the environment | nothing |
| `read-test (--org <org> or --manuals) --project [--team] [--seed]... [--only]... [--utcp]` | through utcp | utcp's | `readtest.json` |
| `search-test [--staged] [--cases <file>] [--top N] [--utcp]` | none (`utcp list` and `utcp search` only) | no | nothing |
| `write-test --org <sandbox> --manuals --create-and-delete-projects [--utcp]` | Azure DevOps, through an isolated utcp | from the environment | a throwaway project; `writetest.json` |
| `route-gaps --org <org> [--out] [--cache]` (maintainers) | as `generate` with the PAT | from the environment | `route-gaps.json` |
| `pins [--revision] [--api-version] [--fresh] [--pins]` (maintainers) | archive | no | the pins file (keeps its `sdk` part) |
| `pins-sdk [--go-commit] [--node-commit] [--pins]` (maintainers) | GitHub | no | the pins file's `sdk` part |
| `help [<command>]` | none | no | nothing |

`azdo-manuals <command> help` is rewritten to `azdo-manuals help <command>` (no command takes a positional argument).
Errors print as `azdo-manuals: <message>`; exit code 2 for usage problems (missing PAT, no manuals, no utcp), 1 for
failures, 0 for success. `check-routes`, `read-test` and `write-test` exit with 1 when any tool failed, `search-test`
when a case that is not a known miss failed.

## 3. Sources and pinning

### REST API specs

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
  every area and continues an existing pins file (unless `--fresh`), so a run cut off by the rate limit resumes. It
  keeps the file's `sdk` part.

### Client libraries

- **Source:** `microsoft/azure-devops-go-api` (MIT, branch `dev`; files `azuredevops/v7/<package>/client.go` and
  `models.go`, API 7.1) and `microsoft/azure-devops-node-api` (MIT, branch `master`; files `api/*Api.ts`,
  `api/*ApiBase.ts`, `api/interfaces/**/*.ts`, API 7.2-preview). `Sdk::wants` is the file rule. Python's `v7_1`
  client has almost the same endpoints as Go (one more GUID, measured 2026-11-23), and the .NET client is on NuGet
  only, so neither is read.
- **`pins.json` `sdk.<go|node>`:** `repo`, `commit`, and per file `file`, `sha1_git` and `length`. `pins-sdk` writes
  it, for the head of each default branch or the commits given.
- **Download:** when any pinned file is missing from the cache, one request for the commit's tarball
  (`https://codeload.github.com/<repo>/tar.gz/<commit>`, about 1.5 MB for Go and 0.6 MB for Node). Every pinned file
  is taken from it, checked against its git blob hash (the tarball's own bytes are not pinned, since GitHub may
  compress them differently over time) and cached as `<cache>/sdk/<hash>`. A missing or different file fails the
  run. `sdk::untar_gz` reads the tar (ustar prefixes, pax `path` records, GNU long names); `flate2` unzips it.
- Only `generate` with the live routes reads the libraries; without a PAT nothing is downloaded from GitHub.

### Live routes

- `GET https://dev.azure.com/<org>/_apis/resourceAreas?api-version=7.1-preview.1` lists every area with the host it
  lives on (the client libraries find hosts the same way). Its hosts, plus `dev.azure.com`, are asked for their
  route list with `OPTIONS https://<host>/<org>/_apis` (`live::fetch`). A host whose list cannot be read is recorded
  in the report and skipped; a failing `resourceAreas` stops `generate` (with a hint to use `--spec-only`).
- Measured on one organization (2026-11-23): 28 hosts, 23 of which list routes; 3,250 routes with 1,791 distinct
  GUIDs.
- The routes are read on every run, not pinned: they are the service's own current answer, and the output then
  follows the service as it changes. Without a PAT the libraries cannot be placed, so `generate` writes the spec's
  tools only and says how to get the rest.

### Analytics metadata

`GET https://analytics.dev.azure.com/<org>/_odata/v4.0-preview/$metadata` (CSDL XML, about 1 MB on one organization).
Read on every run with the PAT; a failure leaves the Analytics manual out and is reported, it does not stop
`generate`.

## 4. Configuration

All decisions live in `src/config.rs` (`Config::real()`) and `src/bodies.rs`. Every operation named in the
configuration must exist in the specs (`generate` refuses a typo), and a test refuses an archive area that is neither
in a manual nor in `excluded_areas`.

| Field | Meaning |
| :--- | :--- |
| `manuals` | Name, spec areas (in order: the first area wins name clashes), client-library areas (`sdk_areas`, areas no spec folder has), title (the manual's `info.title`) and hint. |
| `excluded_areas` | Areas in no manual, each with a reason (`account`, `tokens`, `tokenAdmin`, `tokenAdministration`, `governance`). |
| `route_areas` | Route areas (as the live route list names them, lower case) and the area their library operations go into, for route areas no spec operation maps (`location` -> `location`, `alert` -> `advancedSecurity`). |
| `excluded_route_areas` | Route areas whose library operations go into no manual, with the reason (web UI extension points, the service's feature flags, telemetry). |
| `post_reads` | POSTs whose only effect is to return data (queries, batch reads, searches, lookups). They count as reads. |
| `tested_writes` | Writes covered by `write-test`; each needs a body in `bodies.rs` (a test enforces this). |
| `deletes` | The DELETEs that are generated (19 small ones an agent may need to take back). |
| `skip_gets` | GETs left out, with the service's answer as the reason (deprecated, broken, or no scope a scoped PAT can have). |
| `required_params` | Parameters the spec calls optional but the service requires. |
| `names` | Tool names set by hand, for clashes or names that say too little. |
| `body_overrides` | Request body schemas replacing the spec's (`bodies::real()`). |
| `host_overrides` | Hosts replacing the spec's, per area (`profile` -> `vssps.dev.azure.com`; the spec's host refuses an organization's PAT). A test refuses an area no manual includes. |
| `summaries` | Hand-written first sentences of tool descriptions, per area and operation id (`SUMMARIES`), each with a comment saying why: only where the operation's own first sentence tells an agent nothing (a developer comment, or a name-made sentence that leaves out what it acts on, such as "Get columns" for a team's taskboard). Section 6, Description. |
| `notes` | Sentences added to the end of a tool description, per area and operation id (`NOTES`), kept when it is shortened: the words agents search for that Microsoft's text lacks (`get_connection_data`: "who am I, the current user, me"). |
| `read_only` | Set by `--read-only`: no writes, and no reads that only a write-level scope allows. |

`API_VERSION` (`7.1`) and `ARCHIVE_REVISION` are constants there too. Operations from the client libraries are named
in the configuration by `<Resource>.<Method>` (`securefiles.GetSecureFiles`); no spec operation id contains a dot. Such
names are checked only when the live routes were read, since only then do those operations exist.

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

## 5a. Operations from the client libraries

`sdkops::join` adds the endpoints the specs lack, as one-operation Swagger specs that then go through sections 5 to 7
like any spec operation (selection, names, URL, inputs, bodies, report, live checks).

1. **Parse** (`sdk_go`, `sdk_node`). Go: each `func (client *ClientImpl) <Method>(ctx, args <Method>Args)` with its
   `routeValues["name"]`, `queryParams.Add("name", ...)`, `json.Marshal(args.X)` body, `additionalHeaders`, the GUID
   from `uuid.Parse` and `client.Client.Send(ctx, http.Method<X>, locationId, "<version>", ..., "<content type>",
   "<accept>", ...)`; argument types and `(required)`/`(optional)` from the `Args` struct; a list sent as one
   comma-separated value (`listAsString`); a stream answer from `return resp.Body`. Models: structs with `json` tags
   and string enums with their `<Name>Values`. Node: each `public async <method>(...)` with its JSDoc `@param` texts,
   `routeValues` and `queryValues` objects (keys are the names on the wire, `queryValues.x = ...` adds more, `x &&
   x.join(",")` is a list), `getVersioningData("<version>", "<area>", "<guid>")` and the call (`rest.get`, `create` =
   POST, `update` = PATCH, `replace` = PUT, `del`, `uploadStream("<VERB>")`, `http.get` for a stream answer). A
   nested object in `queryValues` is sent flattened (`criteria.kind`), one level deep. Interfaces with their
   `extends` lists, and enums (names in camelCase, as the service takes them). Route values that a method fills with
   a fixed string (`action: "Branches"`) are written into the path.
2. **Map the spec to GUIDs** (`coverage`). A spec operation calls the route whose area and resource are its
   `x-ms-vss-area` and `x-ms-vss-resource`; when that gives several, or none, its path is matched to the route
   templates (the `check-routes` rule), and of several matches the one with the most literal segments wins. The spec
   then covers (GUID, HTTP method, number of path parameters): the number tells the forms of one endpoint apart
   (`.../alerts` and `.../alerts/{alertId}` share a GUID).
3. **Pick the missing forms.** A library method whose GUID the organization does not serve is listed in the report
   (`library_methods_not_on_organization`). One that has the same (GUID, method, parameter count) as a spec
   operation, or the same client method name on that GUID and HTTP method, is the spec's. Of the rest, one method per
   form: Go before Node (Go targets 7.1), a JSON answer before a file variant of the same endpoint (`...Zip`,
   `...Text`, `...Content`), then by name.
4. **Area and manual.** `route_areas` first, then `excluded_route_areas` (left out with its reason), then the spec
   area most spec operations of that route area belong to. An area in `excluded_areas`, or in no manual, is left out
   with the reason; so is a route area nothing maps (`route area X is in no manual yet`). These go into `left_out`
   with `"manual": null` and the GUID.
5. **Too new.** A route whose `minVersion` is above the manuals' API version (7.2-only endpoints) is left out:
   `starts at API 7.2; these manuals are for 7.1`.
6. **Host.** The one `resourceAreas` gives the route's area, if it serves the GUID; else `dev.azure.com`; else the
   first host that serves it.
7. **api-version** (`api_version_for`). The target is 7.1, or the route's `maxVersion` when that is lower. A route
   released at or after the target is called at the target (`7.1`); any other as `<target>-preview.<n>`, where n is
   the library's preview number capped at the route's `resourceVersion` (the parameters match that resource
   version). The version goes into the operation as `x-azdo-manuals.api-version`, which `api_version_of` reads first.
8. **Path.** `/{organization}/` plus the route template with `{area}` and `{resource}` filled in and each route
   parameter the method supplies named as the method names it (also inside a segment, `@{packageScope}`). A segment
   whose parameter the method does not supply is left out, as the libraries do. An optional route value (Go
   `(optional)`, Node `?`) is marked `x-ms-required: false`, so it is an optional input (see Inputs).
9. **Inputs and body.** Query and header parameters with the library's types: integers with their format, uuid and
   date-time formats, enums with their values, lists as comma-separated strings. The body is a `$ref` into the
   spec's `definitions`, built from the library's types down to four levels (section 6, Body, then inlines three).
   `Content-Type` comes from the library (`application/json-patch+json` and `application/octet-stream` get the same
   treatment as in the spec). `operationId` is `<Resource>.<Method>`, `x-ms-vss-method` the method name, the tag the
   resource name, and `x-azdo-manuals` records `source` (`go` or `node`), `client`, `guid`, `api-version` and how the
   scopes were found.
10. **Scopes.** The libraries name none. The operation takes the most common scope list of the spec's operations on
    the same GUID that read or write as it does (a GET reads); else that of the spec's operations in its area that
    read or write as it does; else none. The report lists the tools whose scopes were inferred
    (`tools_with_inferred_scopes`) and those without any (`tools_without_scope`). `read-test` with a PAT that has
    exactly the report's scopes finds the wrong guesses: a read that answers 401 goes to `skip_gets` with "401 with
    every scope the report lists; the scope it needs is not known".

## 5b. The Analytics manual

`analytics::entries` reads the `$metadata` with a small XML reader (elements and attributes only) and makes one read
tool per entity set, in the manual `azdo_analytics`:

- URL `https://analytics.dev.azure.com/<org>/{project}/_odata/v4.0-preview/<EntitySet>`, `project` required. The
  `$metadata` is read from the organization's root (the union of every project's fields); the tools query a project's
  root, which needs only project-level Analytics permission.
- Inputs: the OData query options `$select`, `$filter`, `$apply`, `$orderby`, `$expand` (when the entity has
  navigation properties), `$top`, `$skip`, `$skiptoken`, `$count`. The `$select` description lists the entity's
  properties with their types; custom work item fields (`Custom_...`, hundreds in a large organization) are counted,
  not listed. `$expand` lists the navigation properties.
- Description: the entity's own description, the display name, and how paging works (`@odata.nextLink` holds the
  next `$skiptoken`). Large sets (names with `Snapshot`, `Daily`, `ActivityResults`) say which date key to filter on;
  snapshot sets also say they are meant for aggregation (`$apply`), as the service warns (`VS403510`).
- Scope `vso.analytics` ("Analytics: Read"), source `odata`, name `query_<entity_set>`.
- The Analytics REST routes the libraries describe (`analytics` route area) go into the same manual.

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
- Tools that do not come from the spec (client libraries, OData) are named after the spec's tools, so adding them
  never renames a spec tool: after their method (`GetConnectionData` -> `get_connection_data`); when that name is a
  spec tool's, or several of them share it, with their area as prefix (or their resource, when one area has several).
- Tools are sorted by name in the manual.

### URL

`https://<host><basePath><path>?api-version=<version>`:

- `host` is the spec's (`dev.azure.com`, `vssps.dev.azure.com`, `vsrm.dev.azure.com`, ...). In a parameterized host
  (`x-ms-parameterized-host`) the optional parameters are removed (`{service}dev.azure.com` -> `dev.azure.com`); a
  required host parameter fails `generate`. An area in `host_overrides` uses that host instead; a path without
  `{organization}` then gets it as its first segment, and the tool is sent the PAT.
- `{organization}` is replaced with `--org`. The name must match `^[A-Za-z0-9][A-Za-z0-9-]{0,48}[A-Za-z0-9]?$`, so it
  needs no encoding. No tool takes the organization as a parameter.
- Every path parameter stays in the URL, optional ones too. A literal `$` before a path parameter becomes `%24`,
  because utcp reads `${NAME}` as a variable.
- utcp fills `{name}` from the argument of exactly that name, so each placeholder takes the spelling of the input it
  matches case-insensitively: the spec's `{DefinitionId}` with the parameter `definitionId` becomes `{definitionId}`
  (since 7.1.3; before, those calls sent the placeholder literally). A placeholder with no such input is kept.
- `version` is `7.1` plus the operation's preview suffix from its `api-Version-preview.N` parameter reference
  (`7.1-preview.1`). An inline `api-version` parameter, an unknown suffix or no version fails `generate`.

### Inputs

- Path parameters are required, except those marked `x-ms-required: false` (since 7.1.3): utcp 0.2.4 leaves the
  segment of one that is not passed out of the URL (`…/timeline/{timelineId}` -> `…/timeline`, the build's own
  timeline). Before 7.1.3 they were required, and `team` in `wit` was dropped from the URL. Since 7.1.4 one stays
  required when a fixed segment follows it before the API root (`changesets/{id}/changes`: without `id` the server
  reads `changes` as the id); one at the end, or before `_apis` or `_packaging`, stays optional. An optional `project`
  or `team` gets what leaving it out means added to its description: the whole organization, or the project's
  default team (`scope_note`). Query and header parameters are required when the spec says so or
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
- **Description:** what agents find a tool by, with its name: `utcp list` and `utcp search` print only its first
  sentence. `generate::tool_description` builds it (since 7.1.5; before, the spec's text on one line, else the
  operation id as "Roledefinitions: List"):
  1. The operation's description on one line (a client-library method's doc comment, section 5a).
  2. A `summaries` entry replaces its first sentence (up to the first `.`, `!` or `?` that a space follows).
  3. Else a first sentence that is a developer comment is dropped: one saying where the code was copied, adapted or
     ported from ("This was copied and adapted from TeamFoundationConnectionService.Connect()"), or one starting with
     a URL (an example request to "http://(tfsserver):8080/tfs/...").
  4. When nothing is left, a sentence made from the tool's name before clashes are settled (`plain_name`): a `names`
     entry, else the client method (`x-ms-vss-method`), else the operation id (`base_name`), with `_` as spaces, the
     first letter capitalized and a last word `async` (.NET's suffix) left out: "Roledefinitions_List" with the
     method GetRoleDefinitions -> "Get role definitions". The method is the source because it has its words apart
     where the operation id glues them ("Roledefinitions", "Agentclouds", "Oidctoken"); every spec operation without
     a description has one. There is no full stop, as client-library tools without a doc comment have always been
     described ("Get policies"); the same rule now makes both.
  5. Cut to 400 characters at a word (`...`). A `notes` entry is added after it, with a full stop before it when the
     text has none, and the text is cut to leave room for it, so the note is never lost.
  6. Writes not in `tested_writes` start with `(untested)`, before everything else.
- **Tags:** `azure-devops`, the area, the operation's first spec tag in snake case, and `untested` for untested
  writes.

### Manual

```json
{"manual_version": "1.0.0", "utcp_version": "1.0.0",
 "info": {"title": "...", "version": "7.1", "description": "... for the '<org>' organization (REST API 7.1). ..."},
 "tools": [...]}
```

The description says what the manual was generated from: "Microsoft's REST API specs", "Microsoft's client
libraries" and "the Analytics service's OData metadata", as far as its tools come from them.

Written pretty-printed. `report.json` sizes and token estimates use the compact ASCII form (`bytes / 4`).

### Hints

`manual_hints` appends `, org <org>` to each manual's hint. utcp refuses hints over 60 characters
(`MAX_MANUAL_HINT_CHARS` in utcp-core); when any hint would be longer, every manual keeps its plain hint, so all nine
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
  "extra": null,
  "manuals": {"azdo_git": {...}, "azdo_work": {...}},
  "excluded_areas": [{"area": "tokens", "reason": "creates and revokes PATs"}],
  "left_out": [{"manual": "...", "area": "...", "method": "DELETE", "operation_id": "...", "reason": "..."}],
  "scopes": [{"scope": "vso.code_manage", "description": "..."}],
  "read_scopes": [{"scope": "vso.code", "description": "..."}],
  "scopes_all": ["every scope any tool lists"]
}
```

`extra` is null without the live inputs. With them it has `client_libraries` (`go`/`node`: `repo`, `commit`),
`route_hosts` (host -> number of routes, or why its list could not be read), `routes`, `library_operations` (the
one-operation specs built, before selection), `library_methods_not_on_organization` (`<sdk> <client>.<Method>`)
and `analytics` (`odata_version`, and `entity_sets` or `error`). Library operations left out before generation are
in `left_out` with `"manual": null` and a `guid`.

Each manual's entry:

| Field | Meaning |
| :--- | :--- |
| `hint` | The hint `generate --stage` passes to `utcp stage` (section 6, Hints). |
| `areas`, `areas_without_spec` | The manual's areas (spec, then client-library), and the spec areas without a 7.1 description. |
| `tools`, `reads`, `writes`, `untested_writes` | Tool counts. |
| `sources` | Tools per source: `spec`, `go`, `node`, `odata`. |
| `bytes`, `approx_tokens` | Size of the compact manual, and that divided by 4. |
| `scopes` | The scopes that cover the manual's tools (section 7). |
| `tools_without_scope` | Tools whose operation lists no scope. |
| `tools_with_inferred_scopes` | Client-library tools whose scopes were inferred (section 5a, step 10). |

`generate --stage` stages each manual with its `hint` from here.

### `scopes.md`

A heading naming the organization, the API version and archive revision, the token page URL, a table "Permission on
the token page | Scope | What it allows" for `scopes`, a sentence with the `read_scopes` permissions when they differ,
and a table "Manual | Tools | Approx. tokens if fully loaded | Permissions".

### What `generate` prints

The manuals with tool counts and hints; "Left out" (areas without a 7.1 folder, excluded areas, counts per reason,
library operations left out before generation by reason, and the `skipped` operations: each one, or a count when a
reason has more than three); how many tools came from the client libraries and from OData, or, without the live
inputs, how to get them; with `--stage`, `Staged the 9 manuals in utcp.`; the permissions as a table with
"All tools" and "Reads only" columns (one column with `--read-only`); and numbered next steps (stage if not staged,
make the PAT, give it to utcp, `utcp doctor`, `utcp diff 'azdo_*'`, `read-test`, `utcp promote 'azdo_*'`). The
`'azdo_*'` keeps other staged manuals out; it needs utcp 0.2.4 or newer, which also refuses a bare `promote`.

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
  request) are not checked; `read-test` calls those tools instead. Neither are OData tools (a URL with `/_odata/`):
  OData has no `_apis` routes.
- Output: the route lists and their sizes, `N of M tools match`, then `no route:`, `version:` and `not checked:` lines,
  and the counts of tools on unchecked hosts and of OData tools. Exit 0 only when every tool matched or was not
  checkable.

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
  3. a general `SOURCES` rule (`param`);
  4. for a parameter with an `enum`, its first value. A URL-chain value counts only if it is one of the listed values
     (case-insensitive), so `structureGroup` gets `areas`, not an id (since 7.1.4).
- **`value_for`** picks the value from that item: the key with the parameter's own name; then, for a parameter ending
  in `refname`, `referenceName`; for one starting with `process`, `typeId`; then `id`, `name`, `descriptor`, `key`,
  `objectId`, `originId`. Keys compare case-insensitively; only non-empty strings, integers and booleans count.
- **`SOURCES` values:** a literal; `@tool:dotted.path` (from that tool's first successful answer; numeric parts index
  lists; `[key=value]` picks the first list item whose `key` is the string `value`); `@seed:name`; or a string with
  `{@tool:path}` parts. `{@date:-N}` is the local date N days ago as `yyyyMMdd` (Analytics date keys), so the
  snapshot queries stay small. A reference to a tool that has not answered, or a path that is missing or empty, makes
  the tool wait.
- Required query parameters come from seeds and `SOURCES`. Tool-specific `SOURCES` also fill optional parameters.
  An optional path parameter with no value is left out, so the default (the build's timeline, the default team)
  is tested.
  `$top`, `top` and `searchCriteria.$top` are set to 5 when the tool has them.
- Printed output has tool names, statuses and sizes only. `readtest.json` has the arguments, which come from the
  organization.

### `search-test`

Checks that agents find the right tool with the words they would type, against the user's real manuals. It cannot
run in CI (the real manuals name the organization and are never committed, rule 1 of `AGENTS.md`); the cases can,
since they hold only queries and generic tool names.

- **Cases:** `tests/search-checks.txt`, embedded with `include_str!` (so the installed binary carries it), or
  `--cases <file>`. One case per line: `query -> manual.tool | manual.tool`, optionally ending with
  `[known miss: reason]`. `#` at the start of a line, or after whitespace outside the brackets, starts a comment, so
  `c# sdk` stays a query. A tool name is `manual.tool` (letters, digits, `_`, `-`). A query may not start with `-`
  (utcp would read it as an option). A malformed line stops the run with `line N: <what>: <the line>` (exit 2), as
  does a file without cases.
- **Registered manuals:** the names in `utcp list --json` whose row is not marked `staged`, plus, with `--staged`,
  the names in `utcp list --staged --json`. A case's tools of other manuals are ignored; a case with none left is
  `SKIP` and not searched. One file thereby serves several manual families (azdo and github cases side by side).
- **Search:** `utcp search --json --limit 10 [--staged] <query>` (`SEARCH_LIMIT`), through the user's utcp and daemon
  like `read-test`, with the same `MIN_UTCP` check. The answer is a JSON array; its `name`s (`manual.tool`), in order,
  are the ranks. A failed search stops the run (exit 1).
- **Judging:** the rank is the best 1-based place of any listed, registered tool. `PASS` when it is at most `--top`
  (default 3, 1 to 10); otherwise `KNOWN` for a known miss and `FAIL` for the rest, with rank `-` when no listed tool
  is in the 10 results.
- **Output:** a header naming what was searched, a table with one line per case (query, rank or `-`, the found tool
  or else the tools that could have been found, result; `KNOWN` adds its reason), then the totals: cases searched
  and skipped, how many found at rank 1, in the top 3 and in the top 5 (of those searched), known misses, failures,
  and the known misses that pass now (their marker should go).
- **Code:** `src/searchtest.rs`. Parsing (`parse_cases`), judging (`check`, `check_all` with the search passed in as a
  function) and the report are pure, so `src/tests_search.rs` tests them on canned results; one test searches the
  fixture manual in an isolated utcp (skipped without utcp, like the load test).
- **A failure** means either the manuals lack the words people use (fix in `summaries`/`notes`, section 4, never
  with odd wording just to pass) or utcp ranks badly (a utcp-rs bug).

### `route-gaps` (maintainers)

The reverse of `check-routes`: which live endpoints have no tool, and why. It reads the inputs as `generate` does with
the PAT, generates in memory, and sorts every live GUID into:

- `tool`: at least one tool calls it;
- `left out`: spec or library operations exist, but every one was left out (the reasons are listed);
- `not described`: neither the specs nor a client library describe it. Grouped by route area (areas in
  `excluded_route_areas` are marked), this is the list to look through for endpoints worth describing by other means.

It prints the counts and the not-described route areas, and writes `route-gaps.json` (`summary`,
`not_described_by_area`, and per GUID: area, resource, template, hosts, versions, status, the libraries that have it
and the reasons) next to the manuals. Measured on one organization (2026-11-23): 1,791 GUIDs; 714 with a tool, 26
left out by a rule, 1,051 not described (most of them internal or plumbing areas, legacy 1.0 routes, and 7.2-only
endpoints).

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
- User agents: `azdo-manuals/<version> (+https://github.com/Faraclas/agent-misc)` (archive and GitHub),
  `azdo-manuals/<version> route-check`, `azdo-manuals/<version> routes` (`resourceAreas`),
  `azdo-manuals/<version> analytics`, `azdo-manuals/<version> write-test`.
- `check-routes`, the live inputs of `generate` and the write-test fixtures send basic auth with user `pat`. The
  tools themselves are called by utcp. GitHub is called without a token (`pins-sdk` asks `api.github.com` for a
  branch's head once per library).

## 11. Tests

- `cargo test` is offline: hand-made specs in `tests/fixtures/` (`alpha.json`, `beta.json`) go in, the exact
  manual and report in `tests/fixtures/expected/` must come out (byte for byte). `UPDATE_EXPECTED=1` rewrites the
  expected files; run it only for `manuals_match_expected_bytes`, since it races with other tests, and review the diff.
- `src/tests_sdk.rs` tests the other inputs with hand-made files in the shape of the real ones, not copies of them:
  a small Go and Node library (`tests/fixtures/sdk/`), a route list and `resourceAreas` answer (`live.json`), and a
  `$metadata` document (`analytics-metadata.xml`). The full output for them is in `tests/fixtures/expected-extra/`
  (`extra_manuals_match_expected_bytes`, rewritten the same way). Tarballs are built in the test.
- With `UTCP_BIN` (utcp 0.2.4 or newer; an older one is skipped), a load test stages the expected manuals in a
  throwaway utcp, checks `schema --staged` and the requests `call --staged --dry-run` builds, and promotes them.
  `src/tests_search.rs` tests the `search-test` case format and judging offline, and with `UTCP_BIN` searches the
  fixture manual (registered, and staged under a second name) in a throwaway utcp.
  Without a binary they are skipped, unless `REQUIRE_UTCP=1` (CI on Linux).
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
- **Hand-written description text, not hand-written operations (7.1.5):** rule 2 forbids hand-written operation files,
  since what an operation is must come from Microsoft's files. A few sentences of description in `summaries` and
  `notes` are a different thing, like the bodies in `bodies.rs`: they change no URL, input or scope, each has a
  comment with its reason, and `generate` refuses one that names an operation the specs lack. They are kept few: a
  rule (the name sentence, the developer-comment check) is preferred wherever it gets the text right, and a short but
  true summary ("Get policies") stays.
- **Client libraries plus live routes for what the specs lack (7.1.2):** the published specs are a subset of the
  service (1,791 live GUIDs on one organization; the specs cover about a quarter). The client libraries are generated
  from the service's own route definitions but hold GUIDs, not URLs; the live route list turns GUIDs into URLs. Both
  are read, not hand-written: no operation file is maintained by hand. The routes are read live on every run instead
  of pinned, so the output follows the service and nothing service-specific is copied into the repository; the cost
  is that the extra tools need the PAT at generate time.
- **Spec first, libraries only fill gaps:** the specs have bodies, scopes and descriptions the libraries lack, so an
  endpoint form the spec has is never generated again, and adding library tools never renames or changes a spec tool.
- **Go before Node:** Go's client targets API 7.1, Node's 7.2-preview. Node is still needed: it has most of the
  endpoints Go lacks (on one organization 302 of the 379 library tools came from Node).
- **Forms, not just GUIDs:** one GUID can serve a list and a single item (`.../alerts`, `.../alerts/{alertId}`), so
  coverage counts path parameters. The same client method name as the spec's on that endpoint counts as covered.
- **Endpoints that start at 7.2 are left out**, not called as 7.2-preview: these manuals are for 7.1, and a 7.2.x
  series would take them.
- **Inferred scopes, checked live:** the libraries carry no scopes. The spec's scopes for the same endpoint or area are
  usually right; where they are not, `read-test` with a PAT scoped to the report shows a 401, and the GET goes to
  `skip_gets`. A tool the PAT cannot call is worse than no tool.
- **Analytics from `$metadata`, v4.0-preview, project root:** Analytics is OData, so it has no Swagger; its
  `$metadata` is the standard self-description. v4.0-preview has the test and pipeline entity sets the skills need;
  v2.0 has only work tracking. Querying a project's root needs less permission than the organization's.
- **No XML or tar crate:** CSDL and GitHub's tarballs are simple and regular, and a new crate changes the Gentoo
  ebuild's crate list. `flate2` was already in the dependency tree (through `ureq`), so using it directly adds no
  crate.
