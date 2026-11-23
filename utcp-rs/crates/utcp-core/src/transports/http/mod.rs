use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use reqwest::{header, Client};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::auth::AuthConfig;
use crate::providers::base::Provider;
use crate::providers::http::HttpProvider;
use crate::security::{validate_size_limit, validate_url_security};
use crate::tools::Tool;
use crate::transfer::{
    content_disposition_filename, mime_essence, ByteStream, CallOutput, CallerFiles, FileDownload,
    FileKind, FileMeta, NoFiles, UploadSpec,
};
use crate::transports::{stream::StreamResult, ClientTransport};

/// Largest tool list `register_tool_provider` reads from an HTTP endpoint.
const MAX_RESPONSE_SIZE: usize = 10 * 1024 * 1024; // 10 MB

/// A call fails when the server sends nothing for this long: no answer after the request (and its
/// upload) went out, or no data in the middle of an answer. A transfer that keeps moving never
/// times out, however long it takes.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Replaces the shared client's total timeout for a tool call (the stall timeout guards it instead).
const NO_TOTAL_TIMEOUT: Duration = Duration::from_secs(365 * 24 * 60 * 60);
/// How much of a failed call's answer goes into the error.
const MAX_ERROR_BODY: usize = 64 * 1024;

/// Transport for synchronous HTTP providers that expose JSON APIs.
pub struct HttpClientTransport {
    pub client: Client,
    stall_timeout: Duration,
}

impl Default for HttpClientTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpClientTransport {
    /// Build a new HTTP client transport with tuned defaults and SSRF protection.
    pub fn new() -> Self {
        Self::with_client(crate::security::build_secure_http_client())
    }

    /// Build an HTTP client transport sharing an existing warm `reqwest::Client`.
    pub fn with_client(client: Client) -> Self {
        Self {
            client,
            stall_timeout: STALL_TIMEOUT,
        }
    }

    /// A different stall timeout (tests).
    pub fn with_stall_timeout(mut self, stall_timeout: Duration) -> Self {
        self.stall_timeout = stall_timeout;
        self
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
        tool_name: &str,
        args: HashMap<String, Value>,
        prov: &dyn Provider,
    ) -> Result<Value> {
        match self.call_tool_files(tool_name, args, prov, &NoFiles).await? {
            CallOutput::Json(value) => Ok(value),
            CallOutput::File(file) => Err(anyhow!(
                "{tool_name} answered with a file ({}), which only `utcp call` and `utcp batch` can save",
                file.meta.mime_type
            )),
        }
    }

