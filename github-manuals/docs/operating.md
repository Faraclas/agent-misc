# github-manuals: operating guide

For people running `github-manuals`: the token, first setup, upgrades and troubleshooting. How the manuals are built
is in the [specification](./github-manuals-spec.md).

## First setup

1. `github-manuals generate --stage` downloads GitHub's description once (14 MB, checked against the pinned hash,
   cached in `~/.cache/github-manuals`, Windows `%LOCALAPPDATA%\github-manuals\cache`), writes the manuals and
   `report.json`, and stages the manuals in utcp.
2. Make the token available to utcp as `GITHUB_MANUALS_PAT`: either a line `GITHUB_MANUALS_PAT=<token>` in utcp's
   `.env` file, or the store a "Secrets Command" reads from (add the name to its allowlist if it has one). Then
   `utcp daemon reload-env` and `utcp doctor` (it should show `[SET] GITHUB_MANUALS_PAT`).
3. Try the staged manuals: `utcp diff 'github_*'`, `utcp schema --staged github_repos.get_repo`,
   `utcp call --staged github_users.get_authenticated_user`, and check that agents find them:
   `github-manuals search-test --staged` (see [Checking search](#checking-search)).
4. Promote them: `utcp promote 'github_*'`. The pattern leaves manuals that others staged alone.

## The token

See the README's [The token](../README.md#the-token) for classic versus fine-grained tokens and the scopes. Signs
that the token lacks something:

- **401** on every call: the token is missing, expired or mistyped. `utcp doctor` shows whether it is set.
- **403** with "Resource not accessible by personal access token": a fine-grained token without that permission, or
  an operation that fine-grained tokens cannot call.
- **404** on a private repository you can see in the browser: the token has no access to it (classic: no `repo`
  scope; fine-grained: the repository is not selected). GitHub answers 404, not 403, to hide private repositories.
- **403** with "SAML" in the message: the organization enforces single sign-on; authorize the token for it on
  GitHub's token page.

## Using the tools

- **Pagination:** list tools take `per_page` (up to 100). When more items exist, utcp prints
  `{"result": <the answer>, "next_page": {...}}`; call again with the same arguments plus the `next_page` values
  (`{"page": "2"}`, a cursor such as `{"after": "..."}`, or `{"since": "..."}` for `list_users` and
  `list_public_repos`). The last page prints the plain answer. `--where`, `--fields` and `--out` apply to `result`. Search
  stops at 1000 results: the tenth page of 100 has no `next_page`, and asking for page 11 is refused (422).
- **Large lists:** narrow them with GitHub's own filters first, so fewer pages come back:
  `list_workflow_runs_for_repo` takes `status` (also a conclusion such as `failure`), `branch`, `event` and `created`;
  `list_jobs_for_workflow_run` and `list_check_runs_for_ref` take `filter: "latest"` (only the last attempt), and
  check runs also `check_name` and `status`. `--fields` then keeps only the fields you need.
- **Filtering what GitHub cannot (`--where`):** utcp keeps only the list items that match. For example:
  - failed jobs of a run: `list_jobs_for_workflow_run` with `--where 'conclusion=failure|cancelled'`;
  - one job's failed steps: `get_job_for_workflow_run` with `--where steps.conclusion=failure`;
  - check runs: `list_check_runs_for_ref` with `--where 'check_runs.conclusion!=success'`;
  - files by path: `compare_commits` with `--where 'files.filename^=src/'`, or `get_git_tree` with
    `"recursive": "1"` and `--where 'path^=src/' --where type=blob`;
  - large changes: `list_pull_request_files` with `--where 'changes>=200'`;
  - issues without pull requests (GitHub's issue lists include them): `list_issues_for_repo` with
    `--where pull_request=null`, or `pull_request!=null` for the pull requests only;
  - by label: `--where labels.name=bug` keeps the items with any label of that name;
  - by time: `--where 'completed_at>=2026-11-24'` (a date alone is a whole day, in UTC).

  Matrix job names need no escaping (`'name=check-latest (ubuntu-latest, 3.13)'`). Values ignore case; field names
  do not (`conclusion`, not `CONCLUSION`). `--where` filters only the page that came back, so a page can be empty
  and still have `next_page`; `total_count` stays GitHub's count of the unfiltered list. When nothing matches, utcp
  prints a note with the values the field has: check the value before concluding there are none.
- **Files:** log, archive and artifact downloads (`download_job_logs_for_workflow_run`, `download_zipball_archive`,
  `download_artifact`, `download_release_asset`, `download_codeql_database`, ...) save a file and print its path; the
  archives without `ref` are of the default branch. `get_content` gives a file base64-encoded inside JSON;
  `get_raw_content` prints a text or JSON file as it is and saves a binary one; give a binary file its name with
  `--out logo.png`, because GitHub sends none and utcp would save it as `get_raw_content.json`. `upload_release_asset`
  takes the file as `"body": {"file": "<path>"}` and the asset name as `name`.
- **Yes/no checks** (`check_repo_is_starred_by_authenticated_user`, `check_collaborator`,
  `check_pull_request_merged`, ...): GitHub answers yes with no content, which utcp prints as `null`, and no with
  404, which utcp reports as a failed call. A 404 there is the answer, not a mistake.
- **Paths with slashes:** `path` (file contents) and `ref` (`heads/main`) may contain `/`; utcp encodes it as `%2F`,
  which GitHub accepts.
- **Request bodies:** body fields are ordinary arguments. A few operations take their body whole as `body`
  (`add_issue_labels` with `{"labels": [...]}`, `create_attestation`, and every DELETE with a body, such as
  `remove_issue_assignees` with `{"assignees": [...]}`); their schema shows it. `render_markdown_raw` takes the
  Markdown text itself as `body`.
- **Rate limits:** `github_users.get_rate_limit` shows what is left. utcp reports a rate-limited answer as an error.

## Upgrading

After installing a new github-manuals: `github-manuals generate --stage`, then `utcp diff 'github_*'` (renamed or
removed tools are listed there and in the release notes), `github-manuals search-test --staged`, try a few tools, and
`utcp promote 'github_*'`. A new release may pin a newer commit of the description; the first run downloads it once.

## Checking search

Agents find tools with `utcp search "<words>"`, so a tool they cannot find is as good as missing.
`github-manuals search-test` runs a list of searches an agent would type (built in; source `tests/search-checks.txt`)
through your utcp and checks that one of the right tools is in the top 3 (`--top N` to change it).

- **When:** after `generate --stage`, with `--staged` (it then searches as if the staged manuals were promoted), and
  before `utcp promote 'github_*'`. Without `--staged` it checks the active manuals. It only reads (`utcp list`,
  `utcp search`) and never stages, promotes or changes anything. It needs the real manuals, so it is run by hand, not
  in CI.
- **Either family counts:** a case may list GitHub and Azure DevOps tools (`azdo_*`, from azdo-manuals); tools of
  manuals you have not registered are ignored, and a case with none registered is `SKIP`.
- **Reading it:** one line per case (query, rank or `-` when not in the results, the tool, `PASS`, `FAIL`, `KNOWN` or
  `SKIP`; a miss also shows what came first), then the top 1, top 3 and top 5 counts and the failures. It exits with
  1 when a case is `FAIL`.
- **A `FAIL` means one of two things:**
  - The manuals do not say what people call the thing ("open an issue" when the summary says "Create an issue"). Fix
    the description in `src/config.rs`: `SUMMARIES` replaces GitHub's summary (the first sentence, which search
    weighs most), `NOTES` adds sentences such as the words people use. Use the words people really use; never add
    odd wording just to make a case pass.
  - utcp ranks badly although the words are there (an exact name or summary match below unrelated tools). That is a
    utcp ranking bug: report it to utcp-rs with the query and the ranks, rather than working around it here.
- **Known misses:** a case that cannot pass yet (waiting for a utcp fix) can end with `[known miss: <reason>]`; it is
  reported as `KNOWN` and does not fail the run. Remove the marker once it passes (the line then says so).

## Troubleshooting

- **"does not match the pinned file"**: the download was changed on the way (a proxy) or GitHub's file at that commit
  changed. Nothing was used or cached. Retry; if it persists, report it.
- **"category ... has no manual"**: only with a pins file other than the built-in one. GitHub added an area that has
  not been reviewed; use the built-in pins.
- **"utcp 0.2.4 or newer is needed"**: `generate --stage` and `search-test` check `utcp --version` first, because
  older utcp versions drop the body of a DELETE, cannot send a text body, silently return only the first page of a
  list, cannot take the `'github_*'` pattern, and cannot search or list the staged manuals.
  Upgrade utcp, or use `--utcp` to point at a newer one.
- **Proxies:** set `HTTPS_PROXY` (both github-manuals and utcp read it). Trusted CAs come from the system store, so a
  company root CA for TLS inspection works.
