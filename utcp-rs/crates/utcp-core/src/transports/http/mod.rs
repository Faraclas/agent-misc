use anyhow::{anyhow, Result};
use async_trait::async_trait;
use reqwest::{header, Client};
use serde_json::Value;
use std::collections::HashMap;

use crate::auth::AuthConfig;
use crate::providers::base::Provider;
use crate::providers::http::HttpProvider;
use crate::security::{validate_size_limit, validate_url_security};
use crate::tools::Tool;
use crate::transports::{stream::StreamResult, ClientTransport};

const MAX_RESPONSE_SIZE: usize = 10 * 1024 * 1024; // 10 MB

/// Transport for synchronous HTTP providers that expose JSON APIs.
pub struct HttpClientTransport {
    pub client: Client,
}

impl HttpClientTransport {
    /// Build a new HTTP client transport with tuned defaults and SSRF protection.
    pub fn new() -> Self {
        Self {
            client: crate::security::build_secure_http_client(),
        }
    }

    /// Build an HTTP client transport sharing an existing warm `reqwest::Client`.
    pub fn with_client(client: Client) -> Self {
        Self { client }
    }

    /// Attach authentication headers or query params to the request builder.
    fn apply_auth(
        &self,
        builder: reqwest::RequestBuilder,
        auth: &AuthConfig,
    ) -> Result<reqwest::RequestBuilder> {
        match auth {
            AuthConfig::ApiKey(api_key) => {
                let location = api_key.location.to_ascii_lowercase();
                match location.as_str() {
                    "header" => Ok(builder.header(&api_key.var_name, &api_key.api_key)),
                    "query" => {
                        Ok(builder.query(&[(api_key.var_name.clone(), api_key.api_key.clone())]))
                    }
                    "cookie" => {
                        let cookie_value = format!("{}={}", api_key.var_name, api_key.api_key);
                        Ok(builder.header(header::COOKIE, cookie_value))
                    }
                    other => Err(anyhow!("Unsupported API key location: {}", other)),
                }
            }
            AuthConfig::Basic(basic) => {
                Ok(builder.basic_auth(&basic.username, Some(&basic.password)))
            }
            AuthConfig::OAuth2(_) => Err(anyhow!(
                "OAuth2 auth is not yet supported by the HTTP transport"
            )),
        }
    }
}

#[async_trait]
impl ClientTransport for HttpClientTransport {
    async fn register_tool_provider(&self, prov: &dyn Provider) -> Result<Vec<Tool>> {
        // Downcast to HttpProvider using as_any
        let http_prov = prov
            .as_any()
            .downcast_ref::<HttpProvider>()
            .ok_or_else(|| anyhow!("Provider is not an HttpProvider"))?;

        // Fetch tool definitions from the HTTP endpoint
        // The endpoint should return a UTCP manifest or OpenAPI spec
        validate_url_security(&http_prov.url, false)?;
        let mut request_builder = self.client.get(&http_prov.url);

        if let Some(headers) = &http_prov.headers {
            for (key, value) in headers {
                request_builder = request_builder.header(key, value);
            }
        }

        if let Some(auth) = &http_prov.base.auth {
            request_builder = self.apply_auth(request_builder, auth)?;
        }

        let response =
            crate::transports::rate_limit::reject_rate_limited(request_builder.send().await?)
                .await?;

        if !response.status().is_success() {
            return Err(anyhow!(
                "Failed to fetch tools from {}: {}",
                http_prov.url,
                response.status()
            ));
        }

        // Try to parse as UTCP manifest first
        let body_bytes = response.bytes().await?;
        validate_size_limit(&body_bytes, MAX_RESPONSE_SIZE)?;

        // Try parsing as JSON
        if let Ok(manifest) = serde_json::from_slice::<Value>(&body_bytes) {
            // Check if it's a UTCP manifest (has "tools" array)
            if let Some(tools_array) = manifest.get("tools").and_then(|v| v.as_array()) {
                let mut tools = Vec::new();
                for tool_value in tools_array {
                    if let Ok(tool) = serde_json::from_value::<Tool>(tool_value.clone()) {
                        tools.push(tool);
                    }
                }
                return Ok(tools);
            }
        }

        // If no tools found, return empty vec
        // In a full implementation, we would also parse OpenAPI specs here
        Ok(vec![])
    }

    async fn deregister_tool_provider(&self, _prov: &dyn Provider) -> Result<()> {
        // HTTP transport is stateless, so nothing to do
        Ok(())
    }

