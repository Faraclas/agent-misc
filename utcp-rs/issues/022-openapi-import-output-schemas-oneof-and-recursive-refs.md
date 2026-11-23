# Issue 022: OpenAPI Import Expands Every Response Schema, Types `oneOf` as `object`, and Overflows on Recursive Schemas

- **Upstream Crate:** `rs-utcp` `v0.3.2`
- **Upstream Files:** `src/openapi/mod.rs`
- **Local Status:** Fixed in `utcp-rs/crates/utcp-core/src/openapi/mod.rs` (`utcp-rs` 0.2.1)
- **Upstream Issue / PR:** Not filed yet

Found by importing GitHub's published REST description (`github/rest-api-description`,
`descriptions/api.github.com/api.github.com.json`, OpenAPI 3.0.3, 1,281 operations, 14 MB) with `utcp import`.

## Problems

1. **Every response schema is expanded in full.** `build_schema_from_value()` runs `resolve_schema()` on the `200` /
   `201` response, which inlines every `$ref` all the way down. GitHub's event and repository schemas nest deeply, so
   one tool (`issues_list-events-for-timeline`) carried a 482 KB output schema and the manual was 27 MB, 87% of it
   output schemas. Nothing reads output schemas to make a call; they only reach agents through `utcp schema` and the
   TUI preview, where hundreds of KB per tool floods the agent's context.
2. **`oneOf` / `anyOf` / `allOf` parameters are typed `object`.** `extract_inputs()` adds `"type": "object"` to every
   parameter and request body without a `type`. A `oneOf` of an integer and a string (GitHub's `workflow_id`, "the ID
   or the workflow file name") became `{"type": "object", "oneOf": [...]}`, which tells an agent to send an object. 39
   GitHub parameters were affected.
3. **A schema that refers to itself overflows the stack.** `resolve_schema()` follows `$ref`s with no record of where it
   is, so a tree node holding a list of nodes (`node.children.items: {"$ref": "#/components/schemas/node"}`) in a
   parameter, body or response recurses until `thread 'main' has overflowed its stack` aborts `import`, `register`
   and `refresh`. GitHub's description happens to have no such schema on an operation; many APIs do.

## Local Fix in `utcp-rs`

1. **Outputs are outlined one level deep.** The response's own `type`, `description`, `title`, `required`, `enum` and
   `format`, plus for each field (of the object, or of each item of a list) its `type`, `format` and item type.
   Nested fields are not expanded. `$ref`s are followed only as far as the outline needs (`deref_shallow()`), `allOf`
   parts are merged one level deep, and a `oneOf` / `anyOf` field gets the distinct types of its variants (for
   example `["null", "object"]`). The outline still shows which fields `--fields` can pick.
2. **`type: object` is a default only when nothing else describes the value:** no `type`, `oneOf`, `anyOf`, `allOf`,
   `enum`, `const` or `not` (`needs_default_type()`).
3. **`resolve_schema()` keeps the `$ref`s it is expanding on the current path** and replaces a reference met again
   inside its own expansion with a `description` saying "Recursive: the same schema as (name) above, not expanded
   again." `deref_shallow()` stops after 32 hops for a loop of references to references.

Inputs are otherwise unchanged: agents need them to build calls.

## Effect on GitHub's Description

| | Before | After |
| :--- | ---: | ---: |
| Manual size | 27.3 MB | 3.8 MB |
| Output schemas, all tools | 11.6 MB | 626 KB |
| Largest output schema | 482 KB | 5.5 KB |
| Largest tool (now its inputs) | 483 KB | 42 KB |
| Median tool | 2.1 KB | 1.3 KB |
| `oneOf` parameters typed `object` without the spec saying so | 39 | 0 |
| `import` time (release build) | 0.35 s | 0.13 s |

The 7 bodies still typed `object` with a `oneOf` are typed `object` by the spec itself (the `oneOf` lists alternative
required fields).

## Tests

`crates/utcp-core/src/openapi/mod.rs`: `outputs_are_outlined_one_level_deep`, `one_of_parameter_is_not_typed_object`,
`self_referencing_schema_stops_instead_of_overflowing` (a spec shaped like GitHub's: shared `$ref` responses, nested
objects, lists, `allOf`, an `anyOf` with `null`, a `oneOf` path parameter and a self-referencing tree).

## Re-import After Upgrading

Manuals already cached from an OpenAPI description keep their old output schemas until `utcp refresh <name>` (or
`utcp import` again). Tool names, inputs and calls do not change, except the `oneOf` parameters that lose their wrong
`type`.
