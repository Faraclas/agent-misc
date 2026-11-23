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
3. Try the staged manuals: `utcp diff`, `utcp schema --staged github_repos.get_repo`,
   `utcp call --staged github_users.get_authenticated_user`.
4. Promote them by name: `utcp promote github_repos` (and the others), or `utcp promote` when nothing else is staged.

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

- **Pagination:** list tools take `per_page` (up to 100) and `page`. GitHub's `Link` header is not shown by utcp; ask
  for the next page until a page has fewer items than `per_page`.
- **Files:** log, archive and artifact downloads (`download_job_logs_for_workflow_run`, `download_zipball_archive`,
  `download_artifact`, ...) save a file and print its path. `upload_release_asset` takes the file as
  `"body": {"file": "<path>"}` and the asset name as `name`.
- **Paths with slashes:** `path` (file contents) and `ref` (`heads/main`) may contain `/`; utcp encodes it as `%2F`,
  which GitHub accepts.
- **Request bodies:** body fields are ordinary arguments. A few operations take their body whole as `body`
  (`add_issue_labels` with `{"labels": [...]}`, `create_attestation`, and every DELETE with a body, such as
  `remove_issue_assignees` with `{"assignees": [...]}`); their schema shows it. `render_markdown_raw` takes the
  Markdown text itself as `body`.
- **Rate limits:** `github_users.get_rate_limit` shows what is left. utcp reports a rate-limited answer as an error.

## Upgrading

After installing a new github-manuals: `github-manuals generate --stage`, then `utcp diff` (renamed or removed tools
are listed there and in the release notes), try a few tools, and promote. A new release may pin a newer commit of the
description; the first run downloads it once.

## Troubleshooting

- **"does not match the pinned file"**: the download was changed on the way (a proxy) or GitHub's file at that commit
  changed. Nothing was used or cached. Retry; if it persists, report it.
- **"category ... has no manual"**: only with a pins file other than the built-in one. GitHub added an area that has
  not been reviewed; use the built-in pins.
- **"utcp 0.2.2 or newer is needed"**: `generate --stage` checks `utcp --version` first, because older utcp
  versions drop the body of a DELETE and cannot send a text body. Upgrade utcp, or use `--utcp` to point at a newer
  one.
- **Proxies:** set `HTTPS_PROXY` (both github-manuals and utcp read it). Trusted CAs come from the system store, so a
  company root CA for TLS inspection works.
