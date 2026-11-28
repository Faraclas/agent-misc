use anyhow::{anyhow, Result};
use reqwest::Url;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

use crate::auth::{ApiKeyAuth, AuthConfig, AuthType, BasicAuth, OAuth2Auth};
use crate::providers::base::{BaseProvider, ProviderType};
use crate::providers::http::HttpProvider;
use crate::tools::{Tool, ToolInputOutputSchema};

pub const VERSION: &str = "1.0";

/// Representation of a generated UTCP manual derived from an OpenAPI spec.
#[derive(Debug, Clone)]
pub struct UtcpManual {
    pub version: String,
    pub tools: Vec<Tool>,
}

/// Converts OpenAPI v2/v3 documents into UTCP tool definitions.
pub struct OpenApiConverter {
    spec: Value,
    spec_url: Option<String>,
    provider_name: String,
}

impl OpenApiConverter {
    /// Build a converter from an already loaded spec value.
    pub fn new(
        openapi_spec: Value,
        spec_url: Option<String>,
        provider_name: Option<String>,
    ) -> Self {
        let provider_name = provider_name
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| derive_provider_name(&openapi_spec));

        Self {
            spec: openapi_spec,
            spec_url,
            provider_name,
        }
    }

    /// Fetch and parse a remote OpenAPI document, inferring a provider name when missing.
    pub async fn new_from_url(spec_url: &str, provider_name: Option<String>) -> Result<Self> {
        let (spec, final_url) = load_spec_from_url(spec_url).await?;
        Ok(Self::new(spec, Some(final_url), provider_name))
    }

    /// Convert the OpenAPI document into a UTCP manual containing tools and metadata.
    pub fn convert(&self) -> UtcpManual {
        let mut tools = Vec::new();
        let base_url = self.base_url();

        if let Some(paths) = self.spec.get("paths").and_then(|v| v.as_object()) {
            for (raw_path, raw_item) in paths {
                if let Some(path_item) = raw_item.as_object() {
                    for (method, raw_op) in path_item {
                        let lower = method.to_ascii_lowercase();
                        if !matches!(lower.as_str(), "get" | "post" | "put" | "delete" | "patch") {
                            continue;
                        }

                        if let Some(op) = raw_op.as_object() {
                            if let Ok(Some(tool)) =
                                self.create_tool(raw_path, &lower, op, &base_url)
                            {
                                tools.push(tool);
                            }
                        }
                    }
                }
            }
        }

        UtcpManual {
            version: VERSION.to_string(),
            tools,
        }
    }

    fn base_url(&self) -> String {
        if let Some(servers) = self.spec.get("servers").and_then(|v| v.as_array()) {
            if let Some(first) = servers.first().and_then(|v| v.as_object()) {
                if let Some(url) = first.get("url").and_then(|v| v.as_str()) {
                    if !url.is_empty() {
                        return self.resolve_server_url(url);
                    }
                }
            }
        }

        if let Some(host) = self.spec.get("host").and_then(|v| v.as_str()) {
            let scheme = self
                .spec
                .get("schemes")
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
                .and_then(|v| v.as_str())
                .unwrap_or("https");
            let base_path = self
                .spec
                .get("basePath")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            return format!("{}://{}{}", scheme, host, base_path);
        }

        if let Some(spec_url) = &self.spec_url {
            if let Ok(parsed) = Url::parse(spec_url) {
                if let Some(host) = parsed.host_str() {
                    if let Some(port) = parsed.port() {
                        return format!("{}://{}:{}", parsed.scheme(), host, port);
                    }
                    return format!("{}://{}", parsed.scheme(), host);
                }
            }
        }

        "/".to_string()
    }

    /// OpenAPI 3.x allows relative server URLs (e.g. `/api/v3`), which must be resolved against
    /// the URL the spec was fetched from. Without a spec URL there is no origin, so it is kept as-is.
    fn resolve_server_url(&self, url: &str) -> String {
        if Url::parse(url).is_ok() {
            return url.to_string();
        }
        self.spec_url
            .as_deref()
            .and_then(|spec_url| Url::parse(spec_url).ok())
            .and_then(|base| base.join(url).ok())
            .map(|resolved| resolved.to_string().trim_end_matches('/').to_string())
            .unwrap_or_else(|| url.to_string())
    }

    fn resolve_ref(&self, reference: &str) -> Result<Value> {
        if !reference.starts_with("#/") {
            return Err(anyhow!("only local refs supported, got {}", reference));
        }
        let pointer = &reference[1..];
        self.spec
            .pointer(pointer)
            .cloned()
            .ok_or_else(|| anyhow!("ref {} not found", reference))
    }

    fn resolve_schema(&self, schema: Value) -> Value {
        self.resolve_schema_within(schema, &mut Vec::new())
    }

    /// Expands `$ref`s recursively. `active` holds the references being expanded on the current path, so a schema
    /// that refers to itself (a tree node holding its children) stops there instead of overflowing the stack.
    fn resolve_schema_within(&self, schema: Value, active: &mut Vec<String>) -> Value {
        match schema {
            Value::Object(map) => {
                if let Some(Value::String(reference)) = map.get("$ref").cloned() {
                    if active.contains(&reference) {
                        return recursive_ref_placeholder(&reference);
                    }
                    if let Ok(resolved) = self.resolve_ref(&reference) {
                        active.push(reference);
                        let out = self.resolve_schema_within(resolved, active);
                        active.pop();
                        return out;
                    }
                    return Value::Object(map);
                }

                let mut out = Map::new();
                for (k, v) in map {
                    out.insert(k, self.resolve_schema_within(v, active));
                }
                Value::Object(out)
            }
            Value::Array(arr) => Value::Array(
                arr.into_iter()
                    .map(|item| self.resolve_schema_within(item, active))
                    .collect(),
            ),
            other => other,
        }
    }

    /// Follows a chain of `$ref`s at the top of `value` only, leaving everything below it unexpanded.
    fn deref_shallow(&self, value: &Value) -> Map<String, Value> {
        let mut current = value.clone();
        for _ in 0..MAX_REF_HOPS {
            let Some(reference) = current.get("$ref").and_then(Value::as_str) else {
                break;
            };
            match self.resolve_ref(reference) {
                Ok(next) => current = next,
                Err(_) => break,
            }
        }
        current.as_object().cloned().unwrap_or_default()
    }

    fn extract_auth(&self, operation: &Map<String, Value>) -> Option<AuthConfig> {
        let mut reqs = Vec::new();
        if let Some(op_sec) = operation.get("security").and_then(|v| v.as_array()) {
            if !op_sec.is_empty() {
                reqs = op_sec.clone();
            }
        }
        if reqs.is_empty() {
            if let Some(global) = self.spec.get("security").and_then(|v| v.as_array()) {
                reqs = global.clone();
            }
        }
        if reqs.is_empty() {
            return None;
        }

        let schemes = self.get_security_schemes().unwrap_or_default();
        for raw in reqs {
            if let Some(sec_map) = raw.as_object() {
                for name in sec_map.keys() {
                    if let Some(Value::Object(scheme)) = schemes.get(name) {
                        if let Some(auth) = self.create_auth_from_scheme(scheme) {
                            return Some(auth);
                        }
                    }
                }
            }
        }
        None
    }

    fn get_security_schemes(&self) -> Option<Map<String, Value>> {
        if let Some(components) = self.spec.get("components").and_then(|v| v.as_object()) {
            if let Some(security_schemes) = components
                .get("securitySchemes")
                .and_then(|v| v.as_object())
            {
                return Some(security_schemes.clone());
            }
        }
        if let Some(defs) = self
            .spec
            .get("securityDefinitions")
            .and_then(|v| v.as_object())
        {
            return Some(defs.clone());
        }
        None
    }

    fn create_auth_from_scheme(&self, scheme: &Map<String, Value>) -> Option<AuthConfig> {
        let typ = scheme
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        match typ.as_str() {
            "apikey" => {
                let location = scheme
                    .get("in")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = scheme
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if location.is_empty() || name.is_empty() {
                    return None;
                }
                let auth = ApiKeyAuth {
                    auth_type: AuthType::ApiKey,
                    api_key: format!("${{{}_API_KEY}}", self.provider_name.to_uppercase()),
                    var_name: name,
                    location,
                };
                Some(AuthConfig::ApiKey(auth))
            }
            "basic" => {
                let auth = BasicAuth {
                    auth_type: AuthType::Basic,
                    username: format!("${{{}_USERNAME}}", self.provider_name.to_uppercase()),
                    password: format!("${{{}_PASSWORD}}", self.provider_name.to_uppercase()),
                };
                Some(AuthConfig::Basic(auth))
            }
            "http" => {
                let scheme_name = scheme
                    .get("scheme")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                match scheme_name.as_str() {
                    "basic" => {
                        let auth = BasicAuth {
                            auth_type: AuthType::Basic,
                            username: format!(
                                "${{{}_USERNAME}}",
                                self.provider_name.to_uppercase()
                            ),
                            password: format!(
                                "${{{}_PASSWORD}}",
                                self.provider_name.to_uppercase()
                            ),
                        };
                        Some(AuthConfig::Basic(auth))
                    }
                    "bearer" => {
                        let auth = ApiKeyAuth {
                            auth_type: AuthType::ApiKey,
                            api_key: format!(
                                "Bearer ${{{}_API_KEY}}",
                                self.provider_name.to_uppercase()
                            ),
                            var_name: "Authorization".to_string(),
                            location: "header".to_string(),
                        };
                        Some(AuthConfig::ApiKey(auth))
                    }
                    _ => None,
                }
            }
            "oauth2" => {
                if let Some(flows) = scheme.get("flows").and_then(|v| v.as_object()) {
                    for raw_flow in flows.values() {
                        if let Some(flow) = raw_flow.as_object() {
                            if let Some(token_url) = flow.get("tokenUrl").and_then(|v| v.as_str()) {
                                let scope = flow
                                    .get("scopes")
                                    .and_then(|v| v.as_object())
                                    .map(|m| m.keys().cloned().collect::<Vec<_>>().join(" "));
                                let auth = OAuth2Auth {
                                    auth_type: AuthType::OAuth2,
                                    token_url: token_url.to_string(),
                                    client_id: format!(
                                        "${{{}_CLIENT_ID}}",
                                        self.provider_name.to_uppercase()
                                    ),
                                    client_secret: format!(
                                        "${{{}_CLIENT_SECRET}}",
                                        self.provider_name.to_uppercase()
                                    ),
                                    scope: optional_string(scope.unwrap_or_default()),
                                };
                                return Some(AuthConfig::OAuth2(auth));
                            }
                        }
                    }
                }

                if let Some(token_url) = scheme.get("tokenUrl").and_then(|v| v.as_str()) {
                    let scope = scheme
                        .get("scopes")
                        .and_then(|v| v.as_object())
                        .map(|m| m.keys().cloned().collect::<Vec<_>>().join(" "));
                    let auth = OAuth2Auth {
                        auth_type: AuthType::OAuth2,
                        token_url: token_url.to_string(),
                        client_id: format!("${{{}_CLIENT_ID}}", self.provider_name.to_uppercase()),
                        client_secret: format!(
                            "${{{}_CLIENT_SECRET}}",
                            self.provider_name.to_uppercase()
                        ),
                        scope: optional_string(scope.unwrap_or_default()),
                    };
                    return Some(AuthConfig::OAuth2(auth));
                }

                None
            }
            _ => None,
        }
    }

    fn create_tool(
        &self,
        path: &str,
        method: &str,
        op: &Map<String, Value>,
        base_url: &str,
    ) -> Result<Option<Tool>> {
        let op_id = op
            .get("operationId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                let sanitized_path = path.trim_matches('/').replace('/', "_");
                format!("{}_{}", method.to_ascii_lowercase(), sanitized_path)
            });

        let description = op
            .get("summary")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .or_else(|| {
                op.get("description")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_default();

        let tags = op
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let (input_schema, headers, body_field) = self.extract_inputs(op);
        let output_schema = self.extract_outputs(op);
        let auth = self.extract_auth(op);

        let provider = HttpProvider {
            base: BaseProvider {
                name: self.provider_name.clone(),
                provider_type: ProviderType::Http,
                auth,
                allowed_communication_protocols: None,
            },
            http_method: method.to_ascii_uppercase(),
            url: join_url(base_url, path),
            content_type: Some("application/json".to_string()),
            headers: None,
            body_field,
            header_fields: if headers.is_empty() {
                None
            } else {
                Some(headers)
            },
            body_type: None,
            response_type: None,
        };

        let provider_value = serde_json::to_value(provider)?;
        Ok(Some(Tool {
            name: op_id,
            description,
            inputs: input_schema,
            outputs: output_schema,
            tags,
            average_response_size: None,
            provider: Some(provider_value),
            annotations: None,
        }))
    }

    fn extract_inputs(
        &self,
        op: &Map<String, Value>,
    ) -> (ToolInputOutputSchema, Vec<String>, Option<String>) {
        let mut props: HashMap<String, Value> = HashMap::new();
        let mut required: Vec<String> = Vec::new();
        let mut headers = Vec::new();
        let mut body_field: Option<String> = None;

        if let Some(parameters) = op.get("parameters").and_then(|v| v.as_array()) {
            for raw_param in parameters {
                let param = self.resolve_schema(raw_param.clone());
                if let Some(param_obj) = param.as_object() {
                    let name = param_obj
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let location = param_obj
                        .get("in")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if name.is_empty() {
                        continue;
                    }
                    if location == "header" {
                        headers.push(name.clone());
                    }
                    if location == "body" {
                        body_field = Some(name.clone());
                    }

                    let schema_val = param_obj
                        .get("schema")
                        .cloned()
                        .unwrap_or_else(|| Value::Object(Map::new()));
                    let schema_obj = self.resolve_schema(schema_val);
                    let schema_map = schema_obj.as_object().cloned().unwrap_or_default();
                    let mut entry = Map::new();

                    if let Some(desc) = param_obj.get("description") {
                        entry.insert("description".to_string(), desc.clone());
                    }
                    if let Some(typ) = schema_map.get("type").or_else(|| param_obj.get("type")) {
                        entry.insert("type".to_string(), typ.clone());
                    }
                    for (k, v) in schema_map {
                        entry.insert(k, v);
                    }
                    if needs_default_type(&entry) {
                        entry.insert("type".to_string(), Value::String("object".to_string()));
                    }

                    props.insert(name.clone(), Value::Object(entry));
                    if param_obj
                        .get("required")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                    {
                        required.push(name);
                    }
                }
            }
        }

        if let Some(request_body) = op.get("requestBody") {
            let rb = self.resolve_schema(request_body.clone());
            if let Some(rb_obj) = rb.as_object() {
                if let Some(content) = rb_obj.get("content").and_then(|v| v.as_object()) {
                    if let Some(app_json) =
                        content.get("application/json").and_then(|v| v.as_object())
                    {
                        if let Some(schema) = app_json.get("schema") {
                            let name = "body".to_string();
                            body_field = Some(name.clone());
                            let schema_obj = self.resolve_schema(schema.clone());
                            let schema_map = schema_obj.as_object().cloned().unwrap_or_default();
                            let mut entry = Map::new();
                            if let Some(desc) = rb_obj.get("description") {
                                entry.insert("description".to_string(), desc.clone());
                            }
                            for (k, v) in schema_map {
                                entry.insert(k, v);
                            }
                            if needs_default_type(&entry) {
                                entry.insert(
                                    "type".to_string(),
                                    Value::String("object".to_string()),
                                );
                            }
                            props.insert(name.clone(), Value::Object(entry));
                            if rb_obj
                                .get("required")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false)
                            {
                                required.push(name);
                            }
                        }
                    }
                }
            }
        }

        let schema = ToolInputOutputSchema {
            type_: "object".to_string(),
            properties: if props.is_empty() { None } else { Some(props) },
            required: if required.is_empty() {
                None
            } else {
                Some(required)
            },
            description: None,
            title: None,
            items: None,
            enum_: None,
            minimum: None,
            maximum: None,
            format: None,
        };

        (schema, headers, body_field)
    }

    fn extract_outputs(&self, op: &Map<String, Value>) -> ToolInputOutputSchema {
        let default_schema = ToolInputOutputSchema {
            type_: "object".to_string(),
            properties: None,
            required: None,
            description: None,
            title: None,
            items: None,
            enum_: None,
            minimum: None,
            maximum: None,
            format: None,
        };

        let responses = match op.get("responses").and_then(|v| v.as_object()) {
            Some(r) => r,
            None => return default_schema,
        };
        let Some(resp) = responses.get("200").or_else(|| responses.get("201")) else {
            return default_schema;
        };

        // The response's schema is outlined, not expanded: only `$ref`s on the way to it are followed here.
        let resp_obj = self.deref_shallow(resp);
        let fallback = resp_obj
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let schema = resp_obj
            .get("content")
            .and_then(|content| content.get("application/json"))
            .and_then(|app_json| app_json.get("schema"))
            .or_else(|| resp_obj.get("schema"));
        match schema {
            Some(schema) => self.build_schema_from_value(schema, fallback),
            None => default_schema,
        }
    }

    /// Outlines a response schema one level deep: its own type and description, and the name, type and format of each
    /// field (of the object, or of each item of a list). Nested fields are not expanded. Nothing reads output schemas
    /// to make a call; the outline shows agents what a tool answers and which fields `--fields` can pick, while a full
    /// expansion of a large API's responses runs to hundreds of KB per tool.
    fn build_schema_from_value(
        &self,
        schema: &Value,
        fallback_description: Option<String>,
    ) -> ToolInputOutputSchema {
        let map = self.merge_all_of(self.deref_shallow(schema));

        let mut out = ToolInputOutputSchema {
            type_: map
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("object")
                .to_string(),
            properties: self.outline_properties(map.get("properties")),
            required: string_slice(map.get("required")),
            description: match map
                .get("description")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
            {
                Some(desc) if !desc.is_empty() => Some(desc),
                _ => fallback_description.filter(|s| !s.is_empty()),
            },
            title: map
                .get("title")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            items: None,
            enum_: map.get("enum").and_then(interface_slice),
            minimum: cast_float(map.get("minimum")),
            maximum: cast_float(map.get("maximum")),
            format: map
                .get("format")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        };

        if out.type_ == "array" {
            out.items = map.get("items").map(|items| {
                let item = self.merge_all_of(self.deref_shallow(items));
                let mut outline = HashMap::new();
                if let Some(typ) = self.outline_type(&item) {
                    outline.insert("type".to_string(), typ);
                }
                if let Some(props) = self.outline_properties(item.get("properties")) {
                    outline.insert(
                        "properties".to_string(),
                        Value::Object(props.into_iter().collect()),
                    );
                }
                outline
            });
        }

        out
    }

    /// Folds the `properties` and `required` of `allOf` parts into the schema itself, one level deep.
    fn merge_all_of(&self, mut map: Map<String, Value>) -> Map<String, Value> {
        let Some(parts) = map.remove("allOf").and_then(|v| v.as_array().cloned()) else {
            return map;
        };
        for part in &parts {
            let part = self.deref_shallow(part);
            if let Some(props) = part.get("properties").and_then(Value::as_object) {
                let target = map
                    .entry("properties")
                    .or_insert_with(|| Value::Object(Map::new()));
                if let Some(target) = target.as_object_mut() {
                    for (k, v) in props {
                        target.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
            }
            if let Some(req) = part.get("required").and_then(Value::as_array) {
                let target = map
                    .entry("required")
                    .or_insert_with(|| Value::Array(Vec::new()));
                if let Some(target) = target.as_array_mut() {
                    for r in req {
                        if !target.contains(r) {
                            target.push(r.clone());
                        }
                    }
                }
            }
            for key in ["type", "description", "title"] {
                if let Some(v) = part.get(key) {
                    map.entry(key).or_insert_with(|| v.clone());
                }
            }
        }
        map
    }

    /// Each field reduced to its `type` (with `format`, and the item type of a list), without its own fields.
    fn outline_properties(&self, value: Option<&Value>) -> Option<HashMap<String, Value>> {
        let props = value.and_then(Value::as_object)?;
        Some(
            props
                .iter()
                .map(|(name, prop)| {
                    let prop = self.merge_all_of(self.deref_shallow(prop));
                    let mut outline = Map::new();
                    if let Some(typ) = self.outline_type(&prop) {
                        outline.insert("type".to_string(), typ);
                    }
                    if let Some(format) = prop.get("format") {
                        outline.insert("format".to_string(), format.clone());
                    }
                    if prop.get("type").and_then(Value::as_str) == Some("array") {
                        if let Some(item_type) = prop
                            .get("items")
                            .map(|items| self.deref_shallow(items))
                            .and_then(|items| self.outline_type(&items))
                        {
                            outline.insert("items".to_string(), json!({ "type": item_type }));
                        }
                    }
                    (name.clone(), Value::Object(outline))
                })
                .collect(),
        )
    }

    /// The schema's `type`, or for a `oneOf` / `anyOf` the distinct types of its variants (a string for one, a list
    /// for several), or `object` when it only has fields.
    fn outline_type(&self, schema: &Map<String, Value>) -> Option<Value> {
        if let Some(typ) = schema.get("type") {
            return Some(typ.clone());
        }
        let variants = schema
            .get("oneOf")
            .or_else(|| schema.get("anyOf"))
            .and_then(Value::as_array);
        if let Some(variants) = variants {
            let mut types: Vec<Value> = Vec::new();
            for variant in variants {
                let variant = self.deref_shallow(variant);
                let typ = variant.get("type").cloned().or_else(|| {
                    variant
                        .contains_key("properties")
                        .then(|| Value::String("object".to_string()))
                });
                if let Some(typ) = typ {
                    if !types.contains(&typ) {
                        types.push(typ);
                    }
                }
            }
            return match types.len() {
                0 => None,
                1 => types.pop(),
                _ => Some(Value::Array(types)),
            };
        }
        schema
            .contains_key("properties")
            .then(|| Value::String("object".to_string()))
    }
}

/// Load an OpenAPI/Swagger document from a URL, handling JSON or YAML.
pub async fn load_spec_from_url(raw_url: &str) -> Result<(Value, String)> {
    crate::security::validate_url_security(raw_url, false)?;
    let client = crate::security::build_secure_http_client();
    let resp =
        crate::transports::rate_limit::reject_rate_limited(client.get(raw_url).send().await?)
            .await?;
    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow!("unexpected HTTP status: {}", status));
    }

    let final_url = resp.url().to_string();
    let bytes = resp.bytes().await?;
    parse_spec_bytes(&bytes).map(|val| (val, final_url))
}

