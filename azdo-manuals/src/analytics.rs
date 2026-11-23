//! The Analytics service's OData API, from its `$metadata` (CSDL XML, part of the OData standard).
//!
//! Analytics is not REST in the spec's sense: it is one OData service per organization or project
//! (`https://analytics.dev.azure.com/<org>[/<project>]/_odata/<version>/`), and its `$metadata` lists every entity set
//! with its properties, keys and navigation properties. Each entity set becomes one read tool taking the OData query
//! options. `$metadata` is read from the organization's service root (the union of every project's fields, custom
//! fields included); the tools query a project's root, which needs only project-level Analytics permission.

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use crate::generate::{Entry, PAT_VAR};
use crate::http::{Request, Transport};
use crate::routes::auth_header;
use crate::text::{snake, sorted_map};

/// v4.0-preview has the test and pipeline entity sets; v2.0, the newest released version, has only work tracking.
pub const ODATA_VERSION: &str = "v4.0-preview";
pub const HOST: &str = "analytics.dev.azure.com";
pub const SCOPE: &str = "vso.analytics";
pub const SCOPE_DESCRIPTION: &str = "Grants the ability to query analytics data.";
pub const MANUAL: &str = "azdo_analytics";
pub const AREA: &str = "analytics";
const USER_AGENT: &str = concat!("azdo-manuals/", env!("CARGO_PKG_VERSION"), " analytics");

/// One XML element: name (without namespace prefix), attributes, children.
#[derive(Debug, Default)]
pub struct Element {
    pub name: String,
    pub attrs: HashMap<String, String>,
    pub children: Vec<Element>,
}

impl Element {
    pub fn attr(&self, key: &str) -> &str {
        self.attrs.get(key).map(String::as_str).unwrap_or("")
    }

    pub fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// Every descendant with this name, depth first.
    pub fn descendants<'a>(&'a self, name: &str, out: &mut Vec<&'a Element>) {
        for c in &self.children {
            if c.name == name {
                out.push(c);
            }
            c.descendants(name, out);
        }
    }

    /// The `String` of a child `<Annotation Term="...">`, by the term's last part (`DisplayName`, `Description`).
    pub fn annotation(&self, term: &str) -> &str {
        self.all("Annotation")
            .find(|a| a.attr("Term").rsplit('.').next() == Some(term))
            .map(|a| a.attr("String"))
            .unwrap_or("")
    }
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Parse well-formed XML into its element tree (text, comments and processing instructions are skipped).
pub fn parse_xml(text: &str) -> Result<Element, String> {
    let mut stack: Vec<Element> = vec![Element::default()];
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        rest = &rest[open..];
        if rest.starts_with("<?") || rest.starts_with("<!--") || rest.starts_with("<!") {
            let end = if rest.starts_with("<!--") { "-->" } else { ">" };
            let close = rest
                .find(end)
                .ok_or("unterminated XML declaration or comment")?;
            rest = &rest[close + end.len()..];
            continue;
        }
        let close = rest.find('>').ok_or("unterminated XML tag")?;
        let tag = &rest[1..close];
        rest = &rest[close + 1..];
        if let Some(name) = tag.strip_prefix('/') {
            let done = stack.pop().ok_or("unbalanced XML")?;
            let local = name.trim().rsplit(':').next().unwrap_or("");
            if done.name != local {
                return Err(format!("XML closes {local} inside {}", done.name));
            }
            stack
                .last_mut()
                .ok_or("unbalanced XML")?
                .children
                .push(done);
            continue;
        }
        let self_closing = tag.ends_with('/');
        let tag = tag.trim_end_matches('/');
        let (name, mut attrs_text) = tag.split_once(char::is_whitespace).unwrap_or((tag, ""));
        let mut el = Element {
            name: name.rsplit(':').next().unwrap_or("").to_string(),
            ..Element::default()
        };
        loop {
            attrs_text = attrs_text.trim_start();
            let Some((key, after)) = attrs_text.split_once('=') else {
                break;
            };
            let after = after.trim_start();
            let quote = after.chars().next().ok_or("bad XML attribute")?;
            let value_end = after[1..].find(quote).ok_or("unterminated XML attribute")?;
            el.attrs
                .insert(key.trim().to_string(), unescape(&after[1..1 + value_end]));
            attrs_text = &after[value_end + 2..];
        }
        if self_closing {
            stack.last_mut().ok_or("unbalanced XML")?.children.push(el);
        } else {
            stack.push(el);
        }
    }
    if stack.len() != 1 {
        return Err("XML ends inside an element".into());
    }
    Ok(stack.pop().unwrap_or_default())
}

