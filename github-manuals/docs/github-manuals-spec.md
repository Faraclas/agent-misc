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
- **Arguments:** path parameters (required, except those in `Config::optional_path`, which utcp 0.2.4 drops from
  the URL when left out, with a note on the argument: `get_content` without `path` lists the repository root, the
  archives without `ref` are of the default branch), query parameters (required when the description says so), then
  the request body. The description has no header or cookie parameters; one would be an error.
- **Argument descriptions:** every argument has one. A path parameter whose description is only GitHub's
  placeholder ("path parameter") gets the one in `Config::param_descriptions`; any other undescribed argument gets
  the one in `Config::input_descriptions`, keyed by operationId and argument. An undescribed argument without an
  entry fails `generate`, and so does an entry for an argument GitHub now describes or the tool lacks, so the
  config cannot go stale.
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
- **Variants:** `Config::variants` adds a second tool for an operation that asks for another `Accept` media type,
  with the operation's inputs and its own name, description and output: `get_raw_content` (a file as it is, not
  base64 in JSON), `download_release_asset` and `download_codeql_database` (`"response_type": "file"`). The base
  tool's description points to its variant (`Config::notes`).
- **Schemas:** `$ref` and `allOf` are inlined down to five levels, a cycle stops at its first repeat (type and
  description only); `oneOf`/`anyOf`, `enum`, `default` and known formats are kept; read-only properties, examples
  and titles are dropped. A `oneOf`/`anyOf` without a description takes the sentences its branches share. An array
  query parameter takes one value: GitHub reads a list as repeated parameters, utcp sends it comma-separated, so
  GitHub's "Accepts one or more" sentence goes and "Takes one value." is added.
- **Outputs:** only success answers count (2xx; a redirect is followed by utcp, so it is never the answer).
  Response schemas are not copied: agents see the answer itself, and copying them would double the manuals' size.
  - `object`, `array` (also for a schema with only `items`) or `string` (non-JSON answers such as `get_zen`).
  - The description is GitHub's response description, unless it only names the outcome or the status ("OK",
    "Response if merge was successful"), restates the summary, or differs between several success answers (utcp
    does not show which one came).
  - An answer with content next to one without (a 204 next to a 200) adds when it is null, from GitHub's text
    ("Response if repository is empty" is "Null if the repository is empty."), or "Can also be null (an answer with
    no content)."
  - `null` without any content: a GET is a yes/no check ("Null means yes; no is a failed call with 404 Not Found.",
    and the tool description says it too unless it already mentions 404); other methods get "No content: the call
    answers null when it worked."
  - File answers get utcp's file reference (`type`, `path`, `mime_type`, `bytes`, `filename`).
