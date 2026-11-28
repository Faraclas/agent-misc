# Operating azdo-manuals

For people who run `azdo-manuals` for their own Azure DevOps organization. The [README](../README.md) covers
installation and the first setup (generate, PAT, review, test, promote). This guide covers everything after that: the
files it writes, the PAT, upgrades and rollback, reading the test results, checking that agents find the tools,
troubleshooting, Windows, and networks with a proxy or TLS inspection. How the generator works inside is in the
[specification](./azdo-manuals-spec.md).

`azdo-manuals` only writes manuals. Everything an agent does goes through [utcp](../../utcp-rs/): its staging area,
its secrets and its daemon. The [utcp operating guide](../../utcp-rs/docs/operating.md) is the reference for those.

## 1. Files and folders

| What | Linux | Windows |
| :--- | :--- | :--- |
| The manuals and their reports (`--out` picks another folder) | `~/.local/share/azdo-manuals/<org>/` (`$XDG_DATA_HOME` when set) | `%LOCALAPPDATA%\azdo-manuals\manuals\<org>\` |
| Downloaded API descriptions and client library files (about 13 MB; `--cache` picks another folder) | `~/.cache/azdo-manuals/` (`$XDG_CACHE_HOME` when set) | `%LOCALAPPDATA%\azdo-manuals\cache\` |

In the manuals folder:

| File | Written by | Holds |
| :--- | :--- | :--- |
| `azdo_*.utcp.json` | `generate` | The manuals (nine with the PAT, eight without). Your organization's name is in every URL. |
| `report.json` | `generate` | Every manual's tools, size, hint and permissions, and every operation left out with its reason. |
| `scopes.md` | `generate` | The PAT permissions, as the New Token page names them, and what each one allows. |
| `readtest.json` | `read-test` | Every read tool's status, size, time and the arguments it was called with. |
| `route-gaps.json` | `route-gaps` | Every live endpoint of the organization, and whether a tool covers it (maintainers). |
| `writetest.json` | `write-test` | The arguments and answers of the write test (maintainers, sandbox organization only). |

All of them name your organization, and the test results also hold its data (project names, ids, user names). Keep
the folder private: never commit it to a public repository. `readtest.json` is replaced by every run.

The cache holds Microsoft's files, each checked against a hash built into the binary. It never goes stale; delete it
to free the space, and the next `generate` downloads the files again.

`generate` also stages the manuals in utcp (`--stage`). utcp keeps its own copy, so agents keep working if you delete
the manuals folder; only `read-test` and the next `generate` need it.

## 2. The PAT

The manuals send a personal access token (PAT) that utcp reads from its secret `AZURE_DEVOPS_EXT_PAT`. The manuals
only hold the placeholder `${AZURE_DEVOPS_EXT_PAT}`, never the token.

### Which permissions

`generate` prints them, and `scopes.md` has the same list with the scope ids. Two columns:

- **All tools:** agents can use every tool, including the writes.
- **Reads only:** agents can read, and every write answers 401. A `-` means: leave that permission unset.

A PAT never allows more than the account that made it may do. A tool whose permission the PAT lacks answers 401;
the other tools keep working. When you are not sure the PAT has a permission, compare it with `scopes.md`, or run
`read-test`: reads that answer 401 while others answer 200 point at a missing permission.

For a PAT that only reads, `generate --read-only` leaves out the write tools altogether (and the few reads that
Azure DevOps allows only with a write-level permission), so agents do not see tools that would fail.

### Giving it to utcp

Either:

- add the line `AZURE_DEVOPS_EXT_PAT=<the token>` to utcp's `.env` file (`utcp doctor` shows its path as
  `Secrets File`), or
- if `utcp doctor` shows a `Secrets Command`, store the token where that command reads secrets. If the command has an
  allowlist (the example script does), add `AZURE_DEVOPS_EXT_PAT` to it.

Then tell the daemon:

```sh
utcp daemon reload-env
```

Check:

```sh
utcp doctor
```

It should show `[SET] AZURE_DEVOPS_EXT_PAT` next to each azdo manual (marked `(staged)` while they are staged).

### Renewing it

PATs expire (the organization may cap their lifetime). Make a new one with the same permissions, put it where the old
one was, and run `utcp daemon reload-env`. The manuals do not change, so there is nothing to regenerate. An expired
PAT makes every call answer 401.

### Commands that read the PAT from the environment

`read-test` never needs the PAT in your shell: it calls through utcp, which has it (`search-test` only searches and
needs no PAT at all). The others read
`AZURE_DEVOPS_EXT_PAT` from the environment of the shell you run them in, and print tool names, statuses and sizes,
never the token:

- **`generate`** (optional): with the PAT it reads your organization's live route list and the Analytics description,
  and adds about 400 tools that Microsoft's REST API descriptions leave out, plus the `azdo_analytics` manual.
  Without it, the manuals hold what the descriptions have, and `generate` says so. `--spec-only` ignores a PAT that
  is set.
- **`check-routes`, `route-gaps`, `write-test`** (maintainers): required.

Set it for that one command rather than in your shell profile, from wherever you keep it, so the token does not land
in your shell history, for example `AZURE_DEVOPS_EXT_PAT="$(<command that prints it>)" azdo-manuals generate ...`.
The same PAT that utcp uses is right: `generate` only reads.

## 3. Upgrading azdo-manuals

A new version can change the manuals (new tools, fixed parameters, new hints). After installing it:

1. Write and stage the new manuals (with the PAT in the environment, section 2, for the full set):

   ```sh
   azdo-manuals generate --org contoso --stage
   ```

2. See what changes for your agents. `utcp diff 'azdo_*'` lists, per manual, the tools added (`+`), removed (`-`)
   and changed (`~`):

   ```sh
   utcp diff 'azdo_*'
   ```

3. Test the staged manuals (read-only; see section 5):

   ```sh
   azdo-manuals read-test --org contoso --project Fabrikam
   ```

4. Check that agents still find the tools with the words they use (read-only; see section 6):

   ```sh
   azdo-manuals search-test --staged
   ```

5. Make them active:

   ```sh
   utcp promote 'azdo_*'
   ```

Replace `contoso` with your organization and `Fabrikam` with one of its projects. `'azdo_*'` (quoted, so the shell
leaves the `*` alone) picks only these manuals when others are staged too; it needs utcp 0.2.4 or newer.

If the staged manuals look wrong, `utcp discard 'azdo_*'` drops them and the active ones stay. If you find a problem
after promoting, `utcp rollback azdo_git` (one manual at a time) brings back the version the promote replaced;
running it again undoes the rollback.

The same steps switch between full and read-only manuals: run `generate` with or without `--read-only`, then review
and promote.

### Hints

Each manual is staged with a hint, the few words agents see next to its name in the utcp skill. The hint ends with
`, org <org>` (for example `Azure DevOps repos and PRs, org contoso`), so agents know which organization the tools
reach: the organization is fixed in every URL, and no tool takes it as a parameter. utcp allows hints of up to 60
characters; when your organization's name is too long for every hint to fit (more than 10 characters), all the
manuals get the plain hint instead. `utcp set-hint` changes a hint by hand, but the next `generate --stage` sets it
again.

## 4. One organization per utcp setup

The manual names (`azdo_git`, `azdo_work`, ...) do not contain the organization. Generating and promoting the manuals
of a second organization replaces those of the first. Agents on one machine therefore work with one organization.

## 5. Reading the read-test results

`read-test` calls every read tool through your utcp, exactly as an agent does, and prints one line per tool:

```text
  ok           17269 azdo_work.get_list_of_processes
  http 400           azdo_work.get_deleted_work_item_shallow_references
