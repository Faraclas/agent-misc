# azdo-manuals

Generates [utcp](../utcp-rs/) HTTP manuals for the Azure DevOps REST API, so agents call Azure DevOps through utcp
(`search` -> `schema` -> `call`) instead of loading an MCP server's tool list into every conversation.

- **Sources:** Microsoft's own descriptions of the API, REST API version 7.1:
  - The API specs (Swagger 2.0). Their GitHub repository (`MicrosoftDocs/vsts-rest-api-specs`) no longer exists, so
    the generator downloads the files from the [Software Heritage](https://www.softwareheritage.org/) archive of it
    (revision of 2026-09-02, "Update REST API specs from releases/M279").
  - The specs describe only part of the service. With your PAT, `generate` also reads your organization's live list
    of endpoints and Microsoft's Go and Node client libraries (from GitHub), which are generated from the service's
    own route definitions, and adds the endpoints the specs leave out.
  - With your PAT it also reads the Analytics service's OData description (`$metadata`) for an Analytics manual.

  Every downloaded file is checked against the hash recorded in [`pins.json`](./pins.json), which is built into the
  binary. Nothing from Microsoft is stored in this repository.
- **Output:** for one organization, nine manuals: every area of the API that has a 7.1 description, the endpoints
  only the client libraries describe, and Analytics (on one organization, 2026-11-23: 1,356 tools, 795 reads and 561
  writes; 945 from the specs, 379 from the client libraries, 32 Analytics queries). Without the PAT, eight manuals
  with the 945 spec tools. Also a `scopes.md` listing the personal access token (PAT) permissions the manuals need,
  and a `report.json` (tools, sizes, and every operation left out with the reason).
- **Requirements:** one static binary, no runtime. Network access to `archive.softwareheritage.org` and
  `codeload.github.com` for the first run (about a minute, 48 downloads); later runs use the cache, and with the PAT
  read your organization's route lists (about 30 requests, a few seconds).
- **Docs:** this README (installation, first setup), the [operating guide](./docs/operating.md) (the PAT, upgrades,
  test results, troubleshooting, Windows, proxies) and the [specification](./docs/azdo-manuals-spec.md) (how the
  manuals are built and checked).

## Installation

- **Linux (x86_64):** the project's GitHub Releases (tags `azdo-manuals-v*`) have
  `azdo-manuals-x86_64-unknown-linux-musl.tar.gz`, a static binary that runs on any distribution, provided as-is.
  Copy `azdo-manuals` to `~/.local/bin/`.
- **Windows:** the release zip `azdo-manuals-x86_64-pc-windows-msvc.zip` holds `azdo-manuals.exe`. Put it in a folder
  on your `PATH`, for example next to `utcp.exe` in `%LOCALAPPDATA%\Programs\utcp`.
- **From source:**

  ```sh
  cd azdo-manuals
  cargo build --release
  cargo test
  install -Dm755 target/release/azdo-manuals ~/.local/bin/azdo-manuals
  ```

## The manuals

| Manual | Spec areas (and client-library areas) | Hint for `--hint` |
| --- | --- | --- |
| `azdo_git` | git, policy, tfvc, advancedSecurity | Azure DevOps repos and PRs |
| `azdo_work` | wit, work, dashboard, processes, processadmin, processDefinitions | Azure DevOps work items, boards, dashboards |
| `azdo_pipelines` | build, pipelines, release, approvalsAndChecks, distributedTask, environments (fileContainer) | Azure DevOps builds and pipelines |
| `azdo_test` | testResults, testPlan, test | Azure DevOps test plans and results |
| `azdo_wiki_search` | wiki, search | Azure DevOps wiki and search |
| `azdo_core` | core, graph, ims, operations, favorite, status, resourceUsage, profile (location, projectAnalysis) | Azure DevOps projects, teams, users |
| `azdo_admin` | security, securityRoles, permissionsReport, serviceEndpoint, hooks, notification, extensionManagement, memberEntitlementManagement, audit, delegatedAuth (featureManagement, settings) | Azure DevOps permissions, connections, hooks |
| `azdo_artifacts` | artifacts, artifactsPackageTypes, symbol | Azure DevOps feeds, packages, symbols |
| `azdo_analytics` | (analytics; the OData entity sets) | Azure DevOps Analytics OData queries |

`generate --stage` adds `, org <your-org>` to each hint (`Azure DevOps repos and PRs, org contoso`), so agents know
which organization the tools reach. utcp allows hints of up to 60 characters, so with an organization name longer
than 10 characters every manual keeps the plain hint above.

Areas without a 7.1 spec (advancedSecurity, delegatedAuth, environments and resourceUsage have only 7.2,
processDefinitions stops at 4.1) are listed where they belong. The client libraries fill in what the organization
serves of them at 7.1 (Advanced Security alerts, process definitions); the rest comes in when the manuals move to
that API version.

**Tools from the client libraries** (with the PAT): every endpoint the organization serves, at a version 7.1 allows,
that the specs lack and a client library describes, for example `azdo_core.get_connection_data` (who the PAT
belongs to, with the three forms of identity other tools take), pull request conflicts, release tags and logs, task
plan timelines and logs, test sessions and flaky tests, wiki page comments, project language metrics. Their names,
parameters and descriptions come from the libraries, their URLs from your organization's route list. The libraries
name no PAT permissions, so these tools take the permissions of the spec's tools for the same endpoint or area
(`report.json` lists them under `tools_with_inferred_scopes`).

**Analytics** (`azdo_analytics`, with the PAT): one tool per Analytics entity set (work items and their history and
snapshots, test runs and results, pipeline runs and jobs, ...), taking the OData options `$select`, `$filter`,
`$apply`, `$orderby`, `$expand`, `$top`, `$skip`, `$skiptoken` and a project. For example:

```sh
utcp call azdo_analytics.query_test_runs '{"project": "MyProject", "$select": "TestRunId,Title,CompletedDate", "$orderby": "CompletedDate desc", "$top": 10}'
```

What a manual contains: every operation of its areas, reads and writes, except those listed below. What an agent
may do is decided by the PAT's permissions (see `generate`'s output and `scopes.md`), not by leaving tools out.