- **Pointers to other endpoints** (`links.rs`, before anything else): GitHub points to other operations by their
  documentation page (`[Get a commit](/rest/commits/commits#get-a-commit)`) or by method and path (`` `GET
  /repos/{owner}/{repo}/commits` ``). Every page anchor and route is matched to its operation (placeholder names
  ignored, `:ref` included); a match that is a tool names it (`github_repos.get_commit`), and "the X endpoint"
  around it goes. A page title used as a name becomes the tool; one inside a sentence, or link text that says what
  to do ("set the repository's subscription manually"), keeps its words and adds the tool in parentheses. A
  pointer to a left-out operation or to a guide keeps only its text; an `/orgs/` alternative ("and `GET
  /orgs/{org}/...` for organizations") is dropped when organization administration is left out. `generate` fails
  when a description names a tool that does not exist.
- **Descriptions:** summary (GitHub's, or `Config::summaries`' replacement where GitHub's misses the words people
  use, such as "Get the contents of a file or directory in a repository" for "Get repository content"; with
  `Config::summary_qualifiers` for its category, such as "for Dependabot", when another kept operation has the same
  summary, so every first sentence differs), then the description up to GitHub's fine-grained-token section or its
  list of custom media types (the fixed `Accept` header does not ask for those), then `Config::notes` (such as the
  words people use for "the authenticated user": who am I, the current user, me). As plain text: links become their
  text (or a tool, above), images their alt text, lists, tables, headings and code blocks paragraphs (a short list
  after a colon joins its line), and alert markers, "Note:", HTML tags and Markdown emphasis go; a search's URL query
  in a code span becomes the JSON arguments the tool takes. `Config::sentence_fixes` then replace GitHub's text where
  it is wrong or names pages; a fix whose text GitHub no longer has fails `generate`, and so does a replacement
  summary for an operation the description lacks or one equal to GitHub's own. Tool names never come from summaries,
  so a replacement renames nothing. Then, sentence by sentence (a paragraph always ends one; a split never follows
  `e.g.`, `i.e.`, `etc.` or `vs.`, and before a lowercase word only a word of three letters or more ends one):
  - Dropped: pointers to GitHub's docs ("For more information", "See ..."), since agents cannot follow them; HTTP
    details utcp takes care of (headers, media types, redirects, upload URLs, client advice); which plans have a
    feature, and what holds only on GitHub Enterprise Server; a sentence equal to the one before; a first sentence
    that restates the summary and route; a commit's signature-verification tables (one sentence instead).
  - Rewritten: the Link header is `next_page`, "Gets a redirect URL to download" is "Downloads", "this endpoint"
    and "the endpoint" are the tool, and "OAuth app tokens and personal access tokens (classic) need the `repo`
    scope to use this endpoint" is "Classic tokens need the `repo` scope", in place, because later sentences may
    refer back to it.
  - A sentence ending with `:` ("For example:") is kept or dropped together with the next one.
  The result keeps whole sentences up to 1,200 characters (argument descriptions: 600); classic-token scopes, notes,
  the search limit and the yes/no answer are always kept. Only a first sentence that alone is too long is cut at a
  word. Argument descriptions get the same treatment without the summary. The summary always ends with a period.
- **Cautions:** a DELETE whose summary deletes, removes or disables something gets "Delete only what the user asked
  for or what you created." (or "Remove ... what you added", "Disable ... what you enabled") right after the
  summary, not before it, so the first sentence (all that `utcp search` and `utcp list` show) still says what
  goes. One that undoes the user's own action (unstar, unlock) gets none. Every search ends with GitHub's limit of
  1,000 results over all pages. Untested writes start with `(untested)`.
- **Tags:** `github`, the category, the subcategory, and `untested` for untested writes.
- **Tested writes:** `config::TESTED_WRITES` lists, by operationId, the writes that worked live through
  `utcp call --staged` against a throwaway public repository of the token's owner (117 of 222). A name it holds that
  the description lacks fails `generate`, so a new pin shows which tested operations GitHub dropped.

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
  operationId, method and path; a variant has its operation's).

## Search check (`search-test`)

- **Cases:** `tests/search-checks.txt`, embedded at build time (`include_str!`), so the installed binary carries
  them; `--cases <file>` reads another. One case per line, `<query> -> <manual>.<tool> | <manual>.<tool>`, an
  optional trailing `[known miss: <reason>]`; `#` starts a comment (anywhere on a line), blank lines are ignored. A
  line without `->`, with an empty query, a name that is not `<manual>.<tool>`, or any other `[...]` is an error
  naming the line. The file holds only queries and tool names, so it is committed.
- **Run:** the utcp from `--utcp` or PATH, with the same minimum version as `--stage` (0.2.4 added `search --staged`
  and `list --staged`). Registered manuals: the rows of `utcp list --json` that are not staged, plus, with
  `--staged`, the rows of `utcp list --staged --json`. Each case with a listed tool of a registered manual runs
  `utcp search --json --limit <max(10, N)> [--staged] -- <query>` and takes the results' `name`s in order; the others
  are skipped without searching. Only `list` and `search` are run.
- **Judging** (`search_test::evaluate`, pure, so the tests use canned results): tools of unregistered manuals are
  ignored; the rank is the best 1-based position of a remaining tool. `PASS` when it is at most N (default 3), else
  `KNOWN` for a known miss and `FAIL` otherwise; `SKIP` when no listed manual is registered. Totals count the searched
  cases in the top 1, 3 and 5, the failures and the known misses. Exit 1 when there is a failure; 2 for a bad cases
  file or a missing or too old utcp.
- **Not in CI:** the real manuals are build output and the fixtures are a hand-made sample, so the check is run by
  hand after `generate --stage`. The tests cover the parsing and judging offline, and, with `UTCP_BIN`, a run
  against the expected fixture manuals staged in a throwaway utcp.

## Decisions

- **Classic token, the token decides.** Fine-grained tokens cannot write to public repositories outside your
  account, which upstream issue and pull request work needs. The manuals therefore carry everything safe to offer,
  and the dangerous operations are left out by rule (a speed bump against accidents, not a security boundary).
- **The API version is pinned** (`2026-03-10`, the newest), so a GitHub default change does not change behavior.
- **Seven manuals** instead of one: a single manual of 608 tools is too large for search and for agents.
- **Not covered yet:** organization administration (`/orgs/...`), fine-grained token permissions per tool, live
  tests of the 105 untested writes, and the other header-selected answer formats (diffs and patches of commits and
  pull requests).
