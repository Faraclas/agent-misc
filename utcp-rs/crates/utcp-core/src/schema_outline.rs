//! Shortening a large input schema for `utcp schema`, and reading one part of it (`--path`).
//!
//! A few tools take bodies whose schema is tens of kilobytes (an Azure DevOps build definition is 44 KB, about
//! 11k tokens), most of it nested objects an agent never fills in. `utcp schema` therefore prints a schema
//! larger than [`SCHEMA_BUDGET_CHARS`] down to the deepest level that fits, and replaces each object below it
//! with a `collapsed` line naming its fields and the `--path` that shows them.

use serde_json::{Map, Value};

/// Longest `inputs` schema, in characters of compact JSON (about 1,000 tokens), that `utcp schema` prints
/// whole. Nineteen in twenty tools fit.
pub const SCHEMA_BUDGET_CHARS: usize = 4000;

/// Most field names a `collapsed` line lists before `and N more`.
const MAX_LISTED_FIELDS: usize = 40;

const ALTERNATIVES: [&str; 3] = ["oneOf", "anyOf", "allOf"];

/// An input schema as `utcp schema` prints it.
#[derive(Debug, Clone, PartialEq)]
pub struct Outline {
    pub inputs: Value,
    /// Levels shown when parts were collapsed: 1 shows the tool's own arguments (or the fields at `--path`)
    /// and collapses the objects in them. `None` when nothing was collapsed.
    pub depth: Option<usize>,
    /// Path of the first collapsed part, for the example in the note.
    pub first_collapsed: Option<String>,
}

/// `schema` whole when its compact JSON fits in `budget` characters; otherwise shown to the deepest level
/// that fits (at least 1), with the objects below it collapsed. `base` is the path of `schema` within the
/// tool's inputs (empty for the whole inputs), so each `collapsed` line names a path `--path` accepts.
pub fn outline(schema: &Value, base: &str, budget: usize) -> Outline {
    let whole = Outline {
        inputs: schema.clone(),
        depth: None,
        first_collapsed: None,
    };
    if compact_len(schema) <= budget {
        return whole;
    }
    let deepest = max_depth(schema, 0);
    let mut chosen = None;
    for depth in 1..=deepest {
        let mut first = None;
        let shown = collapse(schema, base, 0, depth, &mut first);
        if first.is_none() {
            break;
        }
        let fits = compact_len(&shown) <= budget;
        if fits || chosen.is_none() {
            chosen = Some(Outline {
                inputs: shown,
                depth: Some(depth),
                first_collapsed: first,
            });
        }
        if !fits {
            break;
        }
    }
    chosen.unwrap_or(whole)
}

/// The part of `inputs` at `path`: dot-separated field names, passing through lists (`body.phases.steps`) and
/// alternatives (`oneOf`); `*` is a map's values. A field name that itself has dots is matched whole.
pub fn schema_at_path<'a>(inputs: &'a Value, path: &str) -> Result<&'a Value, String> {
    let segments: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return Err("--path is empty; name a field, such as --path body".to_string());
    }
    let mut node = inputs;
    let mut walked: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < segments.len() {
        let mut found = None;
        for end in (i + 1..=segments.len()).rev() {
            let key = segments[i..end].join(".");
            if let Some(child) = child(node, &key) {
                found = Some((child, end));
                break;
            }
        }
        let Some((next, end)) = found else {
            let here = if walked.is_empty() {
                "the tool's inputs".to_string()
            } else {
                format!("'{}'", walked.join("."))
            };
            let fields = field_names(node);
            let there = if fields.is_empty() {
                format!("{here} has no fields")
            } else {
                format!("fields of {here}: {}", fields.join(", "))
            };
            let names: Vec<&str> = fields.iter().map(String::as_str).collect();
            let guess = crate::arguments::similar_names(segments[i], &names, 1)
                .first()
                .map(|g| format!("; did you mean '{g}'?"))
                .unwrap_or_default();
            return Err(format!(
                "no field '{}' in {here}{guess} ({there})",
                segments[i]
            ));
        };
        walked.extend(&segments[i..end]);
        node = next;
        i = end;
    }
    Ok(node)
}

fn compact_len(v: &Value) -> usize {
    serde_json::to_string(v)
        .map(|s| s.chars().count())
        .unwrap_or(0)
}

/// The object schemas `node` stands for: itself, the items of a list (of a list ...), and each alternative.
fn shapes(node: &Value) -> Vec<&Map<String, Value>> {
    let Some(obj) = node.as_object() else {
        return Vec::new();
    };
    if let Some(items) = obj.get("items").filter(|i| i.is_object()) {
        return shapes(items);
    }
    let mut out = vec![obj];
    for key in ALTERNATIVES {
        if let Some(list) = obj.get(key).and_then(Value::as_array) {
            out.extend(list.iter().flat_map(shapes));
        }
    }
    out
}

fn child<'a>(node: &'a Value, key: &str) -> Option<&'a Value> {
    shapes(node).into_iter().find_map(|s| {
        if key == "*" {
            s.get("additionalProperties").filter(|v| v.is_object())
        } else {
            s.get("properties").and_then(|p| p.get(key))
        }
    })
}

