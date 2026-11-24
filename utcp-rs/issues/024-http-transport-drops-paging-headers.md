# Issue 024: HTTP Transport Drops Paging Headers, So Long Lists Silently Stop at Page One

- **Upstream Crate:** `rs-utcp` `v0.3.2`
- **Upstream Files:** `src/transports/http/mod.rs`
- **Local Status:** Fixed in `utcp-rs/crates/utcp-core/src/transports/http/mod.rs` (`utcp-rs` 0.2.3)
- **Upstream Issue / PR:** Not filed yet

Found while writing an agent skill for Azure DevOps: `utcp call azdo_core.get_projects '{"$top": 2}'` printed
`{"count": 2, "value": [...]}` with nothing saying that the organization has 24 projects.

## Problem

The HTTP transport returns only the response body for a JSON or text answer and discards every response header. Many
APIs say that a list has more pages only in a header:

- **Azure DevOps** sends `x-ms-continuationtoken: <token>`; the next call passes it as `continuationToken`.
- **GitHub** (and other APIs following RFC 8288) sends `Link: <https://...?page=2>; rel="next", <...>; rel="last"`.

Without the header, the answer of the first page looks complete. An agent that lists builds, pull requests, issues or
repositories works on part of the list and does not know it, and nothing in the answer tells it to ask for more.
Upstream `rs-utcp` 0.3.2 has the same behavior (`call_tool` keeps only the parsed body).

## Local Fix in `utcp-rs`

- `call_tool_files()` reads the paging headers of a JSON or text answer (`next_page_args()`) and returns
  `CallOutput::Page { value, next_page }` instead of `CallOutput::Json` when there is a next page:
  - `x-ms-continuationtoken` becomes `{"continuationToken": <token>}`, or `{"x-ms-continuationtoken": <token>}` when
    the template's `header_fields` declares that header (case-insensitive). An empty token means no next page.
  - `Link` with `rel="next"` (`link_next()`: any order of links and parameters, quoted or bare `rel`, several
    relation types in one `rel`) becomes the query pairs of the next URL that differ from the ones this call sent (the
    template URL's own query plus the query arguments), as strings. A relative URL is resolved against the request
    URL first.
- `utcp call` prints a page as `{"result": <answer>, "next_page": {<args>}}`; the last page prints the answer alone,
  so answers that never page are unchanged. `--fields` and `--out` apply to `result`. A `batch` item gets `next_page`
  next to its `result` (`BatchResultItem::succeeded()`).
- The daemon sends a page to the client in the same shape; the client splits it (`transfer::split_page()`) to save
  `result` for `--out` and wraps it again.
- File answers ignore paging headers.

APIs that page in the body (`nextLink`, `@odata.nextLink`) already show it in their answer and are unchanged.

## Known Limit

A daemon client treats an API answer that is exactly `{"result": ..., "next_page": {...}}` (two keys, `next_page` an
object) as a page. No API is known to answer that way.

## Tests

- `crates/utcp-core/src/transports/http/mod.rs`: `link_headers_name_their_next_target`,
  `next_page_arguments_come_from_paging_headers`, `a_paged_answer_carries_the_next_page_arguments`.
- `crates/utcp-core/src/transfer.rs`: `pages_split_back_into_answer_and_next_page_arguments`.
- `crates/utcp-cli/tests/integration_dual_mode.rs`: `paged_answers_name_the_arguments_for_the_next_page` (a mock with
  `Link`, `x-ms-continuationtoken` as a query argument and as a header; `--fields`, `--out` and `batch`; through the
  daemon and in direct mode).

Checked live: `azdo_core.get_projects` with `$top` 2 answered `next_page: {"continuationToken": "2"}`, and the next
call returned the next two projects; GitHub's `repos/list-for-authenticated-user` with `per_page` 2 answered
`next_page: {"page": "2"}`, then `{"page": "3"}`.