pub fn metadata_url(org: &str) -> String {
    format!("https://{HOST}/{org}/_odata/{ODATA_VERSION}/$metadata")
}

/// The organization's `$metadata` (needs the PAT and Analytics read permission).
pub fn fetch_metadata(transport: &mut Transport, org: &str, pat: &str) -> Result<String, String> {
    let req = Request {
        method: "GET",
        url: metadata_url(org),
        headers: vec![
            ("Authorization".into(), auth_header(pat)),
            ("Accept".into(), "application/xml".into()),
            ("User-Agent".into(), USER_AGENT.into()),
        ],
        body: None,
    };
    let resp = transport(&req).map_err(|e| format!("Analytics $metadata: {e}"))?;
    if !(200..300).contains(&resp.status) {
        return Err(format!("Analytics $metadata: HTTP {}", resp.status));
    }
    Ok(String::from_utf8_lossy(&resp.body).into_owned())
}

fn short_type(t: &str) -> String {
    let (collection, inner) = match t
        .strip_prefix("Collection(")
        .and_then(|s| s.strip_suffix(')'))
    {
        Some(inner) => (true, inner),
        None => (false, t),
    };
    let name = inner.rsplit('.').next().unwrap_or(inner);
    if collection {
        format!("{name}[]")
    } else {
        name.to_string()
    }
}

/// Custom work item fields; listed by count only (an organization can have hundreds).
fn is_custom(name: &str) -> bool {
    name.starts_with("Custom_")
}

