# github-manuals: specification

How the manuals are built from GitHub's OpenAPI description, and the decisions behind it. Users start with the
[README](../README.md) and the [operating guide](./operating.md).

## Source

- `github/rest-api-description` publishes one OpenAPI 3.0 file per REST API version under
  `descriptions/api.github.com/`. Its tagged releases stopped in 2022, so `pins.json` pins a commit, the file, its
  SHA-256 and length. `github-manuals pins --commit <sha> [--api-version <v>]` writes new pins (one request to
  `api.github.com` for the commit date, one download).
- The file is downloaded from `raw.githubusercontent.com/<repo>/<commit>/<file>` with a `github-manuals/<version>`
  User-Agent, verified, and cached as `<sha256>.json`. A cached file is verified again before use.

## Selection

For each operation (`paths` x method), in order:

1. HEAD and OPTIONS: left out (none exist today).
2. `deprecated: true` or `x-github.deprecationDate`: left out.
3. The first matching rule of `Config::rules` (method regex, path regex, optional `x-github` category, exceptions):
   left out with the rule's reason. The rules and their reasons are the README's "What is left out" table.
4. `--read-only`: every non-GET is left out.

Everything else becomes a tool in the manual of its `x-github` category, or of its (category, subcategory) when
`Config::subcategories` moves it. A category without a manual is an error.

## Tools

- **URL:** the operation's own server (`uploads.github.com` for release assets) or the description's
  (`https://api.github.com`), plus the path with its `{placeholders}`. utcp fills and percent-encodes them.
- **Headers:** `Accept: application/vnd.github+json` and `X-GitHub-Api-Version: <API version>` on every tool. Auth:
  `{"auth_type": "api_key", "var_name": "Authorization", "api_key": "Bearer ${GITHUB_MANUALS_PAT}", "location":
  "header"}`. GitHub's description declares no security scheme, so this is added by the generator.
- **Arguments:** path parameters (required), query parameters (required when the description says so), then the
  request body. The description has no header or cookie parameters; one would be an error.
- **Bodies are flat** when the JSON body is an object with properties, the operation has no query parameters, and no
  body field shares a name with a parameter: the fields become arguments, and utcp sends the leftover arguments of a
  POST, PUT or PATCH as the JSON body. This matches GitHub's documentation and the earlier hand-written manual, and
  GitHub's own `body` field (issue and comment text) stays an ordinary argument. Otherwise the whole body is one
  argument `body` (`"body_field": "body"`): bodies that are `oneOf` an object or an array (add or set labels), and
  bodies next to query parameters (utcp sends leftover arguments as query parameters only when a body field is set).
  Required body fields are required arguments only when the body itself is required.
- **DELETE bodies** are always the one argument `body` (`"body_field": "body"`): utcp sends a DELETE's other
  arguments as query parameters, so a flat body cannot work. `body` is required when the body is (delete a file).
- **Text bodies:** a `text/plain` body (render Markdown in raw mode) is `"body_type": "text"` with a string
  argument `body`, sent as is with `Content-Type: text/plain`.
- A `body` argument without a description of its own gets "The request body."
- **Uploads:** an `application/octet-stream` body is `"body_type": "file"`; the argument `body` is `{"file": path}`.
- **File answers:** an operation whose success answers have no content and include a 302 redirect (logs, archives,
  artifacts, SBOM reports) gets `"response_type": "file"`; utcp follows the redirect and saves the file.
- **Schemas:** `$ref` and `allOf` are inlined down to three levels, a cycle stops at its first repeat (type and
  description only); `oneOf`/`anyOf`, `enum`, `default` and known formats are kept; read-only properties, examples
  and titles are dropped. An array query parameter takes one value (utcp would send an array as one JSON string).
- **Outputs:** `object`, `array` or `string` (non-JSON answers such as `get_zen`), with the response description;
  `null` for answers without content; the file description for file answers. Response schemas are not copied: agents
  see the answer itself, and copying them would double the manuals' size.
- **Descriptions:** summary, then the description up to GitHub's fine-grained-token section or its list of custom
  media types (the fixed `Accept` header does not ask for those), as plain text (links become their text, alert
  markers, HTML tags and Markdown emphasis go), cut to about 400 characters. DELETEs start with
  "Delete only what the user asked for or what you created."; untested writes with `(untested)`.
- **Tags:** `github`, the category, the subcategory, and `untested` for untested writes.

## Names (scheme C)

1. `Config::names` (operationId -> name) wins.
2. `search/<x>` is `search_<x>`.
3. Otherwise the operationId after the `/`, in snake_case. When the words after the verb are only qualifiers (`for`,
   `by`, `authenticated`, `all`, ...) and owners (`repo`, `user`, `org`), the area noun goes after the verb
   (`repos/get` -> `get_repo`, `repos/list-for-org` -> `list_repos_for_org`, `users/get-authenticated` ->
   `get_authenticated_user`).
4. Areas in `Config::tag_nouns` get their noun after the verb when none of their marker words is in the name
   (`pulls/list-files` -> `list_pull_request_files`). A `list` whose noun is followed by nothing or a qualifier takes
   the plural (`reactions/list-for-issue` -> `list_reactions_for_issue`).
5. Two tools of one manual with the same name: the tools of the manual's home area (`repos` in `github_repos`) keep
   it, the others get their area after the verb (`git/get-commit` -> `get_git_commit`). A clash that remains, or one
   between overrides, fails the generator.

`list_issues` is deliberately no tool's name: the hand-written manual used it for a repository's issues, so
`issues/list` (issues assigned to you) is `list_assigned_issues` and the repository's issues are
`list_issues_for_repo`.

## Output files

- `<manual>.utcp.json`: `manual_version` and `utcp_version` 1.0.0, `info` (title, API version, a description naming
  the commit and the token variable), and the tools sorted by name.
- `report.json`: `api_version`, `source` (repository, commit, date, file, SHA-256), per manual (title, hint, tools,
  reads, writes, untested writes, bytes, approximate tokens), `left_out_summary` (count per reason), `left_out`
  (every operation with method, path, category, subcategory and reason), and `tools` (every tool with its manual,
  operationId, method and path).

## Decisions

- **Classic token, the token decides.** Fine-grained tokens cannot write to public repositories outside your
  account, which upstream issue and pull request work needs. The manuals therefore carry everything safe to offer,
  and the dangerous operations are left out by rule (a speed bump against accidents, not a security boundary).
- **The API version is pinned** (`2026-03-10`, the newest), so a GitHub default change does not change behavior.
- **Seven manuals** instead of one: a single manual of 605 tools is too large for search and for agents.
- **Not covered yet:** organization administration (`/orgs/...`), fine-grained token links, live read and write
  tests, header-selected answer formats (diffs and patches of commits and pull requests, raw file contents).