    async fn call_tool_files(
        &self,
        tool_name: &str,
        args: HashMap<String, Value>,
        prov: &dyn Provider,
        files: &dyn CallerFiles,
    ) -> Result<CallOutput> {
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

        let activity = Activity::new();
        let parts = plan_request(tool_name, http_prov, remaining_args)?;
        if !parts.query.is_empty() {
            request_builder = request_builder.query(&parts.query);
        }
        let sets_content_type = http_prov.headers.as_ref().is_some_and(|h| {
            h.keys()
                .any(|k| k.eq_ignore_ascii_case(header::CONTENT_TYPE.as_str()))
        });
        match parts.body {
            RequestBody::None => {}
            RequestBody::Json(value) => request_builder = request_builder.json(&value),
            RequestBody::Text(text) => {
                if !sets_content_type {
                    request_builder =
                        request_builder.header(header::CONTENT_TYPE, "text/plain; charset=utf-8");
                }
                request_builder = request_builder.body(text);
            }
            RequestBody::Upload { field, spec } => {
                let spec = UploadSpec::from_value(&spec, &field)?;
                let upload = files.open_upload(&spec).await?;
                if !sets_content_type {
                    request_builder =
                        request_builder.header(header::CONTENT_TYPE, "application/octet-stream");
                }
                request_builder = request_builder
                    .header(header::CONTENT_LENGTH, upload.length)
                    .body(reqwest::Body::wrap_stream(activity.track(upload.stream)));
            }
        }

        let host = url::Url::parse(&url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| url.clone());
        let response = self
            .send_watching(request_builder.timeout(NO_TOTAL_TIMEOUT), &activity, &host)
            .await?;
        let response = crate::transports::rate_limit::reject_rate_limited(response).await?;
        let status = response.status();
        let headers = response.headers().clone();
        let size = response.content_length();
        let mut body = stall_guard(response.bytes_stream(), self.stall_timeout, host);

        if !status.is_success() {
            let mut snippet = Vec::new();
            while snippet.len() < MAX_ERROR_BODY {
                match body.next().await {
                    Some(Ok(chunk)) => snippet.extend_from_slice(&chunk),
                    _ => break,
                }
            }
            snippet.truncate(MAX_ERROR_BODY);
            let snippet = String::from_utf8_lossy(&snippet);
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

        let header_str = |name: header::HeaderName| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let content_type = header_str(header::CONTENT_TYPE);
        let disposition = header_str(header::CONTENT_DISPOSITION);
        let filename = disposition
            .as_deref()
            .and_then(content_disposition_filename);
        let attachment = disposition.as_deref().is_some_and(|d| {
            d.trim_start()
                .to_ascii_lowercase()
                .starts_with("attachment")
        });
        let forced = http_prov.response_type.as_deref() == Some("file") || attachment;
        let meta = |mime: Option<&str>, size: Option<u64>| FileMeta {
            kind: FileKind::Download,
            mime_type: mime
                .and_then(mime_essence)
                .unwrap_or_else(|| "application/octet-stream".into()),
            filename: filename.clone(),
            size,
            tool: tool_name.to_string(),
        };

        let kind = classify(content_type.as_deref(), forced);
        if kind == AnswerKind::File {
            return Ok(CallOutput::File(FileDownload {
                meta: meta(content_type.as_deref(), size),
                stream: body,
            }));
        }
        let mut body_bytes = Vec::new();
        while let Some(chunk) = body.next().await {
            body_bytes.extend_from_slice(&chunk?);
        }
        if body_bytes.is_empty() {
            return Ok(CallOutput::Json(Value::Null));
        }
        if kind != AnswerKind::Text {
            if let Ok(result) = serde_json::from_slice::<Value>(&body_bytes) {
                return Ok(CallOutput::Json(result));
            }
        }
        match String::from_utf8(body_bytes) {
            Ok(text) => Ok(CallOutput::Json(Value::String(text))),
            // Not text after all: keep the bytes as they are instead of mangling them.
            Err(e) => {
                let bytes = e.into_bytes();
                let size = Some(bytes.len() as u64);
                Ok(CallOutput::File(FileDownload::from_bytes(
                    meta(content_type.as_deref(), size),
                    bytes,
                )))
            }
        }
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

impl HttpClientTransport {
    /// Send the request and wait for the answer's headers, failing only when nothing moved for the
    /// stall timeout: an upload that keeps sending, or a server that answers, never times out.
    async fn send_watching(
        &self,
        builder: reqwest::RequestBuilder,
        activity: &Activity,
        host: &str,
    ) -> Result<reqwest::Response> {
        let send = builder.send();
        tokio::pin!(send);
        loop {
            let idle = activity.idle();
            if idle >= self.stall_timeout {
                return Err(anyhow!(stall_message(host, self.stall_timeout, true)));
            }
            tokio::select! {
                sent = &mut send => return Ok(sent?),
                _ = tokio::time::sleep(self.stall_timeout - idle) => {}
            }
        }
    }
}

fn query_value(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// What an HTTP call sends besides its URL and headers.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestParts {
    /// Query parameters, sorted by name; strings are sent without JSON quotes.
    pub query: Vec<(String, String)>,
    pub body: RequestBody,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RequestBody {
    None,
    Json(Value),
    /// `"body_type": "text"`: the string is the whole body, sent as is.
    Text(String),
    /// `"body_type": "file"`: the `{"file": "<path>", ...}` argument named `field`.
    Upload {
        field: String,
        spec: Value,
    },
}

/// Split a call's arguments (path parameters and `header_fields` already taken out) into query
/// parameters and a body, following the tool's template:
///
/// - POST, PUT and PATCH: the `body_field` argument is the body and the other arguments are query
///   parameters; without `body_field` (or without that argument) all arguments form a JSON object body.
/// - DELETE: no body, all arguments are query parameters, unless the template sets `body_field` and
///   the call passes that argument: then it is the body, as for POST.
/// - GET and HEAD: no body.
/// - `body_type` `"text"` sends the `body_field` argument (a string) as the raw body; `"file"` uploads
///   the file it names (POST, PUT or PATCH only). Default and `"json"`: a JSON body.
pub fn plan_request(
    tool_name: &str,
    prov: &HttpProvider,
    mut args: HashMap<String, Value>,
) -> Result<RequestParts> {
    let method = prov.http_method.to_uppercase();
    let always_body = matches!(method.as_str(), "POST" | "PUT" | "PATCH");
    let field = prov.body_field.as_deref().unwrap_or("body").to_string();
    let query = |args: HashMap<String, Value>| {
        let mut q: Vec<(String, String)> = args
            .into_iter()
            .map(|(k, v)| {
                let v = query_value(&v);
                (k, v)
            })
            .collect();
        q.sort();
        q
    };
    match prov.body_type.as_deref().unwrap_or("json") {
        "file" => {
            if !always_body {
                return Err(anyhow!(
                    "tool `{tool_name}` is broken: its template uploads a file (`\"body_type\": \"file\"`) \
                     with {method}, which sends no body; an upload needs POST, PUT or PATCH. Fix the manual."
                ));
            }
            let spec = args.remove(&field).ok_or_else(|| {
                anyhow!("this tool uploads a file: pass it as `{field}`: {{\"file\": \"<path>\"}}")
            })?;
            Ok(RequestParts {
                query: query(args),
                body: RequestBody::Upload { field, spec },
            })
        }
        "text" => {
            if !always_body && method != "DELETE" {
                return Err(anyhow!(
                    "tool `{tool_name}` is broken: its template sends a text body (`\"body_type\": \"text\"`) \
                     with {method}, which sends no body; a body needs POST, PUT, PATCH or DELETE. Fix the manual."
                ));
            }
            match args.remove(&field) {
                Some(Value::String(text)) => Ok(RequestParts {
                    query: query(args),
                    body: RequestBody::Text(text),
                }),
                _ => Err(anyhow!(
                    "this tool sends `{field}` as the raw request body: pass it as a string"
                )),
            }
        }
        "json" => {
            if always_body || method == "DELETE" {
                if let Some(name) = &prov.body_field {
                    if let Some(body) = args.remove(name) {
                        return Ok(RequestParts {
                            query: query(args),
                            body: RequestBody::Json(body),
                        });
                    }
                }
            }
            if always_body {
                Ok(RequestParts {
                    query: Vec::new(),
                    body: RequestBody::Json(Value::Object(args.into_iter().collect())),
                })
            } else {
                Ok(RequestParts {
                    query: query(args),
                    body: RequestBody::None,
                })
            }
        }
        other => Err(anyhow!(
            "tool `{tool_name}` is broken: its template has `\"body_type\": \"{other}\"`, which utcp does \
             not know (use \"json\", \"text\" or \"file\"). Fix the manual."
        )),
    }
}

fn stall_message(host: &str, stall: Duration, waiting_for_answer: bool) -> String {
    let secs = stall.as_secs();
    if waiting_for_answer {
        format!(
            "{host} did not answer for {secs} s, so utcp stopped waiting (the connection stalled). \
             A request that changes something may still have been carried out; check before retrying."
        )
    } else {
        format!(
            "{host} sent no data for {secs} s in the middle of its answer, so utcp stopped waiting \
             (the connection stalled). Nothing partial was kept."
        )
    }
}

/// When the request last made progress (an upload chunk went out).
struct Activity {
    start: Instant,
    last_ms: Arc<AtomicU64>,
}

impl Activity {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            last_ms: Arc::new(AtomicU64::new(0)),
        }
    }

    fn idle(&self) -> Duration {
        let last = Duration::from_millis(self.last_ms.load(Ordering::Relaxed));
        self.start.elapsed().saturating_sub(last)
    }

    /// `stream`, noting the time of every chunk taken from it.
    fn track(&self, stream: ByteStream) -> ByteStream {
        let start = self.start;
        let last_ms = self.last_ms.clone();
        Box::pin(stream.inspect(move |_| {
            last_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
        }))
    }
}

/// An answer's body, failing with a clear error when no data arrives for `stall`.
fn stall_guard<S>(stream: S, stall: Duration, host: String) -> ByteStream
where
    S: Stream<Item = reqwest::Result<Bytes>> + Send + 'static,
{
    let state = Some(Box::pin(stream));
    Box::pin(futures::stream::unfold(state, move |state| {
        let host = host.clone();
        async move {
            let mut stream = state?;
            match tokio::time::timeout(stall, stream.next()).await {
                Ok(Some(Ok(chunk))) => Some((Ok(chunk), Some(stream))),
                Ok(Some(Err(e))) => Some((Err(std::io::Error::other(e)), None)),
                Ok(None) => None,
                Err(_) => Some((
                    Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        stall_message(&host, stall, false),
                    )),
                    None,
                )),
            }
        }
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnswerKind {
    Json,
    Text,
    /// No `Content-Type`: read it, then JSON if it parses, text if it is UTF-8, else a file.
    Unknown,
    File,
}

/// How to treat an answer, from its `Content-Type`.
fn classify(content_type: Option<&str>, forced_file: bool) -> AnswerKind {
    if forced_file {
        return AnswerKind::File;
    }
    let Some(ct) = content_type else {
        return AnswerKind::Unknown;
    };
    let essence = ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if essence.is_empty() {
        AnswerKind::Unknown
    } else if essence == "application/json" || essence.ends_with("+json") || essence == "text/json"
    {
        AnswerKind::Json
    } else if essence.starts_with("text/")
        || essence.ends_with("+xml")
        || matches!(
            essence.as_str(),
            "application/xml"
                | "application/yaml"
                | "application/x-yaml"
                | "application/javascript"
                | "application/x-www-form-urlencoded"
        )
    {
        AnswerKind::Text
    } else {
        AnswerKind::File
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
            body_type: None,
            response_type: None,
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
            body_type: None,
            response_type: None,
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

    #[test]
    fn answers_are_json_text_or_files_by_content_type() {
        assert_eq!(
            classify(Some("application/json; charset=utf-8"), false),
            AnswerKind::Json
        );
        assert_eq!(
            classify(Some("application/vnd.api+json"), false),
            AnswerKind::Json
        );
        assert_eq!(classify(Some("text/plain"), false), AnswerKind::Text);
        assert_eq!(classify(Some("application/xml"), false), AnswerKind::Text);
        assert_eq!(classify(Some("application/zip"), false), AnswerKind::File);
        assert_eq!(
            classify(Some("application/octet-stream"), false),
            AnswerKind::File
        );
        assert_eq!(classify(None, false), AnswerKind::Unknown);
        assert_eq!(classify(Some("application/json"), true), AnswerKind::File);
    }

    fn provider_at(url: String) -> HttpProvider {
        let mut p = HttpProvider::new("t".into(), url, "GET".into(), None);
        p.content_type = None;
        p
    }

    async fn serve(app: Router) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    /// A transfer that keeps moving outlasts the stall timeout; one that stops fails with a clear
    /// message, both before the answer starts and in its middle. Binary without a type is a file.
    #[tokio::test]
    async fn the_stall_timeout_stops_only_transfers_that_stop_moving() {
        use axum::body::Body;
        let slow_body = || async {
            let chunks = futures::stream::unfold(0u32, |n| async move {
                if n == 6 {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                Some((
                    Ok::<_, std::io::Error>(Bytes::from(vec![0xffu8; 10])),
                    n + 1,
                ))
            });
            Body::from_stream(chunks)
        };
        let stalled_body = || async {
            let chunks = futures::stream::unfold(0u32, |n| async move {
                if n > 0 {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
                Some((Ok::<_, std::io::Error>(Bytes::from_static(b"x")), n + 1))
            });
            (
                [("content-type", "application/octet-stream")],
                Body::from_stream(chunks),
            )
        };
        let silent = || async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            "late"
        };
        let addr = serve(
            Router::new()
                .route("/slow", get(slow_body))
                .route("/stalled", get(stalled_body))
                .route("/silent", get(silent)),
        )
        .await;
        let transport = HttpClientTransport::new().with_stall_timeout(Duration::from_millis(300));

        // 600 ms in all, never 300 ms without data; no type and not UTF-8, so a file.
        let out = transport
            .call_tool_files(
                "t.slow",
                HashMap::new(),
                &provider_at(format!("http://{addr}/slow")),
                &NoFiles,
            )
            .await
            .unwrap();
        let CallOutput::File(mut file) = out else {
            panic!("expected a file")
        };
        assert_eq!(file.meta.mime_type, "application/octet-stream");
        let mut got = 0;
        while let Some(chunk) = file.stream.next().await {
            got += chunk.unwrap().len();
        }
        assert_eq!(got, 60);

        let out = transport
            .call_tool_files(
                "t.stalled",
                HashMap::new(),
                &provider_at(format!("http://{addr}/stalled")),
                &NoFiles,
            )
            .await
            .unwrap();
        let CallOutput::File(mut file) = out else {
            panic!("expected a file")
        };
        assert!(file.stream.next().await.unwrap().is_ok());
        let err = file.stream.next().await.unwrap().unwrap_err().to_string();
        assert!(err.contains("sent no data for 0 s in the middle"), "{err}");
        assert!(
            file.stream.next().await.is_none(),
            "a stall ends the stream"
        );

        let err = transport
            .call_tool(
                "t.silent",
                HashMap::new(),
                &provider_at(format!("http://{addr}/silent")),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("did not answer"), "{err}");
        assert!(err.contains("may still have been carried out"), "{err}");
    }

    /// Upload chunks count as progress: an upload longer than the stall timeout succeeds.
    #[tokio::test]
    async fn a_slow_upload_is_progress_and_goes_out_with_its_length() {
        use crate::transfer::{Upload, UploadSpec};
        struct SlowFile;
        #[async_trait]
        impl CallerFiles for SlowFile {
            async fn open_upload(&self, spec: &UploadSpec) -> Result<Upload> {
                assert_eq!(spec.file, "a.bin");
                let chunks = futures::stream::unfold(0u32, |n| async move {
                    if n == 5 {
                        return None;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Some((Ok(Bytes::from(vec![7u8; 4])), n + 1))
                });
                Ok(Upload {
                    length: 20,
                    stream: Box::pin(chunks),
                })
            }
        }
        let echo = |headers: axum::http::HeaderMap, body: axum::body::Bytes| async move {
            Json(json!({
                "len": body.len(),
                "type": headers.get("content-type").and_then(|v| v.to_str().ok()),
                "length": headers.get("content-length").and_then(|v| v.to_str().ok()),
            }))
        };
        let addr = serve(Router::new().route("/up", post(echo))).await;
        let mut prov = provider_at(format!("http://{addr}/up"));
        prov.http_method = "POST".into();
        prov.body_field = Some("body".into());
        prov.body_type = Some("file".into());
        let transport = HttpClientTransport::new().with_stall_timeout(Duration::from_millis(300));
        let mut args = HashMap::new();
        args.insert("body".to_string(), json!({"file": "a.bin"}));
        let out = transport
            .call_tool_files("t.up", args, &prov, &SlowFile)
            .await
            .unwrap();
        let CallOutput::Json(v) = out else {
            panic!("expected JSON")
        };
        assert_eq!(
            v,
            json!({"len": 20, "type": "application/octet-stream", "length": "20"})
        );

        // Without a file argument the error says how to pass one.
        let err = transport
            .call_tool_files("t.up", HashMap::new(), &prov, &SlowFile)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("{\"file\": \"<path>\"}"), "{err}");
    }

    /// A request without a body cannot carry an upload: the call fails instead of dropping the file.
    #[tokio::test]
    async fn an_upload_on_a_method_without_a_body_is_an_error() {
        let transport = HttpClientTransport::new();
        for method in ["GET", "DELETE", "HEAD"] {
            // Nothing listens there: the call must fail before it sends anything.
            let mut prov = provider_at("http://127.0.0.1:9/up".to_string());
            prov.http_method = method.into();
            prov.body_type = Some("file".into());
            let mut args = HashMap::new();
            args.insert("body".to_string(), json!({"file": "a.bin"}));
            let err = transport
                .call_tool_files("t.up", args, &prov, &NoFiles)
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("POST, PUT or PATCH"), "{method}: {err}");
            assert!(err.contains(method), "{err}");
        }
    }

    fn args(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn q(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Where each argument goes, per method, `body_field` and `body_type`.
    #[test]
    fn arguments_go_to_the_query_or_the_body_by_method_and_template() {
        let plan = |method: &str, field: Option<&str>, kind: Option<&str>, a| {
            let mut p = provider_at("http://h/x".into());
            p.http_method = method.into();
            p.body_field = field.map(str::to_string);
            p.body_type = kind.map(str::to_string);
            plan_request("t.x", &p, a)
        };
        let body = json!({"message": "m", "sha": "s"});

        // POST: the body_field argument is the body, the rest is query; else one JSON object.
        let p = plan(
            "POST",
            Some("body"),
            None,
            args(&[("body", body.clone()), ("v", json!(1))]),
        )
        .unwrap();
        assert_eq!(p.query, q(&[("v", "1")]));
        assert_eq!(p.body, RequestBody::Json(body.clone()));
        let p = plan("POST", None, None, args(&[("a", json!("x"))])).unwrap();
        assert_eq!(p.query, q(&[]));
        assert_eq!(p.body, RequestBody::Json(json!({"a": "x"})));

        // DELETE with body_field and the argument: a JSON body, the rest stays query.
        let p = plan(
            "DELETE",
            Some("body"),
            None,
            args(&[("body", body.clone()), ("v", json!("7.1"))]),
        )
        .unwrap();
        assert_eq!(p.query, q(&[("v", "7.1")]));
        assert_eq!(p.body, RequestBody::Json(body.clone()));
        // DELETE without body_field, or without the argument: no body, as before.
        for (field, a) in [
            (None, args(&[("body", json!("b")), ("v", json!(1))])),
            (Some("body"), args(&[("v", json!(1))])),
        ] {
            let p = plan("DELETE", field, None, a.clone()).unwrap();
            assert_eq!(p.body, RequestBody::None, "{field:?}");
            assert_eq!(p.query.len(), a.len());
        }
        // GET never has a body, even with body_field.
        let p = plan("GET", Some("body"), None, args(&[("body", json!("b"))])).unwrap();
        assert_eq!(p.body, RequestBody::None);
        assert_eq!(p.query, q(&[("body", "b")]));

        // Text: the string as is, on POST and DELETE; not on GET; not a non-string.
        for method in ["POST", "PUT", "PATCH", "DELETE"] {
            let p = plan(
                method,
                Some("text"),
                Some("text"),
                args(&[("text", json!("# Hi")), ("v", json!(1))]),
            )
            .unwrap();
            assert_eq!(p.body, RequestBody::Text("# Hi".into()), "{method}");
            assert_eq!(p.query, q(&[("v", "1")]));
        }
        let err = plan("GET", None, Some("text"), args(&[("body", json!("x"))]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("POST, PUT, PATCH or DELETE"), "{err}");
        for a in [args(&[]), args(&[("body", json!({"a": 1}))])] {
            let err = plan("POST", None, Some("text"), a).unwrap_err().to_string();
            assert!(err.contains("pass it as a string"), "{err}");
        }

        // Upload stays POST, PUT or PATCH only; an unknown body_type is a broken manual.
        let err = plan(
            "DELETE",
            None,
            Some("file"),
            args(&[("body", json!({"file": "a"}))]),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("POST, PUT or PATCH"), "{err}");
        let p = plan("POST", None, Some("json"), args(&[("a", json!(1))])).unwrap();
        assert_eq!(p.body, RequestBody::Json(json!({"a": 1})));
        let err = plan("POST", None, Some("xml"), args(&[]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("\"body_type\": \"xml\""), "{err}");
    }

    /// What the server receives: a JSON body on DELETE, and a raw text body with the template's
    /// `Content-Type` or `text/plain`.
    #[tokio::test]
    async fn delete_bodies_and_text_bodies_reach_the_server() {
        use axum::routing::delete;
        let echo = |method: axum::http::Method,
                    uri: axum::http::Uri,
                    headers: axum::http::HeaderMap,
                    body: axum::body::Bytes| async move {
            Json(json!({
                "method": method.as_str(),
                "query": uri.query(),
                "type": headers.get("content-type").and_then(|v| v.to_str().ok()),
                "body": String::from_utf8_lossy(&body),
            }))
        };
        let addr = serve(
            Router::new()
                .route("/contents/:path", delete(echo))
                .route("/raw", post(echo)),
        )
        .await;
        let transport = HttpClientTransport::new();

        let mut del = provider_at(format!("http://{addr}/contents/{{path}}"));
        del.http_method = "DELETE".into();
        del.body_field = Some("body".into());
        let a = args(&[
            ("path", json!("README.md")),
            ("body", json!({"message": "m", "sha": "s"})),
            ("v", json!(2)),
        ]);
        let v = transport.call_tool("t.del", a, &del).await.unwrap();
        assert_eq!(v["method"], "DELETE");
        assert_eq!(v["query"], "v=2");
        assert_eq!(v["type"], "application/json");
        let sent: Value = serde_json::from_str(v["body"].as_str().unwrap()).unwrap();
        assert_eq!(sent, json!({"message": "m", "sha": "s"}));

        let mut raw = provider_at(format!("http://{addr}/raw"));
        raw.http_method = "POST".into();
        raw.body_field = Some("text".into());
        raw.body_type = Some("text".into());
        let v = transport
            .call_tool("t.raw", args(&[("text", json!("# \"Hi\""))]), &raw)
            .await
            .unwrap();
        assert_eq!(v["body"], "# \"Hi\"", "sent as is, not as a JSON string");
        assert_eq!(v["type"], "text/plain; charset=utf-8");
        assert_eq!(v["query"], Value::Null);
        raw.headers = Some(HashMap::from([(
            "Content-Type".to_string(),
            "text/x-markdown".to_string(),
        )]));
        let v = transport
            .call_tool("t.raw", args(&[("text", json!("x"))]), &raw)
            .await
            .unwrap();
        assert_eq!(v["type"], "text/x-markdown");
    }
}