/// Parse raw JSON or YAML OpenAPI/UTCP bytes into a `serde_json::Value`.
pub fn parse_spec_bytes(bytes: &[u8]) -> Result<Value> {
    if let Ok(json_spec) = serde_json::from_slice::<Value>(bytes) {
        return Ok(json_spec);
    }

    let yaml_value: serde_norway::Value = serde_norway::from_slice(bytes)
        .map_err(|err| anyhow!("failed to parse as JSON or YAML: {}", err))?;
    let json_value = serde_json::to_value(yaml_value)?;
    Ok(json_value)
}

fn derive_provider_name(spec: &Value) -> String {
    let title = spec
        .get("info")
        .and_then(|v| v.as_object())
        .and_then(|info| info.get("title"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let base = if title.is_empty() {
        "openapi_provider".to_string()
    } else {
        title
    };
    let invalid = " -.,!?'\"\\\\/()[]{}#@$%^&*+=~`|;:<>";

    let mut output = String::new();
    for ch in base.chars() {
        if invalid.contains(ch) {
            output.push('_');
        } else {
            output.push(ch);
        }
    }
    if output.is_empty() {
        "openapi_provider".to_string()
    } else {
        output
    }
}

fn optional_string(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Longest chain of `$ref`s to `$ref`s followed before giving up (guards against a reference loop).
const MAX_REF_HOPS: usize = 32;

/// A parameter or body schema gets `type: object` only when nothing else says what it holds. A `oneOf` of an integer
/// and a string (GitHub's `workflow_id`) is not an object.
fn needs_default_type(schema: &Map<String, Value>) -> bool {
    !["type", "oneOf", "anyOf", "allOf", "enum", "const", "not"]
        .iter()
        .any(|key| schema.contains_key(*key))
}

/// Stands in for a `$ref` met again inside its own expansion.
fn recursive_ref_placeholder(reference: &str) -> Value {
    let name = reference.rsplit('/').next().unwrap_or(reference);
    json!({ "description": format!("Recursive: the same schema as `{name}` above, not expanded again.") })
}

fn join_url(base: &str, path: &str) -> String {
    let trimmed_base = base.trim_end_matches('/');
    let trimmed_path = path.trim_start_matches('/');
    if trimmed_base.is_empty() {
        format!("/{}", trimmed_path)
    } else if trimmed_path.is_empty() {
        trimmed_base.to_string()
    } else {
        format!("{}/{}", trimmed_base, trimmed_path)
    }
}

fn string_slice(value: Option<&Value>) -> Option<Vec<String>> {
    value.and_then(|v| v.as_array()).map(|arr| {
        let collected = arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect::<Vec<_>>();
        if collected.is_empty() {
            None
        } else {
            Some(collected)
        }
    })?
}

fn interface_slice(value: &Value) -> Option<Vec<Value>> {
    value.as_array().map(|arr| {
        if arr.is_empty() {
            None
        } else {
            Some(arr.to_vec())
        }
    })?
}

fn cast_float(value: Option<&Value>) -> Option<f64> {
    value.and_then(|v| {
        if let Some(n) = v.as_f64() {
            Some(n)
        } else {
            v.as_i64().map(|i| i as f64)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn build_test_converter() -> OpenApiConverter {
        let spec = json!({
            "info": {"title": "Test"},
            "components": {
                "schemas": {
                    "Obj": {
                        "type": "object",
                        "properties": { "name": { "type": "string" } }
                    }
                },
                "securitySchemes": {
                    "apiKey": { "type": "apiKey", "name": "X-Token", "in": "header" },
                    "basicAuth": { "type": "http", "scheme": "basic" }
                }
            },
            "security": [ { "apiKey": [] } ]
        });

        OpenApiConverter::new(
            spec,
            Some("https://api.example.com/spec.json".to_string()),
            Some("test".to_string()),
        )
    }

    #[test]
    fn resolve_ref_and_schema() {
        let converter = build_test_converter();
        let obj = converter.resolve_ref("#/components/schemas/Obj").unwrap();
        assert_eq!(obj.get("type").and_then(|v| v.as_str()), Some("object"));
        assert!(converter.resolve_ref("#/bad/ref").is_err());

        let resolved = converter.resolve_schema(json!({"$ref": "#/components/schemas/Obj"}));
        assert_eq!(
            resolved
                .get("properties")
                .and_then(|v| v.get("name"))
                .and_then(|v| v.get("type"))
                .and_then(|v| v.as_str()),
            Some("string")
        );
    }

    #[test]
    fn create_auth_from_scheme_and_extract() {
        let converter = build_test_converter();
        let api_key = converter
            .create_auth_from_scheme(
                json!({"type": "apiKey", "in": "header", "name": "X"})
                    .as_object()
                    .unwrap(),
            )
            .unwrap();
        match api_key {
            AuthConfig::ApiKey(auth) => {
                assert_eq!(auth.var_name, "X");
                assert_eq!(auth.location, "header");
                assert_eq!(auth.api_key, "${TEST_API_KEY}");
            }
            _ => panic!("expected ApiKey auth"),
        }

        let basic = converter
            .create_auth_from_scheme(
                json!({"type": "http", "scheme": "basic"})
                    .as_object()
                    .unwrap(),
            )
            .unwrap();
        matches!(basic, AuthConfig::Basic(_));

        let bearer = converter
            .create_auth_from_scheme(
                json!({"type": "http", "scheme": "bearer"})
                    .as_object()
                    .unwrap(),
            )
            .unwrap();
        match bearer {
            AuthConfig::ApiKey(auth) => {
                assert_eq!(auth.var_name, "Authorization");
                assert!(auth.api_key.contains("${TEST_API_KEY}"));
            }
            _ => panic!("expected bearer api key auth"),
        }

        let mut op = Map::new();
        op.insert("security".to_string(), json!([{"basicAuth": []}]));
        let auth = converter.extract_auth(&op).unwrap();
        matches!(auth, AuthConfig::Basic(_));
    }

    #[test]
    fn inputs_outputs_and_create_tool() {
        let converter = build_test_converter();
        let op_value = json!({
            "operationId": "ping",
            "summary": "Ping",
            "tags": ["t"],
            "parameters": [
                { "name": "id", "in": "query", "required": true, "schema": { "type": "string" }},
                { "name": "X", "in": "header", "schema": { "type": "string" }}
            ],
            "requestBody": {
                "required": true,
                "content": {
                    "application/json": {
                        "schema": {
                            "type": "object",
                            "properties": { "foo": { "type": "string" } }
                        }
                    }
                }
            },
            "responses": {
                "200": {
                    "content": {
                        "application/json": {
                            "schema": {
                                "type": "object",
                                "description": "desc",
                                "properties": { "ok": { "type": "boolean" } }
                            }
                        }
                    }
                }
            }
        });
        let op = op_value.as_object().unwrap().clone();

        let (schema, headers, body) = converter.extract_inputs(&op);
        assert_eq!(schema.properties.as_ref().map(|m| m.len()), Some(3));
        assert_eq!(headers, vec!["X".to_string()]);
        assert_eq!(body.as_deref(), Some("body"));

        let out = converter.extract_outputs(&op);
        assert_eq!(out.type_, "object");
        assert!(out.properties.unwrap().contains_key("ok"));

        let tool = converter
            .create_tool("/ping", "get", &op, "https://api.example.com")
            .unwrap()
            .unwrap();
        assert_eq!(tool.name, "ping");
        let prov: HttpProvider = serde_json::from_value(tool.provider.unwrap()).unwrap();
        assert_eq!(prov.url, "https://api.example.com/ping");
    }

    #[test]
    fn relative_server_url_resolves_against_spec_url() {
        let spec = |server: &str| {
            json!({
                "info": {"title": "Pets"},
                "servers": [{"url": server}],
                "paths": {}
            })
        };
        let spec_url = Some("http://127.0.0.1:8765/api/v3/openapi.json".to_string());

        let abs = OpenApiConverter::new(spec("/api/v3"), spec_url.clone(), None);
        assert_eq!(abs.base_url(), "http://127.0.0.1:8765/api/v3");

        let rel = OpenApiConverter::new(spec("v2/"), spec_url.clone(), None);
        assert_eq!(rel.base_url(), "http://127.0.0.1:8765/api/v3/v2");

        let full = OpenApiConverter::new(spec("https://api.example.com/v1"), spec_url, None);
        assert_eq!(full.base_url(), "https://api.example.com/v1");

        let no_origin = OpenApiConverter::new(spec("/api/v3"), None, None);
        assert_eq!(no_origin.base_url(), "/api/v3");
    }

    #[test]
    fn convert_basic() {
        let spec = json!({
            "info": {"title": "Test API"},
            "servers": [{"url": "https://api.example.com"}],
            "paths": {
                "/ping": {
                    "get": {
                        "operationId": "ping",
                        "summary": "Ping",
                        "responses": {
                            "200": {
                                "content": {
                                    "application/json": {
                                        "schema": { "type": "object" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });
        let converter = OpenApiConverter::new(spec, None, None);
        let manual = converter.convert();
        assert_eq!(manual.version, VERSION);
        assert_eq!(manual.tools.len(), 1);
        assert_eq!(manual.tools[0].name, "ping");
    }

    /// A spec shaped like GitHub's: shared response schemas, nested objects, lists, `allOf`, a `oneOf` parameter, and a
    /// schema that refers to itself.
    fn outline_test_converter() -> OpenApiConverter {
        let spec = json!({
            "info": {"title": "Outline"},
            "servers": [{"url": "https://api.example.com"}],
            "components": {
                "schemas": {
                    "owner": {
                        "type": "object",
                        "properties": {
                            "login": {"type": "string", "description": "Login name"},
                            "plan": {"type": "object", "properties": {"seats": {"type": "integer"}}}
                        }
                    },
                    "repo": {
                        "type": "object",
                        "description": "A repository",
                        "required": ["id"],
                        "properties": {
                            "id": {"type": "integer", "format": "int64"},
                            "owner": {"$ref": "#/components/schemas/owner"},
                            "topics": {"type": "array", "items": {"type": "string"}},
                            "license": {"anyOf": [{"type": "null"}, {"$ref": "#/components/schemas/owner"}]}
                        }
                    },
                    "full-repo": {
                        "allOf": [
                            {"$ref": "#/components/schemas/repo"},
                            {"type": "object", "properties": {"forks": {"type": "integer"}}}
                        ]
                    },
                    "node": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string"},
                            "children": {"type": "array", "items": {"$ref": "#/components/schemas/node"}}
                        }
                    }
                },
                "responses": {
                    "repo-list": {
                        "description": "Response",
                        "content": {"application/json": {"schema": {
                            "type": "array",
                            "items": {"$ref": "#/components/schemas/repo"}
                        }}}
                    }
                }
            },
            "paths": {
                "/repos": {
                    "get": {
                        "operationId": "list-repos",
                        "responses": {"200": {"$ref": "#/components/responses/repo-list"}}
                    }
                },
                "/repos/{id}": {
                    "get": {
                        "operationId": "get-repo",
                        "parameters": [{
                            "name": "id", "in": "path", "required": true,
                            "schema": {"oneOf": [{"type": "integer"}, {"type": "string"}]}
                        }],
                        "responses": {"200": {
                            "description": "Response",
                            "content": {"application/json": {"schema": {"$ref": "#/components/schemas/full-repo"}}}
                        }}
                    }
                },
                "/trees": {
                    "post": {
                        "operationId": "create-tree",
                        "requestBody": {"content": {"application/json": {"schema": {
                            "oneOf": [{"$ref": "#/components/schemas/node"}, {"type": "string"}]
                        }}}},
                        "responses": {"201": {
                            "description": "Created",
                            "content": {"application/json": {"schema": {"$ref": "#/components/schemas/node"}}}
                        }}
                    }
                }
            }
        });
        OpenApiConverter::new(spec, None, Some("outline".to_string()))
    }

    fn tool<'a>(manual: &'a UtcpManual, name: &str) -> &'a Tool {
        manual.tools.iter().find(|t| t.name == name).unwrap()
    }

    #[test]
    fn outputs_are_outlined_one_level_deep() {
        let manual = outline_test_converter().convert();

        let get = serde_json::to_value(&tool(&manual, "get-repo").outputs).unwrap();
        assert_eq!(
            get,
            json!({
                "type": "object",
                "description": "A repository",
                "required": ["id"],
                "properties": {
                    "id": {"type": "integer", "format": "int64"},
                    "owner": {"type": "object"},
                    "topics": {"type": "array", "items": {"type": "string"}},
                    "license": {"type": ["null", "object"]},
                    "forks": {"type": "integer"}
                }
            })
        );

        let list = serde_json::to_value(&tool(&manual, "list-repos").outputs).unwrap();
        assert_eq!(list["type"], "array");
        assert_eq!(list["description"], "Response");
        assert_eq!(list["items"]["type"], "object");
        assert_eq!(
            list["items"]["properties"]["owner"],
            json!({"type": "object"})
        );
        assert!(list["items"]["properties"]["owner"]
            .get("properties")
            .is_none());
    }

    #[test]
    fn one_of_parameter_is_not_typed_object() {
        let manual = outline_test_converter().convert();
        let inputs = serde_json::to_value(&tool(&manual, "get-repo").inputs).unwrap();
        assert_eq!(
            inputs["properties"]["id"],
            json!({"oneOf": [{"type": "integer"}, {"type": "string"}]})
        );
    }

    #[test]
    fn self_referencing_schema_stops_instead_of_overflowing() {
        let manual = outline_test_converter().convert();
        let create = tool(&manual, "create-tree");

        let inputs = serde_json::to_value(&create.inputs).unwrap();
        let body = &inputs["properties"]["body"];
        assert!(
            body.get("type").is_none(),
            "a oneOf body is not typed object"
        );
        let node = &body["oneOf"][0];
        assert_eq!(node["properties"]["name"]["type"], "string");
        let again = &node["properties"]["children"]["items"];
        assert!(again["description"]
            .as_str()
            .unwrap()
            .contains("Recursive: the same schema as `node`"));

        let outputs = serde_json::to_value(&create.outputs).unwrap();
        assert_eq!(
            outputs["properties"]["children"],
            json!({"type": "array", "items": {"type": "object"}})
        );
    }
}