- Reads: every GET, and the POSTs that only read (queries, batch reads, searches, lookups).
- Writes (POST, PUT, PATCH): all of them. Eleven are tested in a sandbox organization with `write-test` (below) and
  have a short hand-written body; the others start their description with `(untested)` and carry the tag `untested`:

  | Manual | Tested tool | What it does |
  | --- | --- | --- |
  | `azdo_git` | `create_pull_request` | Open a pull request (draft, reviewers, labels, linked work items) |
  | `azdo_git` | `update_pull_request` | Edit, publish a draft, abandon, reactivate, auto-complete, complete |
  | `azdo_git` | `create_thread` | New comment thread, general or on lines of a file |
  | `azdo_git` | `create_comment` | Reply in a thread |
  | `azdo_git` | `update_comment` | Edit a comment |
  | `azdo_git` | `update_thread` | Resolve (fixed, closed, ...) or reopen a thread |
  | `azdo_work` | `create_work_item` | New work item (JSON Patch: fields, links) |
  | `azdo_work` | `update_work_item` | Change fields and links, with optional optimistic locking on `rev` |
  | `azdo_work` | `create_attachment` | Upload a file to attach to a work item (`{"file": "<path>"}`) |
  | `azdo_pipelines` | `queue_build` | Run a pipeline on a branch or commit, with runtime parameters |
  | `azdo_pipelines` | `update_build` | Cancel a build, or rerun its failed jobs |

- Deletes: only 19 small ones an agent may need to take back: pull request comments, likes, labels, reviewers,
  statuses and attachments, ref favorites, build and pipeline tags, work item comments and reactions, work items
  (they go to the recycle bin), test cases in a suite, wiki pages, dashboard widgets and favorites (`deletes` in
  `src/config.rs`).

Pushing code is left to `git push`. `generate --read-only` leaves every write out, for a read-only PAT.

Files: operations whose answer is a file or zip (build artifacts and logs, attachments, packages, process templates)
are tools whose template has `"response_type": "file"`: utcp saves the file (to the Downloads folder, or `--out`)
and prints its path. Operations whose body is a file (work item, pull request and wiki attachments, process import)
take `{"file": "<path>"}` (with `offset` and `length` for one chunk of a chunked upload, plus the `Content-Range`
header). Header parameters the specs name differently from the real header are generated under the real name:
`If-Match` (wiki page version), `Content-Range` and `x-ms-continuationtoken`.