```

then `Results: N ok, ...`. It exits with 1 when any tool failed, 0 otherwise.

| Status | Meaning |
| :--- | :--- |
| `ok` | The call succeeded; the number is the size of the answer in bytes (for a file answer, the file's size). |
| `http NNN` | Azure DevOps answered with that status. `readtest.json` has the start of its error text under `error`. |
| `error` | utcp failed without an HTTP status (no connection, a refused argument). `error` has utcp's message. |
| `not exercised` | Not called. `reason` says why (below). |

How it finds arguments: `--project` and `--team` are used wherever a tool takes them, and ids are chained from
earlier answers (a tool that needs a pull request id gets the first one the list of pull requests returned). Tools
run shortest URL first, so list tools come before the tools that need their ids.

Reasons a tool is not exercised:

| `reason` | What it means | What helps |
| :--- | :--- | :--- |
| `not a read` | A write. Only the write test calls those. | Nothing; expected. |
| `waiting for <tool>` | The tool that would give it an id failed or returned nothing. | Fix that tool's failure, or `--seed`. |
| `no value for {param}` | No earlier answer has an id for this URL part. | `--seed param=value`. |
| `required parameter <name>` | A required query parameter has no known value. | `--seed name=value`. |
| `no sample body` | A read-only POST that needs a body the test does not have. | Nothing; expected. |

The `not exercised` count includes every write. Many reads are not exercised either, mostly because the
organization has no data of that kind (no test plans, no feeds, no TFVC). That is not a failure.

`--seed name=value` fixes a parameter everywhere it appears, before any chaining. `--seed repositoryId=<a repository
with pull requests>` gives a much fuller test of `azdo_git` than the first repository the test finds. `--only
azdo_work` tests one manual (repeat it for more); a test of one manual cannot borrow ids from the others, so a few
more tools stay unexercised.

### Failures that are expected

Some reads fail because of the organization's settings, its data or your account, not because of the manuals. These
are known; when you see them there is nothing to fix:

| Tool (manual) | Status | Why |
| :--- | :--- | :--- |
| Entitlement, audit log, security namespace, deleted and recycle-bin repository reads, Artifacts global permissions (`azdo_admin`, `azdo_git`, `azdo_artifacts`) | `http 403` | They need an organization or project administrator. A PAT never allows more than its account. |
| `get_deleted_work_item_shallow_references` (`azdo_work`) | `http 400` | The project has more than 20,000 deleted work items; the service refuses to list that many. |
| `get_work_item_columns` (`azdo_work`) | `http 400` | The team's taskboard columns were never customized. |
| `get_coverage_status_badge` (`azdo_test`) | `http 404` | The feature is turned off in the organization. |
| `get_definition_yaml` (`azdo_pipelines`) | `http 404` | The pipeline the test picked is a classic (designer) pipeline, which has no YAML. `--seed definitionId=<a YAML pipeline's id>` tests it. |
| `get_merge_bases` (`azdo_git`) | `http 500` | The service could not find one of the two commits in the repository. |
| `get_estimated_billable_pushers_details_for_org` (`azdo_git`) | `http 403` | Advanced Security billing estimates need an organization administrator ("VS2150012"). |
| `get_detected_build_frameworks` (`azdo_pipelines`) | `http 400` | It needs the repository's GUID; a `--seed repositoryId=<name>` gives a name. `--seed repositoryId=<GUID>` tests it. |
| `get_definition_environments` (`azdo_pipelines`) | `http 400` | It needs `taskGroupId` or `propertyFilters`, one of the two; the test has neither. |
| `get_descriptor_by_id` (`azdo_core`) | `http 400` | Only with `--seed id=me`: that seed is for `get_profile`, and this tool takes another kind of id. |