fn field_names(node: &Value) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut is_map = false;
    for s in shapes(node) {
        if let Some(props) = s.get("properties").and_then(Value::as_object) {
            for k in props.keys() {
                if !names.contains(k) {
                    names.push(k.clone());
                }
            }
        }
        is_map |= s.get("additionalProperties").is_some_and(Value::is_object);
    }
    if is_map {
        names.push("*".to_string());
    }
    names
}

fn max_depth(node: &Value, level: usize) -> usize {
    let Some(obj) = node.as_object() else {
        return level;
    };
    if let Some(items) = obj.get("items").filter(|i| i.is_object()) {
        return max_depth(items, level);
    }
    let mut deepest = level;
    if let Some(props) = obj.get("properties").and_then(Value::as_object) {
        for v in props.values() {
            deepest = deepest.max(max_depth(v, level + 1));
        }
    }
    if let Some(extra) = obj.get("additionalProperties").filter(|v| v.is_object()) {
        deepest = deepest.max(max_depth(extra, level + 1));
    }
    for key in ALTERNATIVES {
        if let Some(list) = obj.get(key).and_then(Value::as_array) {
            for v in list {
                deepest = deepest.max(max_depth(v, level));
            }
        }
    }
    deepest
}

fn join(base: &str, key: &str) -> String {
    if base.is_empty() {
        key.to_string()
    } else {
        format!("{base}.{key}")
    }
}