Left out (`generate` prints this list, `report.json` has every operation with its reason):

- The other DELETEs (100 from the specs, about 50 from the client libraries): deleting a project, repository,
  branch policy, pipeline, feed, package, user or permission cannot be undone, and an agent that guesses wrong should
  not be able to.
- 3 HEAD operations (the GET of the same thing is included).
- Areas: `account` (hosted on `app.vssps.visualstudio.com`, outside the organization), `tokens`, `tokenAdmin` and
  `tokenAdministration` (they create or revoke PATs), and `governance` (its 7.1 folder has examples only). From the
  client libraries also web UI extension points, the service's own feature flags and telemetry.
- Client-library endpoints that start at API 7.2 (12 on one organization).
- Operations that do not work: one Microsoft is deprecating (404), three test-results reads whose spec lists no
  scope and which only a full-access PAT may call, and about 40 client-library reads that answered 401 to a PAT with
  every permission the report lists, or that the service no longer supports.
- With `--read-only`, also the reads that Azure DevOps allows only with a write-level permission (deployment groups,
  some test reads), since a read-only PAT cannot call them.

## Usage

`azdo-manuals help` shows the same steps. Once per user and organization (needs utcp 0.2.0 or newer):

| Step | Run | What you should see |
| --- | --- | --- |
| 1. Write and stage the manuals | `azdo-manuals generate --org <your-org> --stage` | Without a PAT in your shell: the eight manuals of the specs with their tool counts, what was left out and why, `Staged the 8 manuals in utcp.`, the PAT permissions they need, and the next steps. Agents do not see staged manuals yet. |
| 2. Make a PAT | At `https://dev.azure.com/<your-org>/_usersSettings/tokens`: under Scopes pick "Custom defined", then "Show all scopes", and set the permissions step 1 printed, plus "Analytics: Read": the "All tools" levels, or the "Reads only" levels for agents that should only read. | The new token, shown once. |
| 3. Give it to utcp | Add it as the secret `AZURE_DEVOPS_EXT_PAT`: a line `AZURE_DEVOPS_EXT_PAT=<the token>` in utcp's `.env` file (`utcp doctor` shows it as `Secrets File`), or, if `utcp doctor` shows a `Secrets Command`, where that command reads secrets (add the name to its allowlist if it has one), then `utcp daemon reload-env`. See the secrets section of the [utcp operating guide](../utcp-rs/docs/operating.md). | Nothing yet. |
| 4. Add the rest | Step 1 again with the PAT in the environment of that one command: `AZURE_DEVOPS_EXT_PAT=<the token> azdo-manuals generate --org <your-org> --stage` (the [operating guide](./docs/operating.md#2-the-pat) shows how to keep it out of your shell history) | Nine manuals, and how many tools came from the client libraries and from Analytics. |
| 5. Check | `utcp doctor` | `[SET] AZURE_DEVOPS_EXT_PAT`, listing the azdo manuals, marked `(staged)`. |
| 6. Review | `utcp diff` | Each manual as `new manual, N tools`, or for an update the tools added, removed and changed. |
| 7. Test | `azdo-manuals read-test --org <your-org> --project <a project>` | One line per read tool with its status, then `Results: N ok, ...` (below). |
| 8. Make them active | `utcp promote` | `Promoted 'azdo_git' (186 tools, new)` and so on; agents now find them, and their skills list them. |

`--org` is the name in `https://dev.azure.com/<your-org>`. Step 1 writes the manuals to a private folder,
`~/.local/share/azdo-manuals/<your-org>` (`%LOCALAPPDATA%\azdo-manuals\manuals\<your-org>` on Windows; `--out` picks
another), and stages each one in utcp under its name and hint. Without `--stage` it only writes the files. The
manual names do not contain the organization, so one utcp setup holds the manuals of one organization.
`azdo-manuals <command> help` and `azdo-manuals help <command>` both print the help for one command.

**After upgrading azdo-manuals**, run step 4 again (step 1 with the PAT), then steps 6 to 8: `utcp diff` shows what
the new version changes, and `utcp rollback <manual>` brings back the previous version if something is wrong. The
[operating guide](./docs/operating.md) has the details, and what to do when a step fails.

The manuals contain the organization name in every URL, so keep them private (never in a public repository). The
downloaded specs and client library files are cached in `~/.cache/azdo-manuals` (`%LOCALAPPDATA%\azdo-manuals\cache`
on Windows); `--cache` picks another folder. The permissions are also written to `scopes.md` in the manuals folder,
with the scope id and what each one allows.

Then agents (or you) can use them:

```sh
utcp search 'pull request reviewers'
utcp call azdo_git.get_pull_requests '{"project": "MyProject", "repositoryId": "my-repo"}'
```

To stage by hand instead: `utcp stage azdo_git <folder>/azdo_git.utcp.json --hint 'Azure DevOps repos and PRs'`
(or `utcp register` to make it active at once).

The manuals authenticate with HTTP basic auth: user name `pat` (Azure DevOps ignores it, utcp needs one) and the PAT
as the password, which is how Azure DevOps expects PATs. The one tool outside your organization, the public service
health page (`azdo_core.get_service_status`), is sent no PAT.

## How the manuals are built

- **Tool names** come from the method name in Microsoft's own client library, in snake case (`GetTeamDaysOff` ->
  `get_team_days_off`), else from the operation id. When two operations in one manual would get the same name, the
  one from the manual's first area keeps it (`get_branches` in `azdo_git` is git's) and the others get their area as
  a prefix (`tfvc_get_branches`), or, when one area has several (the package types), their group (`npm_...`,
  `nuget_...`). Tools from the client libraries and Analytics are named after the spec's tools and yield on a clash,
  so they never rename one. `names` in `src/config.rs` sets a name by hand.
