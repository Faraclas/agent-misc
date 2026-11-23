# Issue 023: HTTP Transport Sends No Body on DELETE and Only JSON Bodies

- **Upstream Crate:** `rs-utcp` `v0.3.2`
- **Upstream Files:** `src/transports/http/mod.rs`
- **Local Status:** Fixed in `utcp-rs/crates/utcp-core/src/transports/http/mod.rs` (`utcp-rs` 0.2.2)
- **Upstream Issue / PR:** Not filed yet

Found while generating a GitHub manual from GitHub's published REST description: 5 operations could not be called,
so the generator left them out.

## Problems

1. **No body on DELETE.** Only POST, PUT and PATCH send a body (upstream sends all arguments as JSON; `utcp-rs` already
   honored `body_field` there, see 003). On DELETE every argument became a query parameter, even when the template
   sets `body_field`: a `body` argument went out as `?body={...}`. HTTP allows a body on DELETE, and GitHub requires
   one to delete a file (`DELETE /repos/{owner}/{repo}/contents/{path}`: `message`, `sha`), to remove assignees,
   a sub-issue, or requested reviewers.
2. **Only JSON bodies.** The body is always sent with `.json(...)`, so a string becomes a quoted JSON string. GitHub's
   `POST /markdown/raw` takes the Markdown as a raw `text/plain` body.
3. **`--dry-run` did not show the split.** It printed the arguments, not which went to the query and which to the body,
   so the problem above could not be seen without sending the request.

## Local Fix in `utcp-rs`

One function, `plan_request()`, decides where the arguments go; the call and `--dry-run` both use it.

1. **DELETE sends the `body_field` argument as the JSON body** when the template sets `body_field` and the call passes
   that argument; the other arguments stay query parameters, as for POST. Without `body_field`, or without that
   argument, nothing changes: all arguments are query parameters. GET and HEAD never send a body.
2. **`"body_type": "text"`** sends the `body_field` argument (a string) as is, with the template's `Content-Type`
   header or `text/plain; charset=utf-8`. It works on POST, PUT, PATCH and DELETE. A missing or non-string argument
   fails with "pass it as a string". `"body_type": "json"` is the default, and an unknown `body_type` now fails as a
   broken manual instead of silently sending JSON.
3. **`--dry-run` prints `query` and `body`** next to `args`, and puts `header_fields` arguments in `headers`.

Uploads (`"body_type": "file"`) still need POST, PUT or PATCH.

## Effect on Existing Manuals

None measured: none of the 20 installed or staged manuals on the maintainer's machine (GitHub, d2, context7, and 9
generated Azure DevOps manuals) had a DELETE tool with `body_field` (checked 2026-11-23), and the only `body_type` in
use was `"file"` (23 POST and PUT tools). An OpenAPI import sets `body_field` on any operation with a request body, so
an imported DELETE with a body now sends it, which is what the description asks for.

## Tests

`crates/utcp-core/src/transports/http/mod.rs`: `arguments_go_to_the_query_or_the_body_by_method_and_template` (every
method, with and without `body_field`, each `body_type`) and `delete_bodies_and_text_bodies_reach_the_server` (a mock
server echoes the method, query, `Content-Type` and body it received). `crates/utcp-cli/tests/integration_dual_mode.rs`:
`dry_run_shows_the_query_and_the_body_it_would_send` (direct mode and daemon).