`get_form_layout` and `get_system_controls` read an inherited process; Microsoft's built-in processes answer 400
("work item types are locked"). The test picks the first inherited process, and when the organization has none the
two tools stay not exercised.

Analytics snapshot sets (`query_work_item_snapshot`, `query_work_item_board_snapshot`, ...) hold millions of rows in a
large organization; unfiltered they do not answer within utcp's 60 seconds. The test filters them to the last days,
and their tool descriptions tell agents to do the same.

### Large answers

Some reads answer with several MB (all project tags, all work item types). utcp saves a JSON answer over 10 MB to a
file and prints a reference instead of the JSON; `read-test` reads it from there and deletes it. Tools that answer
with a file (logs, zips, attachments) are saved to a temporary file and deleted once counted; nothing lands in your
Downloads folder.

## 6. Checking that agents find the tools

Agents find tools with `utcp search "<words>"`, and only see the first few results. `search-test` runs a list of
searches an agent would type, each with the tools that count as a right answer, through your utcp, and checks that
one of them ranks in the top 3 (`--top N`, 1 to 10, changes that). It runs only `utcp list` and `utcp search` and
changes nothing, and its output names no organization data.

Run it after `generate --stage`, with `--staged` so the staged manuals are searched as if promoted, and before
`utcp promote 'azdo_*'` (section 3, step 4). Without `--staged` it checks what your agents see now.

