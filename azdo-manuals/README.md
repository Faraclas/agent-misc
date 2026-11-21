# azdo-manuals

Generates [utcp](../utcp-rs/) HTTP manuals for the Azure DevOps REST API, so agents call Azure DevOps through utcp
(`search` -> `schema` -> `call`) instead of loading an MCP server's tool list into every conversation.

- **Source:** Microsoft's own API specs (Swagger 2.0), REST API version 7.1. Their GitHub repository
  (`MicrosoftDocs/vsts-rest-api-specs`) no longer exists, so the generator downloads the files from the
  [Software Heritage](https://www.softwareheritage.org/) archive of it (revision of 2026-09-02, "Update REST API specs
  from releases/M279") and checks each one against the hash recorded in [`pins.json`](./pins.json), which is built
  into the binary. Nothing from Microsoft is stored in this repository.
- **Output:** six manuals for one organization (385 tools: 375 reads, 10 writes), a `scopes.md` listing the personal
  access token (PAT) scopes the manuals need, and a `report.json` (tools, sizes, and every operation left out with the
  reason).
- **Requirements:** one static binary, no runtime. Network access to `archive.softwareheritage.org` for the first run
  (about 30 seconds, 21 downloads); later runs use the cache and take well under a second.

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

| Manual | Spec areas | Hint for `--hint` |
| --- | --- | --- |
| `azdo_git` | git, policy | Azure DevOps repos and PRs |
| `azdo_work` | wit, work | Azure DevOps work items and boards |
| `azdo_pipelines` | build, pipelines, release, approvalsAndChecks, distributedTask | Azure DevOps builds and pipelines |
| `azdo_test` | testResults, testPlan, test | Azure DevOps test plans and results |
| `azdo_wiki_search` | wiki, search | Azure DevOps wiki and search |
| `azdo_core` | core, graph, ims | Azure DevOps projects, teams, users |

What a manual contains:

- Every GET of its areas, except the few that return only binary content (zip files, images).
- POSTs that only read: WIQL queries, batch reads, code, wiki and work item search, graph subject lookups.
- Ten writes, each tested in a sandbox organization with `write-test` (below):

  | Manual | Tool | What it does |
  | --- | --- | --- |
  | `azdo_git` | `create_pull_request` | Open a pull request (draft, reviewers, labels, linked work items) |
  | `azdo_git` | `update_pull_request` | Edit, publish a draft, abandon, reactivate, auto-complete, complete |
  | `azdo_git` | `create_thread` | New comment thread, general or on lines of a file |
  | `azdo_git` | `create_comment` | Reply in a thread |
  | `azdo_git` | `update_comment` | Edit a comment |
  | `azdo_git` | `update_thread` | Resolve (fixed, closed, ...) or reopen a thread |
  | `azdo_work` | `create_work_item` | New work item (JSON Patch: fields, links) |
  | `azdo_work` | `update_work_item` | Change fields and links, with optional optimistic locking on `rev` |
  | `azdo_pipelines` | `queue_build` | Run a pipeline on a branch or commit, with runtime parameters |
  | `azdo_pipelines` | `update_build` | Cancel a build, or rerun its failed jobs |

  Pushing code is left to `git push`. DELETE is never generated. `generate --read-only` leaves the writes out, for
  a read-only PAT.

Left out on purpose:

- Security, tokens, audit, service hooks, extensions, process templates, Advanced Security, Artifacts and the other
  admin areas.
- `environments`: it exists only as a 7.2 preview, so it is not in the 7.1 manuals.
- `profile`: its one operation runs on `app.vssps.visualstudio.com`, outside the organization, and refuses a PAT
  scoped to one organization.
- Eight reads that Azure DevOps allows only with a write-level scope (deployment groups and targets, environments),
  so the PAT needs no more than the writes above. `allow_write_scopes` in `src/config.rs` brings them back.
- Three test-results reads whose spec lists no scope and which a PAT with every read scope cannot call.

`report.json` lists every operation left out, with the reason.

## Usage

```sh
azdo-manuals generate --org <your-org> --register
```

`--org` is the name in `https://dev.azure.com/<your-org>`. This writes the manuals to a private folder,
`~/.local/share/azdo-manuals/<your-org>` (`%LOCALAPPDATA%\azdo-manuals\manuals\<your-org>` on Windows; `--out` picks
another), and registers each one with utcp under its name and hint. Run the same command again to refresh them, for
example after upgrading azdo-manuals. Without `--register` it only writes the files.

The manuals contain the organization name in every URL, so keep them private (never in a public repository). The
downloaded specs are cached in `~/.cache/azdo-manuals` (`%LOCALAPPDATA%\azdo-manuals\cache` on Windows); `--cache`
picks another folder.

Then create a PAT with the scopes listed in `scopes.md` and make it available to utcp as `AZURE_DEVOPS_EXT_PAT` (see
the secrets section of the [utcp operating guide](../utcp-rs/docs/operating.md)):

```sh
utcp search 'pull request reviewers'
utcp call azdo_git.get_pull_requests '{"project": "MyProject", "repositoryId": "my-repo"}'
```

To register by hand instead: `utcp register azdo_git <folder>/azdo_git.utcp.json --hint 'Azure DevOps repos and PRs'`.

The manuals authenticate with HTTP basic auth: user name `pat` (Azure DevOps ignores it, utcp needs one) and the PAT
as the password, which is how Azure DevOps expects PATs.

## How the manuals are built

- **Tool names** come from the method name in Microsoft's own client library, in snake case (`GetTeamDaysOff` ->
  `get_team_days_off`), else from the operation id. When two operations in one manual would get the same name, both
  get their area as a prefix (`build_get_folders`, `release_get_folders`). `names` in `src/config.rs` sets a name by
  hand.