- **URLs** use the host each spec names (for example `vsrm.dev.azure.com` for releases, `vssps.dev.azure.com` for
  graph, `vstmr.dev.azure.com` for test results, `feeds.dev.azure.com` for Artifacts) and carry the `api-version`
  each operation needs, including previews such as `7.1-preview.1`. One exception: the spec puts `profile` on
  `app.vssps.visualstudio.com`, which refuses an organization's PAT, so `azdo_core.get_profile` uses the
  organization's `vssps.dev.azure.com/<your-org>`, which serves the same route (`host_overrides` in `src/config.rs`).
  `get_profile` with `{"id": "me"}` returns the PAT owner's name and email. Client-library tools take their URL,
  host and version from the organization's route list: `7.1` where the endpoint is released at 7.1, else
  `7.1-preview.N`.
- **Path parameters** are required. The optional `team` segment of the WIQL routes is left out, since utcp cannot
  drop a segment per call; board and iteration tools keep `team`.
- **Spec corrections:** `required_params` in `src/config.rs` marks parameters the spec calls optional but the service
  requires, and `skip_gets` drops operations that no longer work or that a read-only PAT cannot call. Both were found
  by the live checks below.
- **Request bodies** have their references and inheritance inlined up to three levels, without read-only fields.
  The specs describe many write bodies with the full response type (35 properties for a pull request, 45 for a
  build, mostly set by the service), so `src/bodies.rs` gives each tested write a short schema with only the fields
  the service accepts. Fields the service refuses are left out, such as `keepForever` on a build (deprecated in favor
  of retention leases). Every `application/json-patch+json` body (work items, pull request statuses and properties,
  entitlements, ...) is a JSON Patch array, although the specs describe it as an object.
- **Responses** pass through as Azure DevOps sends them; use `utcp call --fields` to keep only what you need. A few
  responses can be large in a big organization (several MB for work item types or all project tags), and utcp
  refuses responses over 10 MB.
- **PAT scopes:** an operation that lists several scopes accepts any one of them, so the report picks a small set
  that covers every tool, preferring read scopes, and a second set for a PAT that only reads. `generate` prints both
  side by side as the New Token page names them ("Code: Read, write, & manage" for all tools, "Code: Read" for reads).
  Pull request threads and comments need no separate "Pull Request Threads" scope: `vso.code_write` covers them. A PAT
  never allows more than the account that made it may do.

## Checking the manuals against a real organization

Both checks are read-only. They print tool names, status codes and sizes, never the token. `check-routes` skips the
service health tool (`azdo_core.get_service_status`), whose host lets nobody read its route list, and the Analytics
OData tools, which have no route list; it says so and leaves those tools to `read-test`.