    async fn call_tool(
        &self,
        _tool_name: &str,
        args: HashMap<String, Value>,
        prov: &dyn Provider,
    ) -> Result<Value> {
        // Downcast to HttpProvider using as_any
        let http_prov = prov
            .as_any()
            .downcast_ref::<HttpProvider>()
            .ok_or_else(|| anyhow!("Provider is not an HttpProvider"))?;

        // Handle URL path parameters (e.g., {id} in URL) cleanly without JSON-quoting strings
        let mut remaining_args = args;
        let mut url = http_prov.url.clone();
        let mut consumed_keys = Vec::new();
        for (key, value) in &remaining_args {
            let placeholder = format!("{{{}}}", key);
            if url.contains(&placeholder) {
                let raw_val = match value {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let encoded = urlencoding::encode(&raw_val);
                url = url.replace(&placeholder, &encoded);
                consumed_keys.push(key.clone());
            }
        }
        for key in consumed_keys {
            remaining_args.remove(&key);
        }

        validate_url_security(&url, false)?;

        let method_upper = http_prov.http_method.to_uppercase();
        let mut request_builder = match method_upper.as_str() {
            "GET" => self.client.get(&url),
            "POST" => self.client.post(&url),
            "PUT" => self.client.put(&url),
            "DELETE" => self.client.delete(&url),
            "PATCH" => self.client.patch(&url),
            "HEAD" => self.client.head(&url),
            method => return Err(anyhow!("Unsupported HTTP method: {}", method)),
        };

        // Add static headers
        if let Some(headers) = &http_prov.headers {
            for (key, value) in headers {
                request_builder = request_builder.header(key, value);
            }
        }

        // Extract dynamic header_fields from remaining_args if declared
        if let Some(header_fields) = &http_prov.header_fields {
            for field in header_fields {
                if let Some(val) = remaining_args.remove(field) {
                    let header_val = match val {
                        Value::String(s) => s,
                        other => other.to_string(),
                    };
                    request_builder = request_builder.header(field, header_val);
                }
            }
        }

        if let Some(auth) = &http_prov.base.auth {
            request_builder = self.apply_auth(request_builder, auth)?;
        }

        // Determine how to send remaining args
        if method_upper == "POST" || method_upper == "PUT" || method_upper == "PATCH" {
            if let Some(body_field) = &http_prov.body_field {
                if let Some(body_val) = remaining_args.remove(body_field) {
                    // Any leftover parameters after extracting body_field are query parameters
                    for (key, value) in &remaining_args {
                        let qval = match value {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        request_builder = request_builder.query(&[(key, qval)]);
                    }
                    request_builder = request_builder.json(&body_val);
                } else {
                    request_builder = request_builder.json(&remaining_args);
                }
            } else {
                request_builder = request_builder.json(&remaining_args);
            }
        } else {
            // Send as query parameters without JSON quotes around strings
            for (key, value) in &remaining_args {
                let qval = match value {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                request_builder = request_builder.query(&[(key, qval)]);
            }
        }

        // Send request
        let response =
            crate::transports::rate_limit::reject_rate_limited(request_builder.send().await?)
                .await?;
        let status = response.status();
        let body_bytes = response.bytes().await?;
        validate_size_limit(&body_bytes, MAX_RESPONSE_SIZE)?;

        if !status.is_success() {
            let snippet = String::from_utf8_lossy(&body_bytes);
            let trimmed = snippet.trim();
            if trimmed.is_empty() {
                return Err(anyhow!("HTTP request failed with status: {}", status));
            }
            return Err(anyhow!(
                "HTTP request failed with status {}: {}",
                status,
                trimmed
            ));
        }

        if body_bytes.is_empty() {
            return Ok(Value::Null);
        }
        if let Ok(result) = serde_json::from_slice::<Value>(&body_bytes) {
            return Ok(result);
        }
        Ok(Value::String(
            String::from_utf8_lossy(&body_bytes).into_owned(),
        ))
    }

    async fn call_tool_stream(
        &self,
        _tool_name: &str,
        _args: HashMap<String, Value>,
        _prov: &dyn Provider,
    ) -> Result<Box<dyn StreamResult>> {
        Err(anyhow!("Streaming not supported by HttpClientTransport"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{ApiKeyAuth, AuthType, BasicAuth, OAuth2Auth};
    use crate::providers::base::{BaseProvider, ProviderType};
    use crate::providers::http::HttpProvider;
    use axum::{extract::Json, routing::get, routing::post, Router};
    use serde_json::json;

    #[test]
    fn apply_auth_handles_api_key_locations() {
        let transport = HttpClientTransport::new();

        // Header location
        let header_auth = AuthConfig::ApiKey(ApiKeyAuth {
            auth_type: AuthType::ApiKey,
            api_key: "secret".to_string(),
            var_name: "X-Key".to_string(),
            location: "header".to_string(),
        });
        let request = transport
            .apply_auth(
                reqwest::Client::new().get("http://example.com"),
                &header_auth,
            )
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(request.headers().get("X-Key").unwrap(), "secret");

        // Query location
        let query_auth = AuthConfig::ApiKey(ApiKeyAuth {
            auth_type: AuthType::ApiKey,
            api_key: "secret".to_string(),
            var_name: "key".to_string(),
            location: "query".to_string(),
        });
        let request = transport
            .apply_auth(
                reqwest::Client::new().get("http://example.com"),
                &query_auth,
            )
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(request.url().query(), Some("key=secret"));

        // Cookie location
        let cookie_auth = AuthConfig::ApiKey(ApiKeyAuth {
            auth_type: AuthType::ApiKey,
            api_key: "cookie-secret".to_string(),
            var_name: "session".to_string(),
            location: "cookie".to_string(),
        });
        let request = transport
            .apply_auth(
                reqwest::Client::new().get("http://example.com"),
                &cookie_auth,
            )
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(
            request.headers().get(header::COOKIE).unwrap(),
            "session=cookie-secret"
        );
    }

    #[test]
    fn apply_auth_sets_basic_auth_header() {
        let transport = HttpClientTransport::new();
        let auth = AuthConfig::Basic(BasicAuth {
            auth_type: AuthType::Basic,
            username: "user".to_string(),
            password: "pass".to_string(),
        });

        let request = transport
            .apply_auth(reqwest::Client::new().get("http://example.com"), &auth)
            .unwrap()
            .build()
            .unwrap();

        assert_eq!(
            request.headers().get(header::AUTHORIZATION).unwrap(),
            "Basic dXNlcjpwYXNz"
        );
    }

    #[test]
    fn apply_auth_rejects_unsupported_oauth2() {
        let transport = HttpClientTransport::new();
        let auth = AuthConfig::OAuth2(OAuth2Auth {
            auth_type: AuthType::OAuth2,
            token_url: "https://auth.example.com/token".to_string(),
            client_id: "client".to_string(),
            client_secret: "secret".to_string(),
            scope: None,
        });

        let err = transport
            .apply_auth(reqwest::Client::new().get("http://example.com"), &auth)
            .unwrap_err();
        assert!(err.to_string().contains("OAuth2 auth is not yet supported"));
    }

    #[tokio::test]
    async fn register_call_and_stream_error_http_transport() {
        async fn manifest_handler() -> Json<Value> {
            Json(json!({
                "tools": [{
                    "name": "greet",
                    "description": "says hello",
                    "inputs": { "type": "object" },
                    "outputs": { "type": "object" },
                    "tags": []
                }]
            }))
        }

        async fn call_handler(Json(payload): Json<Value>) -> Json<Value> {
            Json(json!({ "echo": payload }))
        }

        let app = Router::new()
            .route("/", get(manifest_handler))
            .route("/", post(call_handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let base_url = format!("http://{}", addr);
        let provider = HttpProvider {
            base: BaseProvider {
                name: "http".to_string(),
                provider_type: ProviderType::Http,
                auth: None,
                allowed_communication_protocols: None,
            },
            http_method: "POST".to_string(),
            url: base_url.clone(),
            content_type: None,
            headers: None,
            body_field: None,
            header_fields: None,
        };

        let transport = HttpClientTransport::new();
        let tools = transport
            .register_tool_provider(&provider)
            .await
            .expect("register tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "greet");

        let mut args = HashMap::new();
        args.insert("name".into(), Value::String("http".into()));
        let result = transport
            .call_tool("ignored", args.clone(), &provider)
            .await
            .expect("call tool");
        assert_eq!(result, json!({ "echo": json!(args) }));

        let err = transport
            .call_tool_stream("greet", args, &provider)
            .await
            .err()
            .expect("expected streaming error");
        assert!(err.to_string().contains("Streaming not supported"));
    }

    #[tokio::test]
    async fn call_tool_reports_rate_limit_with_host_and_wait() {
        use axum::http::StatusCode;
        use axum::response::IntoResponse;

        async fn limited() -> axum::response::Response {
            let reset = (chrono::Utc::now().timestamp() + 120).to_string();
            (
                StatusCode::FORBIDDEN,
                [
                    ("x-ratelimit-remaining", "0".to_string()),
                    ("x-ratelimit-limit", "5000".to_string()),
                    ("x-ratelimit-reset", reset),
                ],
                "{\"message\":\"API rate limit exceeded\"}",
            )
                .into_response()
        }
        async fn forbidden() -> axum::response::Response {
            (StatusCode::FORBIDDEN, "no access").into_response()
        }

        let app = Router::new()
            .route("/limited", get(limited))
            .route("/forbidden", get(forbidden));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let provider = |path: &str| HttpProvider {
            base: BaseProvider {
                name: "gh".to_string(),
                provider_type: ProviderType::Http,
                auth: None,
                allowed_communication_protocols: None,
            },
            http_method: "GET".to_string(),
            url: format!("http://{addr}{path}"),
            content_type: None,
            headers: None,
            body_field: None,
            header_fields: None,
        };
        let transport = HttpClientTransport::new();

        let err = transport
            .call_tool("gh.x", HashMap::new(), &provider("/limited"))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with("rate limited by 127.0.0.1 (HTTP 403): retry after"),
            "{err}"
        );
        assert!(err.contains("API rate limit exceeded"), "{err}");

        // A 403 without rate-limit headers stays an ordinary error.
        let err = transport
            .call_tool("gh.x", HashMap::new(), &provider("/forbidden"))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("HTTP request failed with status 403"), "{err}");
    }
}