- **URLs** use the host each spec names (for example `vsrm.dev.azure.com` for releases, `vssps.dev.azure.com` for
  graph, `vstmr.dev.azure.com` for test results) and carry the `api-version` each operation needs, including
  previews such as `7.1-preview.1`.
- **Path parameters** are required. The optional `team` segment of the WIQL routes is left out, since utcp cannot
  drop a segment per call; board and iteration tools keep `team`.
- **Spec corrections:** `required_params` in `src/config.rs` marks parameters the spec calls optional but the service
  requires, and `skip_gets` drops operations that no longer work or that a read-only PAT cannot call. Both were found
  by the live checks below.
- **Request bodies** have their references and inheritance inlined up to three levels, without read-only fields.
  The writes are the exception: the specs describe their bodies with the full response type (35 properties for a
  pull request, 45 for a build, mostly set by the service), and the work item body as an object although it is a
  JSON Patch array. `src/bodies.rs` gives each write a short schema with only the fields the service accepts. Fields
  the service refuses are left out, such as `keepForever` on a build (deprecated in favor of retention leases).
- **Responses** pass through as Azure DevOps sends them; use `utcp call --fields` to keep only what you need. A few
  responses can be large in a big organization (several MB for work item types or all project tags), and utcp
  refuses responses over 10 MB.
- **PAT scopes:** an operation that lists several scopes accepts any one of them, so the report picks a small set
  that covers every tool, preferring read scopes. With the writes this is `vso.code_write`, `vso.work_write` and
  `vso.build_execute` ("Code: Read & write", "Work Items: Read & write", "Build: Read & execute") plus read scopes.
  Pull request threads and comments need no separate "Pull Request Threads" scope: `vso.code_write` covers them.

## Checking the manuals against a real organization

Both checks are read-only and take the PAT from `AZURE_DEVOPS_EXT_PAT`. They print tool names, status codes and
sizes, never the token. `read-test` skips the write tools.

```sh
# Every tool's URL must match a route the organization lists, at a version it supports.
azdo-manuals check-routes --org <your-org> --manuals <folder>
# Call every read tool through utcp, taking ids from earlier responses (needs utcp on PATH or --utcp).
azdo-manuals read-test --manuals <folder> --project <project> --seed repositoryId=<repo-with-pull-requests>
```

`read-test` uses its own throwaway utcp configuration, so it neither needs nor changes your registered manuals. It
writes `readtest.json` next to the manuals with every call's status, size and arguments; that file contains
organization data, so keep it as private as the manuals. Tools it found no ids for are listed as not exercised.

### Write test (sandbox organization only)

`write-test` calls every write tool through utcp in a throwaway project. Run it only in a free organization of your
own, never in a work organization: it creates a project and deletes every project whose name starts with
`azdo-manuals-test-` (leftovers of earlier runs), so it refuses to start without `--create-and-delete-projects`.

```sh
azdo-manuals write-test --org <sandbox-org> --manuals <folder> --create-and-delete-projects
```

- **PAT:** the scopes in `scopes.md`, plus "Project and Team: Read, write, & manage" to create and delete the
  project.
- **Fixtures**, made with direct REST calls: the project `azdo-manuals-test-<UTC time>`, a repository with `main`
  and two feature branches, and a YAML pipeline on the self-hosted `Default` pool. A new organization has no agents
  in that pool, so queued builds wait and can be cancelled without using build minutes.
- **Checks:** each write's answer must show what was sent (a field the service ignores fails the test), and the
  effects that happen later (auto-complete, completion, cancellation) are polled for. A write tool the test did not
  call is reported as not run.
- **Output:** tool names and statuses; `writetest.json` next to the manuals holds the arguments and answers. The
  project is deleted at the end (Azure DevOps keeps deleted projects restorable for 28 days).

## Maintainers

- `src/config.rs` holds the grouping and every inclusion decision; `src/bodies.rs` the write bodies. `generate`
  refuses a configuration that names an operation the specs do not have.
- `azdo-manuals pins --pins pins.json` (run in this folder) records the file hashes of an archive revision. It costs
  about 40 of the archive's 120 requests per hour and continues where it stopped if it runs out. The binary embeds
  `pins.json`, so rebuild after changing it.
- Tests: `cargo test` (offline; set `UTCP_BIN` to a utcp binary to also load the manuals in utcp).
- Rules for changes and releases: [`AGENTS.md`](./AGENTS.md).

## License

MIT, like the rest of this repository. The specs this generator reads are Microsoft's, published under the MIT
License, and are downloaded at run time, not stored here; see [`NOTICE`](./NOTICE).