```sh
# Every tool's URL must match a route the organization lists, at a version it supports. PAT from AZURE_DEVOPS_EXT_PAT.
azdo-manuals check-routes --org <your-org>
# Call every read tool through utcp, taking ids from earlier responses (needs utcp on PATH or --utcp).
azdo-manuals read-test --org <your-org> --project <project> --seed repositoryId=<repo-with-pull-requests>
```

`read-test` calls the tools through your utcp, exactly as agents do (through the daemon when it runs), with the PAT
utcp already has, so it needs no PAT in your shell. Manuals that are staged are called with `utcp call --staged`, so
you can test them before `utcp promote`; the others are called as they are active. The tool definitions come from
the folder `generate` wrote (`--manuals <folder>` when it is not the default one for `--org`). `--seed name=value`
fixes a parameter by name: with `repositoryId` it reads that repository's pull requests, commits and threads,
which gives a fuller test than the first repository it finds. `--team` picks the team for board tools (default: the
project's default team), `--only <manual>` tests one manual. It writes `readtest.json` next to the manuals with
every call's status, size and arguments; that file contains organization data, so keep it as private as the
manuals. Tools it found no ids for are listed as not exercised, and a few reads can fail because of the
organization's settings or data (an admin-only read, a disabled feature); the
[operating guide](./docs/operating.md#5-reading-the-read-test-results) lists the known ones.

### Write test (sandbox organization only)

`write-test` calls the tested write tools through utcp in a throwaway project. Run it only in a free organization of
your own, never in a work organization: it creates a project and deletes every project whose name starts with
`azdo-manuals-test-` (leftovers of earlier runs), so it refuses to start without `--create-and-delete-projects`.
It uses its own throwaway utcp setup in a temporary folder (never your manuals, daemon or agent skills) and takes
the sandbox organization's PAT from `AZURE_DEVOPS_EXT_PAT` in the environment.

```sh
azdo-manuals write-test --org <sandbox-org> --manuals <folder> --create-and-delete-projects
```

- **PAT:** the scopes in `scopes.md`, plus "Project and Team: Read, write, & manage" to create and delete the
  project.
- **Fixtures**, made with direct REST calls: the project `azdo-manuals-test-<UTC time>`, a repository with `main`
  and two feature branches, and a YAML pipeline on the self-hosted `Default` pool. A new organization has no agents
  in that pool, so queued builds wait and can be cancelled without using build minutes.
- **Checks:** each write's answer must show what was sent (a field the service ignores fails the test), and the
  effects that happen later (auto-complete, completion, cancellation) are polled for. A tested write the test did not
  call is reported as not run; writes tagged `untested` are not expected.
- **Output:** tool names and statuses; `writetest.json` next to the manuals holds the arguments and answers. The
  project is deleted at the end (Azure DevOps keeps deleted projects restorable for 28 days).

## Maintainers

- `src/config.rs` holds the grouping and everything left out, each with its reason; `src/bodies.rs` the bodies of
  the tested writes. `generate` refuses a configuration that names an operation the specs do not have, and a test
  refuses an area of the spec folder that is neither in a manual nor excluded with a reason.
- `azdo-manuals pins --pins pins.json` (run in this folder) records the file hashes of an archive revision and the
  list of all areas. It costs about 80 of the archive's 120 requests per hour and continues where it stopped if it
  runs out. `azdo-manuals pins-sdk` records the client library files of the latest commits (or `--go-commit`,
  `--node-commit`) in the same file. The binary embeds `pins.json`, so rebuild after changing it.
- `azdo-manuals route-gaps --org <org>` (PAT in the environment) lists the organization's live endpoints that no
  tool covers: left out by a rule (with the reason), or described neither by the specs nor by a client library. It
  writes `route-gaps.json` next to the manuals.
- Tests: `cargo test` (offline; set `UTCP_BIN` to a utcp binary, 0.2.0 or newer, to also stage the manuals in a
  throwaway utcp).
- Version: the REST API version plus a counter, so `7.1.x` for API 7.1 (`azdo-manuals --version`).
- How it works inside, file formats and design decisions: [`docs/azdo-manuals-spec.md`](./docs/azdo-manuals-spec.md).
- Rules for changes and releases: [`AGENTS.md`](./AGENTS.md).

## License

MIT, like the rest of this repository. The specs and client libraries this generator reads are Microsoft's,
published under the MIT License, and are downloaded at run time, not stored here; see [`NOTICE`](./NOTICE).