/// One read tool per entity set of the `$metadata` document.
pub fn entries(csdl: &str, org: &str) -> Result<Vec<Entry>, String> {
    let root = parse_xml(csdl).map_err(|e| format!("Analytics $metadata: {e}"))?;
    let mut types: HashMap<String, &Element> = HashMap::new();
    let mut schemas = Vec::new();
    root.descendants("Schema", &mut schemas);
    for schema in &schemas {
        for t in schema.all("EntityType") {
            types.insert(
                format!("{}.{}", schema.attr("Namespace"), t.attr("Name")),
                t,
            );
        }
    }
    let mut sets = Vec::new();
    root.descendants("EntitySet", &mut sets);
    if sets.is_empty() {
        return Err("Analytics $metadata lists no entity sets".into());
    }
    let mut out = Vec::new();
    for set in sets {
        let set_name = set.attr("Name");
        let Some(entity) = types.get(set.attr("EntityType")) else {
            continue;
        };
        let display = match set.annotation("DisplayName") {
            "" => set_name,
            d => d,
        };
        let keys: Vec<&str> = entity
            .all("Key")
            .flat_map(|k| k.all("PropertyRef"))
            .map(|p| p.attr("Name"))
            .collect();
        let props: Vec<&Element> = entity.all("Property").collect();
        let navs: Vec<&Element> = entity.all("NavigationProperty").collect();
        let custom = props.iter().filter(|p| is_custom(p.attr("Name"))).count();
        let listed: Vec<String> = props
            .iter()
            .filter(|p| !is_custom(p.attr("Name")))
            .map(|p| format!("{} ({})", p.attr("Name"), short_type(p.attr("Type"))))
            .collect();
        let mut properties = listed.join(", ");
        if custom > 0 {
            properties.push_str(&format!(
                ", and {custom} custom fields named Custom_<field> (their reference names are in the WorkItemTypeFields \
                 entity set)"
            ));
        }
        let expandable: Vec<&str> = navs
            .iter()
            .map(|n| n.attr("Name"))
            .filter(|n| !is_custom(n))
            .collect();
        let entity_description = entity.annotation("Description");
        let mut description = format!(
            "Query {display} ({set_name}) in Analytics with OData. Answers {{\"value\": [rows]}}; a long answer comes in \
             pages, and \"@odata.nextLink\" holds the $skiptoken for the next page."
        );
        if !entity_description.is_empty() {
            description = format!("{entity_description} {description}");
        }
        // Daily snapshots and per-activity results are large: unfiltered, a query does not answer within a minute.
        let large = ["Snapshot", "Daily", "ActivityResults"]
            .iter()
            .any(|s| set_name.contains(s));
        let date_key = ["DateSK", "SamplingDateSK", "PipelineRunCompletedDateSK"]
            .into_iter()
            .find(|k| props.iter().any(|p| p.attr("Name") == *k));
        if let (true, Some(key)) = (large, date_key) {
            description.push_str(&format!(
                " A large set: filter on {key} (a date as yyyyMMdd, e.g. $filter={key} ge 20260101), or the query may \
                 not answer."
            ));
            // The service warns (VS403510) that snapshot sets are meant only for aggregation.
            if set_name.contains("Snapshot") {
                description.push_str(&format!(
                    " Snapshots are meant for aggregation: $apply=filter({key} ge 20260101)/groupby((Property), \
                     aggregate($count as Count))."
                ));
            }
        }
        let mut inputs = Map::new();
        inputs.insert(
            "project".into(),
            json!({"type": "string", "description": "Project ID or project name"}),
        );
        let option = |d: String| json!({"type": "string", "description": d});
        inputs.insert(
            "$select".into(),
            option(format!(
                "Comma-separated properties to return. Properties: {properties}."
            )),
        );
        inputs.insert(
            "$filter".into(),
            option(format!(
                "OData filter, e.g. {} eq 'value' and ... (same properties as $select).",
                keys.first().copied().unwrap_or("Property")
            )),
        );
        inputs.insert(
            "$apply".into(),
            option(
                "OData aggregation, e.g. filter(...)/groupby((Property), aggregate($count as Count)).".into(),
            ),
        );
        inputs.insert(
            "$orderby".into(),
            option("Properties to sort by, e.g. Property desc.".into()),
        );
        if !expandable.is_empty() {
            inputs.insert(
                "$expand".into(),
                option(format!(
                    "Related entities to include, e.g. Name($select=...). Navigation properties: {}.",
                    expandable.join(", ")
                )),
            );
        }
        inputs.insert(
            "$top".into(),
            json!({"type": "integer", "description": "Rows to return at most."}),
        );
        inputs.insert(
            "$skip".into(),
            json!({"type": "integer", "description": "Rows to skip."}),
        );
        inputs.insert(
            "$skiptoken".into(),
            option("The next page's token, from the previous answer's @odata.nextLink.".into()),
        );
        inputs.insert(
            "$count".into(),
            json!({"type": "boolean", "description": "Also return the number of matching rows (@odata.count)."}),
        );
        let mut tool = Map::new();
        let method = format!("Query{set_name}");
        tool.insert("name".into(), Value::Null);
        tool.insert("description".into(), json!(description));
        tool.insert(
            "tags".into(),
            json!(["azure-devops", AREA, snake(set_name)]),
        );
        tool.insert(
            "inputs".into(),
            json!({"type": "object", "properties": Value::Object(sorted_map(inputs)), "required": ["project"]}),
        );
        tool.insert("outputs".into(), json!({"type": "object"}));
        tool.insert(
            "tool_call_template".into(),
            json!({
                "call_template_type": "http",
                "http_method": "GET",
                "url": format!("https://{HOST}/{org}/{{project}}/_odata/{ODATA_VERSION}/{set_name}"),
                "headers": {"Accept": "application/json"},
                "auth": {"auth_type": "basic", "username": "pat", "password": format!("${{{PAT_VAR}}}")},
            }),
        );
        out.push(Entry {
            area: AREA.into(),
            operation_id: format!("{set_name}.Query"),
            method_name: Some(method),
            scopes: vec![SCOPE.into()],
            read: true,
            source: Some("odata".into()),
            scopes_inferred: false,
            name: String::new(),
            tool,
        });
    }
    Ok(out)
}