```text
Searching the active and staged manuals (utcp search --staged --limit 10); a case passes with a tool in the top 3.
query                  rank  tool                                                                 result
current user              1  github_users.get_authenticated_user                                  PASS
list builds               2  azdo_pipelines.get_builds                                            PASS
list projects             -  azdo_core.get_projects                                               KNOWN (Azure DevOps calls it "Get all projects", and search has no synonyms)
who am i                  -  azdo_core.get_connection_data | github_users.get_authenticated_user  FAIL
...

20 cases: 20 searched, 0 skipped.
Found: top-1 13 of 20, top-3 18 of 20, top-5 18 of 20.
1 known miss; 1 failure (not in the top 3).
```

| Result | Meaning |
| :--- | :--- |
| `PASS` | One of the right tools is in the top 3; `rank` is the best one's place, `tool` names it. |
| `FAIL` | None is in the top 3. `rank` is `-` when none is in the first 10 results; `tool` then lists the right ones. |
| `KNOWN` | A known miss: reported, with its reason, but not a failure. |
| `SKIP` | None of the case's tools belongs to a registered manual (the GitHub tools when `github-manuals` is not registered, or a manual that is only staged when `--staged` is missing). |

It exits with 1 when a case failed, 0 otherwise. The searches are built in (`tests/search-checks.txt` in the source);
`--cases <file>` uses your own, one per line: `query words -> manual.tool | manual.other_tool`, with
`[known miss: why]` at the end for a known miss and `#` for comments. A case may list tools of several manual
families: tools of manuals you have not registered are ignored.

A `FAIL` means one of two things:

- **The manuals do not say what people call the thing.** Microsoft's text names it differently, or not at all
  (a developer comment instead of a description). Fix it in `src/config.rs`: a `summaries` entry for a first sentence
  that tells an agent nothing, or a `notes` entry for the words people search for. Never add odd wording just to make
  a search pass; the text must stay true and read well to an agent.
- **utcp ranks badly.** The right tool's name and description do contain the words, but other tools rank above it.
  That is a utcp bug: report it to [utcp-rs](../../utcp-rs/) with the query and the tools.

When a known miss passes, the totals say so: remove its marker.

## 7. Troubleshooting

