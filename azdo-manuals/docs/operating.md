# Operating azdo-manuals

For people who run `azdo-manuals` for their own Azure DevOps organization. The [README](../README.md) covers
installation and the first setup (generate, PAT, review, test, promote). This guide covers everything after that: the
files it writes, the PAT, upgrades and rollback, reading the test results, troubleshooting, Windows, and networks with
a proxy or TLS inspection. How the generator works inside is in the [specification](./azdo-manuals-spec.md).

`azdo-manuals` only writes manuals. Everything an agent does goes through [utcp](../../utcp-rs/): its staging area,
its secrets and its daemon. The [utcp operating guide](../../utcp-rs/docs/operating.md) is the reference for those.

## 1. Files and folders

| What | Linux | Windows |
| :--- | :--- | :--- |
| The manuals and their reports (`--out` picks another folder) | `~/.local/share/azdo-manuals/<org>/` (`$XDG_DATA_HOME` when set) | `%LOCALAPPDATA%\azdo-manuals\manuals\<org>\` |
| Downloaded API descriptions (about 5 MB; `--cache` picks another folder) | `~/.cache/azdo-manuals/` (`$XDG_CACHE_HOME` when set) | `%LOCALAPPDATA%\azdo-manuals\cache\` |

In the manuals folder:

| File | Written by | Holds |
| :--- | :--- | :--- |
| `azdo_*.utcp.json` | `generate` | The eight manuals. Your organization's name is in every URL. |
| `report.json` | `generate` | Every manual's tools, size, hint and permissions, and every operation left out with its reason. |
| `scopes.md` | `generate` | The PAT permissions, as the New Token page names them, and what each one allows. |
| `readtest.json` | `read-test` | Every read tool's status, size, time and the arguments it was called with. |
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

`read-test` and `generate` never need the PAT in your shell: `read-test` calls through utcp, which has it. Only the
maintainer commands `check-routes` and `write-test` read `AZURE_DEVOPS_EXT_PAT` from the environment of the shell
you run them in. They print tool names, statuses and sizes, never the token.

## 3. Upgrading azdo-manuals

A new version can change the manuals (new tools, fixed parameters, new hints). After installing it:

1. Write and stage the new manuals:

   ```sh
   azdo-manuals generate --org contoso --stage
   ```

2. See what changes for your agents. `utcp diff` lists, per manual, the tools added (`+`), removed (`-`) and changed
   (`~`):

   ```sh
   utcp diff
   ```

3. Test the staged manuals (read-only; see section 5):

   ```sh
   azdo-manuals read-test --org contoso --project Fabrikam
   ```

4. Make them active:

   ```sh
   utcp promote
   ```

Replace `contoso` with your organization and `Fabrikam` with one of its projects.

If the staged manuals look wrong, `utcp discard` drops them and the active ones stay. If you find a problem after
promoting, `utcp rollback azdo_git` (one manual at a time) brings back the version the promote replaced; running it
again undoes the rollback.

The same steps switch between full and read-only manuals: run `generate` with or without `--read-only`, then review
and promote.

### Hints

Each manual is staged with a hint, the few words agents see next to its name in the utcp skill. The hint ends with
`, org <org>` (for example `Azure DevOps repos and PRs, org contoso`), so agents know which organization the tools
reach: the organization is fixed in every URL, and no tool takes it as a parameter. utcp allows hints of up to 60
characters; when your organization's name is too long for every hint to fit (more than 10 characters), all eight
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

`get_form_layout` and `get_system_controls` read an inherited process; Microsoft's built-in processes answer 400
("work item types are locked"). The test picks the first inherited process, and when the organization has none the
two tools stay not exercised.

### Large answers

Some reads answer with several MB (all project tags, all work item types). utcp saves a JSON answer over 10 MB to a
file and prints a reference instead of the JSON; `read-test` reads it from there and deletes it. Tools that answer
with a file (logs, zips, attachments) are saved to a temporary file and deleted once counted; nothing lands in your
Downloads folder.

## 6. Troubleshooting

| You see | Cause | Do |
| :--- | :--- | :--- |
| `'<name>' is not a valid Azure DevOps organization name` | `--org` must be the name in `https://dev.azure.com/<org>`: letters, digits and `-`. | Use only the name, not the URL. |
| `the archive's rate limit (120 requests per hour) is used up; try again after HH:MM` | A first `generate` downloads 46 files; several cold runs in one hour can use up the archive's limit. | Wait until the time it names. Files already downloaded stay cached. |
| `cannot reach the archive (...)` | No network path to `archive.softwareheritage.org`. | Check the network, proxy and CA (section 8). |
| `<file>: the archive sent content with hash ..., pinned ...` | The download is not the pinned file (a proxy changed it, or the archive is broken). | Retry; if it repeats, report it. Nothing was cached. |
| `utcp stage <manual> failed (output above; utcp 0.2.0 or newer is needed)` | utcp is older than 0.2.0, which added staging, or utcp's own message above says why. | Upgrade utcp, then run `generate --stage` again. |
| `no utcp binary (use --utcp)` | utcp is not on `PATH`. | Put it on `PATH`, or pass `--utcp <path>`. |
| `utcp has neither a staged nor an active manual named ...` (`read-test`) | The manuals were generated but never staged or registered. | `azdo-manuals generate --org contoso --stage`. |
| `utcp doctor` shows `[MISSING] AZURE_DEVOPS_EXT_PAT` | utcp has no value for the secret. | Section 2, then `utcp daemon reload-env`. |
| Every call answers 401 | The PAT is wrong, expired or revoked, or belongs to another organization. | Make a new PAT (section 2). |
| Some calls answer 401, others work | The PAT lacks the permission those tools need. | Compare the PAT with `scopes.md`. |
| A call prints an HTML page (a sign-in page) instead of JSON | Azure DevOps did not receive usable credentials. | Check `utcp doctor` and the secret's value; reload the daemon. |
| 403 | Your account may not do that (an admin-only read or write). | Expected for those tools; the PAT cannot grant more. |
| 404 on a tool that should work | A disabled feature, a classic pipeline (no YAML), or an id from another project. | See "Failures that are expected" in section 5. |
| A write answers 400 with a field error | The body differs from what the service accepts. Writes marked `(untested)` were never checked live. | Read the tool's schema (`utcp schema <manual>.<tool>`) and the service's message; report it if the schema is wrong. |

## 7. Windows

- Put `azdo-manuals.exe` in a folder on `PATH`; next to `utcp.exe` (`%LOCALAPPDATA%\Programs\utcp`, where utcp's
  installer puts it) is simplest.
- Folders are under `%LOCALAPPDATA%` (section 1). `%LOCALAPPDATA%` is Command Prompt syntax; in PowerShell write
  `$env:LOCALAPPDATA`.
- utcp's installer starts its daemon at logon through the scheduled task "utcp daemon". After changing the PAT, run
  `utcp daemon reload-env` as on Linux.

## 8. Proxies and TLS inspection

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
| `dev.azure.com` and its service hosts: `vssps`, `vsrm`, `vstmr`, `vsaex`, `feeds`, `pkgs`, `artifacts`, `almsearch`, `auditservice` and `extmgmt` `.dev.azure.com` | utcp, for the tools; `read-test` through utcp. |
| `status.dev.azure.com` | utcp, for `azdo_core.get_service_status` only (public, sent no PAT). |