/// `node` with the objects at `level >= depth` collapsed. The schema itself is level 0 and its fields are
/// level 1; list items and alternatives keep their list's level.
fn collapse(
    node: &Value,
    path: &str,
    level: usize,
    depth: usize,
    first: &mut Option<String>,
) -> Value {
    let Some(obj) = node.as_object() else {
        return node.clone();
    };
    let mut out = obj.clone();
    if let Some(items) = obj.get("items").filter(|i| i.is_object()) {
        out.insert("items".into(), collapse(items, path, level, depth, first));
        return Value::Object(out);
    }
    let mut fields = field_names(node);
    if level >= depth && level > 0 && !fields.is_empty() {
        let is_map = fields.last().is_some_and(|f| f == "*");
        if is_map {
            fields.pop();
        }
        for key in ["properties", "additionalProperties"]
            .into_iter()
            .chain(ALTERNATIVES)
        {
            if key != "additionalProperties" || obj.get(key).is_some_and(Value::is_object) {
                out.remove(key);
            }
        }
        let line = if fields.is_empty() {
            format!(
                "a map from names to objects; see one with --path {}",
                join(path, "*")
            )
        } else {
            let n = fields.len();
            let listed: Vec<&str> = fields
                .iter()
                .take(MAX_LISTED_FIELDS)
                .map(String::as_str)
                .collect();
            let more = n - listed.len();
            let noun = if n == 1 { "field" } else { "fields" };
            let tail = if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            };
            let other = if is_map { ", and any other name" } else { "" };
            format!(
                "{n} {noun}: {}{tail}{other}; see with --path {path}",
                listed.join(", ")
            )
        };
        out.insert("collapsed".into(), Value::String(line));
        first.get_or_insert_with(|| path.to_string());
        return Value::Object(out);
    }
    if let Some(props) = obj.get("properties").and_then(Value::as_object) {
        let shown: Map<String, Value> = props
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    collapse(v, &join(path, k), level + 1, depth, first),
                )
            })
            .collect();
        out.insert("properties".into(), Value::Object(shown));
    }
    if let Some(extra) = obj.get("additionalProperties").filter(|v| v.is_object()) {
        out.insert(
            "additionalProperties".into(),
            collapse(extra, &join(path, "*"), level + 1, depth, first),
        );
    }
    for key in ALTERNATIVES {
        if let Some(list) = obj.get(key).and_then(Value::as_array) {
            let shown = list
                .iter()
                .map(|v| collapse(v, path, level, depth, first))
                .collect();
            out.insert(key.into(), Value::Array(shown));
        }
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn definition() -> Value {
        json!({
            "type": "object",
            "required": ["project", "body"],
            "properties": {
                "project": {"type": "string", "description": "Project ID or name"},
                "body": {
                    "type": "object",
                    "description": "The new definition.",
                    "properties": {
                        "name": {"type": "string"},
                        "process": {
                            "type": "object",
                            "properties": {
                                "yamlFilename": {"type": "string", "description": "x".repeat(200)},
                                "type": {"type": "integer"}
                            }
                        },
                        "triggers": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "branchFilters": {"type": "array", "items": {"type": "string"}},
                                    "settings": {"type": "object", "properties": {"a": {"type": "string"}}}
                                }
                            }
                        },
                        "variables": {
                            "type": "object",
                            "additionalProperties": {
                                "type": "object",
                                "properties": {"value": {"type": "string"}, "isSecret": {"type": "boolean"}}
                            }
                        }
                    }
                }
            }
        })
    }

    #[test]
    fn a_schema_that_fits_is_printed_whole() {
        let s = definition();
        let o = outline(&s, "", 100_000);
        assert_eq!(o.inputs, s);
        assert_eq!(o.depth, None);
        assert_eq!(o.first_collapsed, None);
    }

    #[test]
    fn a_large_schema_keeps_the_deepest_level_that_fits() {
        let s = definition();
        let o = outline(&s, "", 400);
        assert_eq!(o.depth, Some(1), "{}", o.inputs);
        let body = &o.inputs["properties"]["body"];
        assert_eq!(body["description"], "The new definition.");
        assert!(body.get("properties").is_none());
        assert_eq!(
            body["collapsed"],
            "4 fields: name, process, triggers, variables; see with --path body"
        );
        assert_eq!(o.inputs["properties"]["project"]["type"], "string");
        assert_eq!(o.inputs["required"], json!(["project", "body"]));
        assert_eq!(o.first_collapsed.as_deref(), Some("body"));

        let o = outline(&s, "", 800);
        assert_eq!(o.depth, Some(2), "{}", o.inputs);
        let body = &o.inputs["properties"]["body"]["properties"];
        assert_eq!(body["name"]["type"], "string");
        assert_eq!(
            body["process"]["collapsed"],
            "2 fields: type, yamlFilename; see with --path body.process"
        );
        // A list's items keep the list's level and path.
        assert_eq!(
            body["triggers"]["items"]["collapsed"],
            "2 fields: branchFilters, settings; see with --path body.triggers"
        );
        assert_eq!(
            body["variables"]["collapsed"],
            "a map from names to objects; see one with --path body.variables.*"
        );
    }

    #[test]
    fn when_even_the_first_level_is_too_long_it_is_shown_anyway() {
        let o = outline(&definition(), "", 10);
        assert_eq!(o.depth, Some(1));
        assert!(o.inputs["properties"]["body"]["collapsed"].is_string());
    }

    #[test]
    fn a_path_reaches_through_lists_maps_and_alternatives() {
        let s = definition();
        assert_eq!(
            schema_at_path(&s, "body.process.type").unwrap()["type"],
            "integer"
        );
        assert_eq!(
            schema_at_path(&s, "body.triggers.settings").unwrap()["properties"]["a"]["type"],
            "string"
        );
        assert_eq!(
            schema_at_path(&s, "body.variables.*.isSecret").unwrap()["type"],
            "boolean"
        );
        let alt = json!({"properties": {"x": {"oneOf": [
            {"type": "object", "properties": {"a": {"type": "string"}}},
            {"type": "object", "properties": {"b": {"type": "number"}}}
        ]}}});
        assert_eq!(schema_at_path(&alt, "x.b").unwrap()["type"], "number");
        let dotted =
            json!({"properties": {"fields": {"properties": {"System.State": {"type": "string"}}}}});
        assert_eq!(
            schema_at_path(&dotted, "fields.System.State").unwrap()["type"],
            "string"
        );
    }

    #[test]
    fn a_wrong_path_names_the_fields_there() {
        let s = definition();
        assert_eq!(
            schema_at_path(&s, "body.proces").unwrap_err(),
            "no field 'proces' in 'body'; did you mean 'process'? (fields of 'body': name, process, triggers, \
             variables)"
        );
        assert_eq!(
            schema_at_path(&s, "bdy").unwrap_err(),
            "no field 'bdy' in the tool's inputs; did you mean 'body'? (fields of the tool's inputs: body, \
             project)"
        );
        assert_eq!(
            schema_at_path(&s, "body.trigger").unwrap_err(),
            "no field 'trigger' in 'body'; did you mean 'triggers'? (fields of 'body': name, process, \
             triggers, variables)"
        );
        assert_eq!(
            schema_at_path(&s, "body.zzz").unwrap_err(),
            "no field 'zzz' in 'body' (fields of 'body': name, process, triggers, variables)"
        );
        assert_eq!(
            schema_at_path(&s, "project.x").unwrap_err(),
            "no field 'x' in 'project' ('project' has no fields)"
        );
        assert!(schema_at_path(&s, "").is_err());
    }

    #[test]
    fn collapsed_paths_under_a_path_start_with_it() {
        let s = definition();
        let part = schema_at_path(&s, "body").unwrap();
        let o = outline(part, "body", 300);
        assert_eq!(o.depth, Some(1), "{}", o.inputs);
        assert_eq!(
            o.inputs["properties"]["process"]["collapsed"],
            "2 fields: type, yamlFilename; see with --path body.process"
        );
    }

    #[test]
    fn many_fields_are_listed_up_to_a_limit() {
        let props: Map<String, Value> = (0..45)
            .map(|i| (format!("f{i}"), json!({"type": "string"})))
            .collect();
        let s = json!({"properties": {"body": {"type": "object", "properties": props}}});
        let o = outline(&s, "", 50);
        let line = o.inputs["properties"]["body"]["collapsed"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(line.starts_with("45 fields: f0, f1, "), "{line}");
        assert!(
            line.ends_with(" and 5 more; see with --path body"),
            "{line}"
        );
    }
}