| You see | Cause | Do |
| :--- | :--- | :--- |
| `'<name>' is not a valid Azure DevOps organization name` | `--org` must be the name in `https://dev.azure.com/<org>`: letters, digits and `-`. | Use only the name, not the URL. |
| `the archive's rate limit (120 requests per hour) is used up; try again after HH:MM` | A first `generate` downloads 46 files; several cold runs in one hour can use up the archive's limit. | Wait until the time it names. Files already downloaded stay cached. |
| `cannot reach the archive (...)` | No network path to `archive.softwareheritage.org`. | Check the network, proxy and CA (section 9). |
| `<file>: the archive sent content with hash ..., pinned ...` | The download is not the pinned file (a proxy changed it, or the archive is broken). | Retry; if it repeats, report it. Nothing was cached. |
| `resourceAreas: HTTP 401 (the PAT is not valid for this organization)` (`generate`) | `AZURE_DEVOPS_EXT_PAT` in your shell is wrong, expired, or for another organization. | Fix it, unset it, or run with `--spec-only`. |
| `cannot download https://codeload.github.com/...` | No network path to GitHub, which serves Microsoft's client libraries. | Check the network and proxy (section 9), or run with `--spec-only`. |
| `microsoft/azure-devops-...@<commit>: <file> has hash ..., pinned ...` | GitHub sent a file other than the pinned one. | Retry; if it repeats, report it. |
| `Analytics left out: Analytics $metadata: HTTP 401` | The PAT lacks "Analytics: Read". | Add it to the PAT; the other manuals are written anyway. |
| `<path> is utcp 0.2.3; these manuals need utcp 0.2.4 or newer, ...` (`generate --stage`, `read-test`, `search-test`, `write-test`) | The manuals leave out optional URL segments such as `{team}` when they are not passed, which older utcp sends as they are. | Upgrade utcp, or pass `--utcp <newer utcp>`. |
| `utcp stage <manual> failed (output above)` | utcp's own message above says why. | Fix that, then run `generate --stage` again. |
| `no utcp binary (use --utcp)` | utcp is not on `PATH`. | Put it on `PATH`, or pass `--utcp <path>`. |
| `utcp has neither a staged nor an active manual named ...` (`read-test`) | The manuals were generated but never staged or registered. | `azdo-manuals generate --org contoso --stage`. |
| `search-test` shows `SKIP` for every azdo case | The manuals are only staged, and `--staged` is missing (or they are not in utcp at all). | `azdo-manuals search-test --staged`, or stage them first. |
| `search-test` shows `FAIL` | The right tool is not in the top 3 of `utcp search`. | Section 6. |
| `utcp doctor` shows `[MISSING] AZURE_DEVOPS_EXT_PAT` | utcp has no value for the secret. | Section 2, then `utcp daemon reload-env`. |
| Every call answers 401 | The PAT is wrong, expired or revoked, or belongs to another organization. | Make a new PAT (section 2). |
| Some calls answer 401, others work | The PAT lacks the permission those tools need. | Compare the PAT with `scopes.md`. |
| A tool answers 401 although the PAT has every permission in `scopes.md` | It comes from Microsoft's client libraries, which name no permissions; its permission was inferred (`tools_with_inferred_scopes` in `report.json`) and is wrong. | Report it; it is then left out with that reason. |
| A call prints an HTML page (a sign-in page) instead of JSON | Azure DevOps did not receive usable credentials. | Check `utcp doctor` and the secret's value; reload the daemon. |
| 403 | Your account may not do that (an admin-only read or write). | Expected for those tools; the PAT cannot grant more. |
| 404 on a tool that should work | A disabled feature, a classic pipeline (no YAML), or an id from another project. | See "Failures that are expected" in section 5. |
| A write answers 400 with a field error | The body differs from what the service accepts. Writes marked `(untested)` were never checked live. | Read the tool's schema (`utcp schema <manual>.<tool>`) and the service's message; report it if the schema is wrong. |

## 8. Windows

- Put `azdo-manuals.exe` in a folder on `PATH`; next to `utcp.exe` (`%LOCALAPPDATA%\Programs\utcp`, where utcp's
  installer puts it) is simplest.
- Folders are under `%LOCALAPPDATA%` (section 1). `%LOCALAPPDATA%` is Command Prompt syntax; in PowerShell write
  `$env:LOCALAPPDATA`.
- utcp's installer starts its daemon at logon through the scheduled task "utcp daemon". After changing the PAT, run
  `utcp daemon reload-env` as on Linux.

## 9. Proxies and TLS inspection

`azdo-manuals` and utcp handle the network the same way:

- **Certificates:** both trust the operating system's certificate store (on Windows the Windows store, on Linux the
  system CA bundle). A company root CA that IT installed for TLS inspection is accepted by both.
- **Proxies:** both use `HTTPS_PROXY` (or `ALL_PROXY`) from the environment. Neither reads the Windows system proxy
  settings (Internet Options, WinHTTP, PAC files). Behind a proxy that only the system settings name, set
  `HTTPS_PROXY` for the shell that runs `azdo-manuals` and for utcp's daemon.

Hosts they contact:

| Host | Who, when |
| :--- | :--- |
| `archive.softwareheritage.org` | `generate`, only for files not yet in the cache (the first run, or a new version's new pins). |
| `codeload.github.com` | `generate` with the PAT, only when the client library files are not yet in the cache (two downloads). |
| `api.github.com` | `pins-sdk` (maintainers), to find a branch's latest commit. |
| `dev.azure.com` and its service hosts (`vssps`, `vsrm`, `vstmr`, `vsaex`, `feeds`, `pkgs`, `artifacts`, `almsearch`, `auditservice`, `extmgmt`, `advsec`, `analytics`, ... `.dev.azure.com`) | utcp, for the tools; `read-test` through utcp. `generate` with the PAT, `check-routes` and `route-gaps` read the route list of every host the organization's `resourceAreas` names (about 28). |
| `status.dev.azure.com` | utcp, for `azdo_core.get_service_status` only (public, sent no PAT). |
