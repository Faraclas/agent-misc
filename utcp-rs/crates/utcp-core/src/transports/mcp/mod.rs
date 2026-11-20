// MCP (Model Context Protocol) Transport
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use reqwest::{header, Client};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;

mod process_tree;
pub use process_tree::wait_for_stopping_servers;
use process_tree::ServerProcess;

use crate::auth::AuthConfig;
use crate::providers::base::Provider;
use crate::providers::mcp::McpProvider;
use crate::security::{validate_size_limit, validate_url_security};
use crate::tools::Tool;
use crate::transports::{stream::StreamResult, ClientTransport, McpServerState, McpServerStatus};

const MAX_RESPONSE_SIZE: usize = 10 * 1024 * 1024; // 10 MB

/// How long one stdio MCP request may wait for its answer. Matches the HTTP client timeout and
/// `catalog::MCP_DISCOVERY_TIMEOUT`, so a first-run `npx`/`uvx` download still fits.
const STDIO_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

type PendingMap = Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;
type SharedWriter = Arc<Mutex<Box<dyn AsyncWrite + Send + Unpin>>>;

/// One provider's stdio server. The lock is held while the server starts, so concurrent first
/// calls share one start-up.
#[derive(Default)]
struct StdioSlot {
    process: Mutex<Option<Arc<McpStdioProcess>>>,
    /// Successful starts, for the restart count.
    starts: AtomicU64,
    /// A start was attempted (it may have failed).
    attempted: AtomicBool,
    /// Stopped on request (`utcp daemon stop-server`); cleared by the next start.
    stopped: AtomicBool,
}

/// What a stdio server was started with. A provider whose command, arguments or (resolved)
/// environment differ, for example after re-registration or a rotated token, needs a new server.
#[derive(Debug, Clone, PartialEq)]
struct StdioLaunch {
    command: String,
    args: Option<Vec<String>>,
    env_vars: Option<HashMap<String, String>>,
}

impl StdioLaunch {
    fn of(prov: &McpProvider) -> Option<Self> {
        Some(Self {
            command: prov.command.clone()?,
            args: prov.args.clone(),
            env_vars: prov.env_vars.clone(),
        })
    }
}

/// One running stdio MCP server, shared by every concurrent call to it.
///
/// Requests are written under a short writer lock; a single reader task reads the server's
/// output and hands each answer to the caller waiting on that JSON-RPC `id`. Calls therefore
/// run concurrently, answers may arrive in any order, and each caller has its own timeout.
/// When the server exits, every waiting call fails at once and the process is marked dead so
/// the next call starts a fresh one.
struct McpStdioProcess {
    name: String,
    // Stops the server and everything it started when dropped: with the last handle, or
    // earlier through [`Self::stop`].
    server: std::sync::Mutex<Option<ServerProcess>>,
    pid: Option<u32>,
    started_at: Instant,
    launch: Option<StdioLaunch>,
    writer: SharedWriter,
    pending: PendingMap,
    next_id: AtomicU64,
    alive: Arc<AtomicBool>,
    reader: JoinHandle<()>,
    request_timeout: Duration,
}

impl Drop for McpStdioProcess {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// Removes a request's pending entry however the call ends (answer, timeout, write error, or
/// the caller's future being dropped), so abandoned ids never accumulate.
struct PendingGuard<'a> {
    pending: &'a PendingMap,
    id: u64,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut map) = self.pending.lock() {
            map.remove(&self.id);
        }
    }
}

async fn write_line(writer: &SharedWriter, line: &str) -> std::io::Result<()> {
    let mut w = writer.lock().await;
    w.write_all(line.as_bytes()).await?;
    w.write_all(b"\n").await?;
    w.flush().await
}

/// Reads the server's output until it closes, routing each response to its waiting caller.
async fn stdio_read_loop<R: AsyncRead + Unpin>(
    name: String,
    reader: R,
    writer: SharedWriter,
    pending: PendingMap,
    alive: Arc<AtomicBool>,
) {
    let mut reader = BufReader::with_capacity(65536, reader);
    let mut line = String::new();
    let reason = loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break format!("MCP server '{name}' closed its output (did it exit?)"),
            Ok(_) => {}
            Err(e) => break format!("failed to read output of MCP server '{name}': {e}"),
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Servers may print non-JSON chatter on stdout; skip it.
        let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        // Notifications carry no id; nothing waits for them.
        let Some(id_val) = msg.get("id").cloned() else {
            continue;
        };
        if let Some(method) = msg.get("method").and_then(Value::as_str) {
            // A request from the server to us. Answer so the server is never left waiting:
            // `ping` is required by the spec; the optional client features are not offered.
            let reply = if method == "ping" {
                serde_json::json!({"jsonrpc": "2.0", "id": id_val, "result": {}})
            } else {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id_val,
                    "error": {"code": -32601, "message": format!("method not supported by utcp: {method}")}
                })
            };
            let _ = write_line(&writer, &reply.to_string()).await;
            continue;
        }
        let Some(id) = id_val.as_u64() else {
            continue;
        };
        let Some(tx) = pending.lock().ok().and_then(|mut map| map.remove(&id)) else {
            continue; // the caller already gave up (timeout or cancelled)
        };
        let outcome = if let Err(e) = validate_size_limit(trimmed.as_bytes(), MAX_RESPONSE_SIZE) {
            Err(e)
        } else if let Some(error) = msg.get("error") {
            Err(anyhow!("MCP error: {}", error))
        } else {
            msg.get("result")
                .cloned()
                .ok_or_else(|| anyhow!("No result in MCP response"))
        };
        let _ = tx.send(outcome);
    };
    // Mark dead before draining: a caller that registers after the drain sees `alive == false`.
    alive.store(false, Ordering::SeqCst);
    fail_pending(&pending, &reason);
}

/// Fail every waiting call with `reason`. Call after marking the server dead.
fn fail_pending(pending: &PendingMap, reason: &str) {
    let drained: Vec<_> = match pending.lock() {
        Ok(mut map) => map.drain().collect(),
        Err(_) => Vec::new(),
    };
    for (_, tx) in drained {
        let _ = tx.send(Err(anyhow!("{reason}")));
    }
}

impl McpStdioProcess {
    /// Wrap already-open streams (a child's stdin/stdout, or in-memory pipes in tests) and start
    /// the reader task. Does not perform the MCP handshake; call [`Self::initialize`].
    fn from_streams<W, R>(
        name: &str,
        writer: W,
        reader: R,
        server: Option<ServerProcess>,
        request_timeout: Duration,
    ) -> Self
    where
        W: AsyncWrite + Send + Unpin + 'static,
        R: AsyncRead + Send + Unpin + 'static,
    {
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(writer)));
        let pending: PendingMap = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let reader = tokio::spawn(stdio_read_loop(
            name.to_string(),
            reader,
            Arc::clone(&writer),
            Arc::clone(&pending),
            Arc::clone(&alive),
        ));
        Self {
            name: name.to_string(),
            server: std::sync::Mutex::new(server),
            pid: None,
            started_at: Instant::now(),
            launch: None,
            writer,
            pending,
            next_id: AtomicU64::new(1),
            alive,
            reader,
            request_timeout,
        }
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Stop the server now, without waiting for calls that still hold this handle: they fail
    /// at once, and the next call through the transport starts a new server.
    fn stop(&self) {
        self.alive.store(false, Ordering::SeqCst);
        self.reader.abort();
        fail_pending(
            &self.pending,
            &format!(
                "MCP server '{}' was stopped on request (utcp daemon stop-server or restart-server)",
                self.name
            ),
        );
        let server = self.server.lock().ok().and_then(|mut s| s.take());
        drop(server);
    }

    /// Standard MCP 2024-11-05 `initialize` + `notifications/initialized` handshake.
    async fn initialize(&self) -> Result<()> {
        let init_params = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "utcp-rs",
                "version": env!("CARGO_PKG_VERSION")
            }
        });
        self.send_request("initialize", init_params).await?;
        self.send_notification("notifications/initialized", serde_json::json!({}))
            .await
    }

    async fn spawn(name: &str, launch: StdioLaunch, request_timeout: Duration) -> Result<Self> {
        let StdioLaunch {
            command,
            args,
            env_vars,
        } = &launch;
        // Security: no allowlist on purpose. The user chose this command at `import-mcp` time;
        // the check still refuses shell metacharacters.
        crate::security::validate_command(command, &[])?;

        // Security: Validate arguments
        if let Some(args_vec) = args {
            crate::security::validate_command_args(args_vec)?;
        }

        let mut cmd = Command::new(crate::program::resolve_with_env(
            std::path::Path::new(command),
            env_vars.as_ref(),
        ));

        if let Some(args_vec) = args {
            cmd.args(args_vec);
        }

        if let Some(env) = env_vars {
            for (k, v) in env {
                cmd.env(k, v);
            }
        }

        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::null());
        process_tree::prepare(&mut cmd);

        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow!("failed to start MCP server command '{}': {}", command, e))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Failed to get stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to get stdout"))?;
        let pid = child.id();

        let mut proc = Self::from_streams(
            name,
            stdin,
            stdout,
            Some(ServerProcess::new(child)),
            request_timeout,
        );
        proc.launch = Some(launch);
        proc.pid = pid;
        proc.initialize().await?;
        Ok(proc)
    }

    async fn send_notification(&self, method: &str, params: Value) -> Result<()> {
        let notif = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        write_line(&self.writer, &notif.to_string())
            .await
            .map_err(|e| {
                self.alive.store(false, Ordering::SeqCst);
                anyhow!(
                    "failed to send '{method}' to MCP server '{}': {e}",
                    self.name
                )
            })
    }

    async fn send_request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .map_err(|_| anyhow!("MCP request table poisoned"))?
            .insert(id, tx);
        let _guard = PendingGuard {
            pending: &self.pending,
            id,
        };
        // Checked after registering: if the reader already drained, nobody would answer us.
        if !self.is_alive() {
            return Err(anyhow!("MCP server '{}' is not running", self.name));
        }

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": id,
        });
        if let Err(e) = write_line(&self.writer, &request.to_string()).await {
            self.alive.store(false, Ordering::SeqCst);
            return Err(anyhow!(
                "failed to send '{method}' to MCP server '{}': {e}",
                self.name
            ));
        }

        match tokio::time::timeout(self.request_timeout, rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => Err(anyhow!("MCP server '{}' closed connection", self.name)),
            Err(_) => Err(anyhow!(
                "MCP server '{}' did not answer '{method}' within {}s",
                self.name,
                self.request_timeout.as_secs_f32()
            )),
        }
    }
}

/// Transport for MCP providers over HTTP (2024-11-05 & 2025-03-26 Streamable HTTP) or stdio.
pub struct McpTransport {
    client: Client,
    // Map of provider name to its stdio server slot. Each provider has its own slot so a slow
    // server start (e.g. an `npx` download) never blocks calls to other servers.
    stdio_processes: Arc<Mutex<HashMap<String, Arc<StdioSlot>>>>,
    // Map of HTTP MCP URL to negotiated `Mcp-Session-Id` (None when server is stateless)
    http_sessions: Arc<Mutex<HashMap<String, Option<String>>>>,
    // JSON-RPC ids for HTTP requests; unique per transport so concurrent calls never share one.
    next_http_id: AtomicU64,
    // How long one stdio request waits for its answer (`STDIO_REQUEST_TIMEOUT`; shorter in tests).
    stdio_request_timeout: Duration,
}

impl Default for McpTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl McpTransport {
    /// Create an MCP transport with a secure reqwest client.
    pub fn new() -> Self {
        Self::with_client(crate::security::build_secure_http_client())
    }

    /// Create an MCP transport sharing an existing warm `reqwest::Client`.
    pub fn with_client(client: Client) -> Self {
        Self {
            client,
            stdio_processes: Arc::new(Mutex::new(HashMap::new())),
            http_sessions: Arc::new(Mutex::new(HashMap::new())),
            next_http_id: AtomicU64::new(1),
            stdio_request_timeout: STDIO_REQUEST_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_stdio_request_timeout(mut self, timeout: Duration) -> Self {
        self.stdio_request_timeout = timeout;
        self
    }

    fn next_http_id(&self) -> u64 {
        self.next_http_id.fetch_add(1, Ordering::Relaxed)
    }

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
                "OAuth2 auth is not yet supported by the MCP transport"
            )),
        }
    }

    fn parse_mcp_http_body(body_bytes: &[u8]) -> Result<Value> {
        if let Ok(json_val) = serde_json::from_slice::<Value>(body_bytes) {
            return Ok(json_val);
        }

        // MCP 2025-03-26 Streamable HTTP servers may return SSE-framed responses (`event: message\ndata: {...}`)
        let text = String::from_utf8_lossy(body_bytes);
        let mut last_parsed: Option<Value> = None;
        for line in text.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("data:") {
                let data_str = rest.trim();
                if data_str.is_empty() {
                    continue;
                }
                if let Ok(val) = serde_json::from_str::<Value>(data_str) {
                    if val.get("result").is_some() || val.get("error").is_some() {
                        return Ok(val);
                    }
                    last_parsed = Some(val);
                }
            }
        }

        last_parsed.ok_or_else(|| anyhow!("Failed to parse HTTP MCP response as JSON or SSE frame"))
    }

    fn build_http_post(
        &self,
        prov: &McpProvider,
        url: &str,
        body: &Value,
        session_id: Option<&str>,
    ) -> Result<reqwest::RequestBuilder> {
        let mut req = self
            .client
            .post(url)
            .header(header::ACCEPT, "application/json, text/event-stream")
            .json(body);

        if let Some(sid) = session_id {
            req = req.header("Mcp-Session-Id", sid);
        }
        if let Some(headers) = &prov.headers {
            for (k, v) in headers {
                req = req.header(k, v);
            }
        }
        if let Some(auth) = &prov.base.auth {
            req = self.apply_auth(req, auth)?;
        }
        Ok(req)
    }

    async fn ensure_http_initialized(
        &self,
        prov: &McpProvider,
        url: &str,
    ) -> Result<Option<String>> {
        {
            let guard = self.http_sessions.lock().await;
            if let Some(existing) = guard.get(url) {
                return Ok(existing.clone());
            }
        }

        let init_req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {
                    "name": "utcp-rs",
                    "version": env!("CARGO_PKG_VERSION")
                }
            },
            "id": self.next_http_id()
        });

        let resp = self
            .build_http_post(prov, url, &init_req, None)?
            .send()
            .await?;
        let resp = crate::transports::rate_limit::reject_rate_limited(resp).await?;

        if !resp.status().is_success() {
            return Err(anyhow!("MCP HTTP initialize failed: {}", resp.status()));
        }

        let session_id = resp
            .headers()
            .get("Mcp-Session-Id")
            .or_else(|| resp.headers().get("mcp-session-id"))
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        let body_bytes = resp.bytes().await?;
        validate_size_limit(&body_bytes, MAX_RESPONSE_SIZE)?;
        if !body_bytes.is_empty() {
            let parsed = Self::parse_mcp_http_body(&body_bytes)?;
            if let Some(err) = parsed.get("error") {
                return Err(anyhow!("MCP initialize error: {}", err));
            }
        }

        // Send `notifications/initialized` (compliant servers return 202 Accepted or 200 OK)
        let notif = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        });
        let _ = self
            .build_http_post(prov, url, &notif, session_id.as_deref())?
            .send()
            .await;

        let mut guard = self.http_sessions.lock().await;
        guard.insert(url.to_string(), session_id.clone());
        Ok(session_id)
    }

    async fn mcp_http_request(
        &self,
        prov: &McpProvider,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let url = prov
            .url
            .as_ref()
            .ok_or_else(|| anyhow!("No URL provided for HTTP MCP provider"))?;

        validate_url_security(url, false)?;

        let mut session_id = self.ensure_http_initialized(prov, url).await?;

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": self.next_http_id(),
        });

        let mut response = self
            .build_http_post(prov, url, &request, session_id.as_deref())?
            .send()
            .await?;

        // Per MCP 2025-03-26, HTTP 404 with a session header indicates session expiry -> re-initialize once
        if response.status() == reqwest::StatusCode::NOT_FOUND && session_id.is_some() {
            {
                let mut guard = self.http_sessions.lock().await;
                guard.remove(url);
            }
            session_id = self.ensure_http_initialized(prov, url).await?;
            response = self
                .build_http_post(prov, url, &request, session_id.as_deref())?
                .send()
                .await?;
        }
        let response = crate::transports::rate_limit::reject_rate_limited(response).await?;

        if !response.status().is_success() {
            return Err(anyhow!("MCP request failed: {}", response.status()));
        }

        let body_bytes = response.bytes().await?;
        validate_size_limit(&body_bytes, MAX_RESPONSE_SIZE)?;
        let result = Self::parse_mcp_http_body(&body_bytes)?;

        if let Some(error) = result.get("error") {
            return Err(anyhow!("MCP error: {}", error));
        }

        result
            .get("result")
            .cloned()
            .ok_or_else(|| anyhow!("No result in MCP response"))
    }

    async fn stdio_slot(&self, name: &str) -> Arc<StdioSlot> {
        let mut processes = self.stdio_processes.lock().await;
        Arc::clone(processes.entry(name.to_string()).or_default())
    }

    /// Start a new server into `current` (the slot's held lock).
    async fn start_stdio_process(
        &self,
        slot: &StdioSlot,
        current: &mut Option<Arc<McpStdioProcess>>,
        name: &str,
        launch: StdioLaunch,
    ) -> Result<Arc<McpStdioProcess>> {
        *current = None;
        slot.attempted.store(true, Ordering::SeqCst);
        slot.stopped.store(false, Ordering::SeqCst);
        let process =
            Arc::new(McpStdioProcess::spawn(name, launch, self.stdio_request_timeout).await?);
        *current = Some(Arc::clone(&process));
        slot.starts.fetch_add(1, Ordering::SeqCst);
        Ok(process)
    }

    async fn get_or_create_stdio_process(
        &self,
        prov: &McpProvider,
    ) -> Result<Arc<McpStdioProcess>> {
        let slot = self.stdio_slot(&prov.base.name).await;
        // Concurrent first calls to the same server wait here for one start-up.
        let launch = StdioLaunch::of(prov)
            .ok_or_else(|| anyhow!("No command provided for stdio MCP provider"))?;
        let mut current = slot.process.lock().await;
        if let Some(process) = current.as_ref() {
            if process.is_alive() && process.launch.as_ref() == Some(&launch) {
                return Ok(Arc::clone(process));
            }
        }
        // Not started yet, the server exited, or it was started with other settings: start a
        // fresh one. Calls still running on the old server finish before it is stopped.
        self.start_stdio_process(&slot, &mut current, &prov.base.name, launch)
            .await
    }

    fn stdio_status(slot: &StdioSlot, process: Option<&Arc<McpStdioProcess>>) -> McpServerStatus {
        let starts = slot.starts.load(Ordering::SeqCst);
        let mut status = McpServerStatus {
            restarts: starts.saturating_sub(1),
            ..McpServerStatus::not_started()
        };
        status.state = match process {
            Some(p) if p.is_alive() => {
                status.pid = p.pid;
                status.uptime_secs = Some(p.started_at.elapsed().as_secs());
                McpServerState::Running
            }
            _ if slot.stopped.load(Ordering::SeqCst) => McpServerState::Stopped,
            None if !slot.attempted.load(Ordering::SeqCst) => McpServerState::NotStarted,
            _ => McpServerState::Exited,
        };
        status
    }

    /// State of the stdio server of the provider named `name`. Never waits for a server that
    /// is starting: that is reported as [`McpServerState::Starting`].
    pub async fn stdio_server_status(&self, name: &str) -> McpServerStatus {
        let slot = self.stdio_processes.lock().await.get(name).cloned();
        let Some(slot) = slot else {
            return McpServerStatus::not_started();
        };
        // Calls hold the lock only for a moment unless they are starting the server.
        for _ in 0..20 {
            if let Ok(current) = slot.process.try_lock() {
                return Self::stdio_status(&slot, current.as_ref());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        McpServerStatus {
            state: McpServerState::Starting,
            restarts: slot.starts.load(Ordering::SeqCst).saturating_sub(1),
            ..McpServerStatus::not_started()
        }
    }

    /// Stop the stdio server of the provider named `name` now. Calls waiting on it fail at
    /// once; the next call starts it again. Returns whether it was running.
    pub async fn stop_stdio_server(&self, name: &str) -> bool {
        let slot = self.stdio_processes.lock().await.get(name).cloned();
        let Some(slot) = slot else {
            return false;
        };
        let mut current = slot.process.lock().await;
        slot.stopped.store(true, Ordering::SeqCst);
        match current.take() {
            Some(process) => {
                let was_running = process.is_alive();
                process.stop();
                was_running
            }
            None => false,
        }
    }

    /// Stop `prov`'s stdio server (if running) and start it again now, so a failing start is
    /// reported here instead of on the next call. Also starts a stopped or never-started server.
    pub async fn restart_stdio_server(&self, prov: &McpProvider) -> Result<McpServerStatus> {
        let launch = StdioLaunch::of(prov)
            .ok_or_else(|| anyhow!("MCP provider '{}' has no command", prov.base.name))?;
        let slot = self.stdio_slot(&prov.base.name).await;
        let mut current = slot.process.lock().await;
        if let Some(old) = current.take() {
            old.stop();
        }
        let process = self
            .start_stdio_process(&slot, &mut current, &prov.base.name, launch)
            .await?;
        Ok(Self::stdio_status(&slot, Some(&process)))
    }

    async fn mcp_request(&self, prov: &McpProvider, method: &str, params: Value) -> Result<Value> {
        if prov.is_http() {
            self.mcp_http_request(prov, method, params).await
        } else if prov.is_stdio() {
            let process = self.get_or_create_stdio_process(prov).await?;
            process.send_request(method, params).await
        } else {
            Err(anyhow!(
                "MCP provider must have either 'url' (HTTP) or 'command' (stdio)"
            ))
        }
    }

    async fn mcp_http_stream(
        &self,
        prov: &McpProvider,
        params: Value,
    ) -> Result<Box<dyn StreamResult>> {
        use eventsource_stream::Eventsource;
        use futures::StreamExt;

        let url = prov
            .url
            .as_ref()
            .ok_or_else(|| anyhow!("No URL provided for HTTP MCP provider"))?;

        validate_url_security(url, false)?;

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "params": params,
            "id": self.next_http_id(),
        });

        let mut req = self.client.post(url).json(&request);

        // Add headers
        if let Some(headers) = &prov.headers {
            for (k, v) in headers {
                req = req.header(k, v);
            }
        }

        // Add authentication
        if let Some(auth) = &prov.base.auth {
            req = self.apply_auth(req, auth)?;
        }

        // Set Accept header for SSE
        req = req.header("Accept", "text/event-stream");

        let response =
            crate::transports::rate_limit::reject_rate_limited(req.send().await?).await?;

        if !response.status().is_success() {
            return Err(anyhow!("MCP stream request failed: {}", response.status()));
        }

        // Create a channel to stream results
        // Create a channel to stream results with larger buffer
        let (tx, rx) = tokio::sync::mpsc::channel(256);

        // Spawn a task to read SSE events
        tokio::spawn(async move {
            let byte_stream = response.bytes_stream();
            let mut event_stream = byte_stream.eventsource();

            while let Some(event_result) = event_stream.next().await {
                match event_result {
                    Ok(event) => {
                        // Parse the event data as JSON
                        match serde_json::from_str::<Value>(&event.data) {
                            Ok(value) => {
                                if tx.send(Ok(value)).await.is_err() {
                                    break; // Receiver dropped
                                }
                            }
                            Err(e) => {
                                let _ = tx
                                    .send(Err(anyhow!("Failed to parse SSE event: {}", e)))
                                    .await;
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(anyhow!("SSE stream error: {}", e))).await;
                        break;
                    }
                }
            }
        });

        Ok(crate::transports::stream::boxed_channel_stream(rx, None))
    }

    /// Standard MCP sends exactly one response per request id, so a stdio "stream" is the single
    /// routed answer delivered as a one-item stream.
    async fn mcp_stdio_stream(
        &self,
        prov: &McpProvider,
        params: Value,
    ) -> Result<Box<dyn StreamResult>> {
        let process = self.get_or_create_stdio_process(prov).await?;
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tokio::spawn(async move {
            let outcome = process.send_request("tools/call", params).await;
            let _ = tx.send(outcome).await;
        });
        Ok(crate::transports::stream::boxed_channel_stream(rx, None))
    }
}

#[async_trait]
impl ClientTransport for McpTransport {
    async fn register_tool_provider(&self, _prov: &dyn Provider) -> Result<Vec<Tool>> {
        let mcp_prov = _prov
            .as_any()
            .downcast_ref::<McpProvider>()
            .ok_or_else(|| anyhow!("Provider is not an McpProvider"))?;

        let mut parsed = Vec::new();
        let mut cursor: Value = Value::Null;

        loop {
            let params = if cursor.is_null() {
                serde_json::json!({})
            } else {
                serde_json::json!({ "cursor": cursor })
            };
            let result = self.mcp_request(mcp_prov, "tools/list", params).await?;

            if let Some(tools) = result.get("tools").and_then(|v| v.as_array()) {
                for tool in tools {
                    if let Ok(t) = serde_json::from_value::<Tool>(tool.clone()) {
                        parsed.push(t);
                    } else if let Some(obj) = tool.as_object() {
                        // Standard MCP `tools/list` schema uses `inputSchema` + optional `outputSchema`
                        let Some(name) = obj.get("name").and_then(|v| v.as_str()) else {
                            continue;
                        };
                        let description = obj
                            .get("description")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let raw_in = obj
                            .get("inputSchema")
                            .or_else(|| obj.get("inputs"))
                            .cloned()
                            .unwrap_or_else(|| serde_json::json!({"type": "object"}));
                        let raw_out = obj
                            .get("outputSchema")
                            .or_else(|| obj.get("outputs"))
                            .cloned()
                            .unwrap_or_else(|| serde_json::json!({"type": "object"}));
                        let inputs = serde_json::from_value(raw_in).unwrap_or(
                            crate::tools::ToolInputOutputSchema {
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
                            },
                        );
                        let outputs = serde_json::from_value(raw_out).unwrap_or(
                            crate::tools::ToolInputOutputSchema {
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
                            },
                        );
                        parsed.push(Tool {
                            name: name.to_string(),
                            description,
                            inputs,
                            outputs,
                            tags: vec!["mcp".to_string()],
                            average_response_size: None,
                            provider: serde_json::to_value(mcp_prov).ok(),
                        });
                    }
                }
            }

            match result.get("nextCursor") {
                Some(Value::String(next)) if !next.is_empty() => {
                    cursor = Value::String(next.clone());
                }
                _ => break,
            }
        }

        Ok(parsed)
    }

    async fn deregister_tool_provider(&self, _prov: &dyn Provider) -> Result<()> {
        let mcp_prov = _prov
            .as_any()
            .downcast_ref::<McpProvider>()
            .ok_or_else(|| anyhow!("Provider is not an McpProvider"))?;

        // For stdio processes, terminate the process
        if mcp_prov.is_stdio() {
            let mut processes = self.stdio_processes.lock().await;
            if let Some(process) = processes.remove(&mcp_prov.base.name) {
                // Process will be dropped and killed when Arc count reaches 0
                drop(process);
            }
        }

        Ok(())
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        args: HashMap<String, Value>,
        prov: &dyn Provider,
    ) -> Result<Value> {
        let mcp_prov = prov
            .as_any()
            .downcast_ref::<McpProvider>()
            .ok_or_else(|| anyhow!("Provider is not an McpProvider"))?;

        let mcp_tool_name = tool_name
            .strip_prefix(&format!("{}.", mcp_prov.base.name))
            .unwrap_or(tool_name);

        let params = serde_json::json!({
            "name": mcp_tool_name,
            "arguments": args,
        });

        self.mcp_request(mcp_prov, "tools/call", params).await
    }

    async fn call_tool_stream(
        &self,
        tool_name: &str,
        args: HashMap<String, Value>,
        prov: &dyn Provider,
    ) -> Result<Box<dyn StreamResult>> {
        let mcp_prov = prov
            .as_any()
            .downcast_ref::<McpProvider>()
            .ok_or_else(|| anyhow!("Provider is not an McpProvider"))?;

        let mcp_tool_name = tool_name
            .strip_prefix(&format!("{}.", mcp_prov.base.name))
            .unwrap_or(tool_name);

        let params = serde_json::json!({
            "name": mcp_tool_name,
            "arguments": args,
        });

        if mcp_prov.is_http() {
            self.mcp_http_stream(mcp_prov, params).await
        } else if mcp_prov.is_stdio() {
            self.mcp_stdio_stream(mcp_prov, params).await
        } else {
            Err(anyhow!(
                "MCP provider must have either 'url' (HTTP) or 'command' (stdio)"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{ApiKeyAuth, AuthType};
    use crate::providers::base::{BaseProvider, ProviderType};
    use axum::{extract::Json, http::HeaderValue, routing::post, Router};
    use bytes::Bytes;
    use serde_json::json;

    #[test]
    fn apply_auth_adds_expected_headers() {
        let transport = McpTransport::new();
        let auth = AuthConfig::ApiKey(ApiKeyAuth {
            auth_type: AuthType::ApiKey,
            api_key: "secret".to_string(),
            var_name: "X-MCP".to_string(),
            location: "header".to_string(),
        });

        let request = transport
            .apply_auth(reqwest::Client::new().post("http://example.com"), &auth)
            .unwrap()
            .build()
            .unwrap();

        assert_eq!(request.headers().get("X-MCP").unwrap(), "secret");
    }

    #[tokio::test]
    async fn mcp_request_requires_transport_configuration() {
        let transport = McpTransport::new();
        let prov = McpProvider {
            base: BaseProvider {
                name: "invalid".to_string(),
                provider_type: ProviderType::Mcp,
                auth: None,
                allowed_communication_protocols: None,
            },
            url: None,
            headers: None,
            command: None,
            args: None,
            env_vars: None,
        };

        let err = transport
            .mcp_request(&prov, "ping", Value::Null)
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("MCP provider must have either 'url' (HTTP) or 'command' (stdio)"));
    }

    #[tokio::test]
    async fn register_call_and_stream_mcp_http_transport() {
        async fn handler(
            headers: axum::http::HeaderMap,
            Json(payload): Json<Value>,
        ) -> Json<Value> {
            if headers
                .get(axum::http::header::ACCEPT)
                .and_then(|v| v.to_str().ok())
                == Some("text/event-stream")
            {
                // Should never hit JSON handler for stream; placeholder
                return Json(json!({ "error": "wrong handler" }));
            }

            let method = payload.get("method").and_then(|v| v.as_str()).unwrap_or("");
            match method {
                "tools/list" => Json(json!({
                    "jsonrpc": "2.0",
                    "result": {
                        "tools": [{
                            "name": "echo",
                            "description": "echo tool",
                            "inputs": { "type": "object" },
                            "outputs": { "type": "object" },
                            "tags": []
                        }]
                    },
                    "id": 1
                })),
                "tools/call" => {
                    let params = payload.get("params").cloned().unwrap_or_default();
                    Json(json!({
                        "jsonrpc": "2.0",
                        "result": { "called": params },
                        "id": 1
                    }))
                }
                _ => Json(json!({ "jsonrpc": "2.0", "result": {}, "id": 1 })),
            }
        }

        async fn stream_handler(
            headers: axum::http::HeaderMap,
            Json(_payload): Json<Value>,
        ) -> impl axum::response::IntoResponse {
            assert_eq!(
                headers.get(axum::http::header::ACCEPT),
                Some(&HeaderValue::from_static("text/event-stream"))
            );

            let stream = tokio_stream::iter(vec![
                Ok::<_, std::convert::Infallible>(Bytes::from_static(b"data: {\"idx\":1}\n\n")),
                Ok(Bytes::from_static(b"data: {\"idx\":2}\n\n")),
            ]);
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                axum::body::Body::from_stream(stream),
            )
        }

        let app = Router::new()
            .route("/", post(handler))
            .route("/stream", post(stream_handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let prov = McpProvider {
            base: BaseProvider {
                name: "mcp".to_string(),
                provider_type: ProviderType::Mcp,
                auth: None,
                allowed_communication_protocols: None,
            },
            url: Some(format!("http://{}", addr)),
            headers: None,
            command: None,
            args: None,
            env_vars: None,
        };

        let transport = McpTransport::new();
        let tools = transport
            .register_tool_provider(&prov)
            .await
            .expect("register");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");

        let mut args = HashMap::new();
        args.insert("msg".into(), Value::String("hi".into()));
        let call_value = transport
            .call_tool("echo", args.clone(), &prov)
            .await
            .expect("call");
        assert_eq!(
            call_value,
            json!({ "called": { "name": "echo", "arguments": json!(args) } })
        );

        // Stream uses /stream endpoint
        let stream_prov = McpProvider {
            url: Some(format!("http://{}/stream", addr)),
            ..prov.clone()
        };
        let mut stream = transport
            .call_tool_stream("echo", args, &stream_prov)
            .await
            .expect("stream");
        assert_eq!(stream.next().await.unwrap().unwrap(), json!({"idx":1}));
        assert_eq!(stream.next().await.unwrap().unwrap(), json!({"idx":2}));
        stream.close().await.unwrap();
    }

    #[tokio::test]
    async fn streamable_http_mcp_negotiates_session_accept_header_and_sse_framing() {
        async fn streamable_handler(
            headers: axum::http::HeaderMap,
            Json(payload): Json<Value>,
        ) -> axum::response::Response {
            use axum::http::StatusCode;
            use axum::response::IntoResponse;

            let accept = headers
                .get(axum::http::header::ACCEPT)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !accept.contains("application/json") || !accept.contains("text/event-stream") {
                return (StatusCode::NOT_ACCEPTABLE, "406 Not Acceptable").into_response();
            }

            let method = payload.get("method").and_then(|v| v.as_str()).unwrap_or("");
            match method {
                "initialize" => (
                    StatusCode::OK,
                    [
                        ("content-type", "text/event-stream"),
                        ("mcp-session-id", "sess-ctx7-42"),
                    ],
                    "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-03-26\",\"capabilities\":{}}}\n\n",
                )
                    .into_response(),
                "notifications/initialized" => {
                    assert_eq!(
                        headers.get("mcp-session-id").and_then(|v| v.to_str().ok()),
                        Some("sess-ctx7-42")
                    );
                    StatusCode::ACCEPTED.into_response()
                }
                "tools/list" => {
                    assert_eq!(
                        headers.get("mcp-session-id").and_then(|v| v.to_str().ok()),
                        Some("sess-ctx7-42")
                    );
                    (
                        StatusCode::OK,
                        [("content-type", "text/event-stream")],
                        "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[{\"name\":\"resolve-library-id\",\"description\":\"Resolve lib\",\"inputSchema\":{\"type\":\"object\",\"properties\":{\"libraryName\":{\"type\":\"string\"}}}}]}}\n\n",
                    )
                        .into_response()
                }
                "tools/call" => {
                    assert_eq!(
                        headers.get("mcp-session-id").and_then(|v| v.to_str().ok()),
                        Some("sess-ctx7-42")
                    );
                    let params = payload.get("params").cloned().unwrap_or_default();
                    let sse = format!(
                        "event: message\ndata: {}\n\n",
                        json!({"jsonrpc": "2.0", "id": 2, "result": {"resolved": params}})
                    );
                    (StatusCode::OK, [("content-type", "text/event-stream")], sse).into_response()
                }
                _ => StatusCode::BAD_REQUEST.into_response(),
            }
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/mcp", post(streamable_handler)),
            )
            .await
            .unwrap();
        });

        let prov = McpProvider {
            base: BaseProvider {
                name: "context7".to_string(),
                provider_type: ProviderType::Mcp,
                auth: None,
                allowed_communication_protocols: None,
            },
            url: Some(format!("http://{}/mcp", addr)),
            headers: None,
            command: None,
            args: None,
            env_vars: None,
        };

        let transport = McpTransport::new();
        let tools = transport.register_tool_provider(&prov).await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "resolve-library-id");

        let mut args = HashMap::new();
        args.insert("libraryName".into(), Value::String("ratatui".into()));
        let res = transport
            .call_tool("context7.resolve-library-id", args, &prov)
            .await
            .unwrap();
        assert_eq!(res["resolved"]["name"], "resolve-library-id");
        assert_eq!(res["resolved"]["arguments"]["libraryName"], "ratatui");
    }

    // ---- stdio MCP: concurrent calls, out-of-order answers, timeouts, server exit ----

    /// In-memory stdio MCP server. Each `tools/call` is answered after `arguments.delay_ms`
    /// with `{"tag": arguments.tag}`, so later requests can overtake earlier ones. Before each
    /// answer it prints noise the client must skip: a non-JSON line, a blank line, a
    /// notification, and a `ping` request (the returned flag turns true once the client answers
    /// it). Tool `silent` is never answered; tool `crash` makes the server exit.
    fn spawn_mock_stdio_server() -> (
        tokio::io::DuplexStream,
        tokio::io::DuplexStream,
        Arc<AtomicBool>,
    ) {
        let (client_w, server_r) = tokio::io::duplex(1 << 16);
        let (server_w, client_r) = tokio::io::duplex(1 << 16);
        let ping_answered = Arc::new(AtomicBool::new(false));
        let ping_flag = Arc::clone(&ping_answered);
        tokio::spawn(async move {
            let out = Arc::new(Mutex::new(server_w));
            let mut lines = BufReader::new(server_r).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let msg: Value = serde_json::from_str(&line).unwrap();
                let Some(method) = msg.get("method").and_then(Value::as_str) else {
                    if msg["id"] == "srv-ping" && msg["result"] == json!({}) {
                        ping_flag.store(true, Ordering::SeqCst);
                    }
                    continue;
                };
                let Some(id) = msg.get("id").cloned() else {
                    continue; // notification
                };
                let method = method.to_string();
                let args = msg["params"]["arguments"].clone();
                match msg["params"]["name"].as_str() {
                    Some("silent") => continue,
                    Some("crash") => return,
                    _ => {}
                }
                let out = Arc::clone(&out);
                tokio::spawn(async move {
                    let delay = args["delay_ms"].as_u64().unwrap_or(0);
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    let result = if method == "initialize" {
                        json!({"protocolVersion": "2024-11-05", "capabilities": {}})
                    } else {
                        json!({"tag": args["tag"]})
                    };
                    let noise = format!(
                        "server log line, not JSON\n\n{}\n{}\n",
                        json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {}}),
                        json!({"jsonrpc": "2.0", "id": "srv-ping", "method": "ping"}),
                    );
                    let answer = json!({"jsonrpc": "2.0", "id": id, "result": result});
                    let mut w = out.lock().await;
                    let _ = w.write_all(noise.as_bytes()).await;
                    let _ = w.write_all(format!("{answer}\n").as_bytes()).await;
                    let _ = w.flush().await;
                });
            }
        });
        (client_w, client_r, ping_answered)
    }

    async fn mock_stdio_process(timeout: Duration) -> (Arc<McpStdioProcess>, Arc<AtomicBool>) {
        let (w, r, ping) = spawn_mock_stdio_server();
        let proc = McpStdioProcess::from_streams("mock", w, r, None, timeout);
        proc.initialize().await.expect("initialize");
        (Arc::new(proc), ping)
    }

    fn call_params(tool: &str, tag: u64, delay_ms: u64) -> Value {
        json!({"name": tool, "arguments": {"tag": tag, "delay_ms": delay_ms}})
    }

    fn slot_holding(process: Arc<McpStdioProcess>) -> Arc<StdioSlot> {
        let slot = StdioSlot::default();
        *slot.process.try_lock().unwrap() = Some(process);
        slot.starts.store(1, Ordering::SeqCst);
        slot.attempted.store(true, Ordering::SeqCst);
        Arc::new(slot)
    }

    fn stdio_provider(name: &str, command: &str) -> McpProvider {
        McpProvider {
            base: BaseProvider {
                name: name.to_string(),
                provider_type: ProviderType::Mcp,
                auth: None,
                allowed_communication_protocols: None,
            },
            url: None,
            headers: None,
            command: Some(command.to_string()),
            args: None,
            env_vars: None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stdio_concurrent_calls_each_get_their_own_answer_out_of_order() {
        let (proc, ping_answered) = mock_stdio_process(Duration::from_secs(10)).await;
        let n = 50u64;
        // Earlier requests wait longer, so answers come back in reverse order.
        let calls = (0..n).map(|tag| {
            let proc = Arc::clone(&proc);
            tokio::spawn(async move {
                proc.send_request("tools/call", call_params("echo", tag, (n - tag) * 4))
                    .await
            })
        });
        let results = futures::future::join_all(calls).await;
        for (tag, res) in results.into_iter().enumerate() {
            let value = res.unwrap().expect("call succeeded");
            assert_eq!(
                value["tag"],
                json!(tag),
                "call {tag} got someone else's answer"
            );
        }
        assert!(
            ping_answered.load(Ordering::SeqCst),
            "server ping was not answered"
        );
        assert!(proc.pending.lock().unwrap().is_empty());
        assert!(proc.is_alive());
    }

    #[tokio::test]
    async fn stdio_unanswered_call_times_out_without_blocking_others() {
        let (proc, _) = mock_stdio_process(Duration::from_millis(300)).await;
        let silent = proc.send_request("tools/call", call_params("silent", 1, 0));
        let normal = proc.send_request("tools/call", call_params("echo", 2, 0));
        let (silent, normal) = tokio::join!(silent, normal);
        let err = silent.unwrap_err().to_string();
        assert!(err.contains("did not answer 'tools/call'"), "{err}");
        assert!(err.contains("mock"), "{err}");
        assert_eq!(normal.unwrap()["tag"], 2);
        // The server is still usable and the timed-out id was cleaned up.
        assert!(proc.is_alive());
        assert!(proc.pending.lock().unwrap().is_empty());
        let again = proc
            .send_request("tools/call", call_params("echo", 3, 0))
            .await
            .unwrap();
        assert_eq!(again["tag"], 3);
    }

    #[tokio::test]
    async fn stdio_server_exit_fails_waiting_calls_at_once() {
        let (proc, _) = mock_stdio_process(Duration::from_secs(30)).await;
        let started = std::time::Instant::now();
        let waiting: Vec<_> = (0..3)
            .map(|tag| {
                let proc = Arc::clone(&proc);
                tokio::spawn(async move {
                    proc.send_request("tools/call", call_params("silent", tag, 0))
                        .await
                })
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let crash = proc
            .send_request("tools/call", call_params("crash", 9, 0))
            .await;
        assert!(crash.is_err());
        for handle in waiting {
            let err = handle.await.unwrap().unwrap_err().to_string();
            assert!(err.contains("closed its output"), "{err}");
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "calls hung after server exit"
        );
        assert!(!proc.is_alive());
        let err = proc
            .send_request("tools/call", call_params("echo", 1, 0))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not running"), "{err}");
    }

    #[tokio::test]
    async fn stdio_dead_server_is_replaced_and_live_one_reused() {
        let transport = McpTransport::new();
        let prov = stdio_provider("mockstdio", "utcp-test-no-such-mcp-server");

        // A live cached server started with the provider's settings is reused.
        let (w, r, _) = spawn_mock_stdio_server();
        let mut live = McpStdioProcess::from_streams("mock", w, r, None, Duration::from_secs(5));
        live.launch = StdioLaunch::of(&prov);
        live.initialize().await.unwrap();
        let live = Arc::new(live);
        transport
            .stdio_processes
            .lock()
            .await
            .insert(prov.base.name.clone(), slot_holding(Arc::clone(&live)));
        let got = transport.get_or_create_stdio_process(&prov).await.unwrap();
        assert!(Arc::ptr_eq(&got, &live));

        // Streaming through the router returns this call's own answer.
        let mut args = HashMap::new();
        args.insert("tag".to_string(), json!(77));
        let mut stream = transport
            .call_tool_stream("mockstdio.echo", args, &prov)
            .await
            .unwrap();
        assert_eq!(stream.next().await.unwrap().unwrap()["tag"], 77);
        assert!(
            stream.next().await.unwrap().is_none(),
            "one answer per request"
        );

        // Changed settings (re-registered with another argument, a rotated token...) are not
        // served by the old server: the transport tries to start a new one.
        let mut changed = prov.clone();
        changed.env_vars = Some(HashMap::from([("TOKEN".into(), "rotated".into())]));
        let err = transport
            .get_or_create_stdio_process(&changed)
            .await
            .err()
            .expect("server with old settings must not be reused")
            .to_string();
        assert!(err.contains("failed to start MCP server command"), "{err}");

        // A dead cached server is not reused: the transport tries to start a new one
        // (which fails here because the test command does not exist).
        let (w, r) = (tokio::io::sink(), tokio::io::empty());
        let mut dead = McpStdioProcess::from_streams("dead", w, r, None, Duration::from_secs(5));
        dead.launch = StdioLaunch::of(&prov);
        for _ in 0..100 {
            if !dead.is_alive() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(!dead.is_alive());
        transport
            .stdio_processes
            .lock()
            .await
            .insert(prov.base.name.clone(), slot_holding(Arc::new(dead)));
        let err = transport
            .get_or_create_stdio_process(&prov)
            .await
            .err()
            .expect("dead server must not be reused")
            .to_string();
        assert!(err.contains("failed to start MCP server command"), "{err}");
    }

    #[tokio::test]
    async fn http_mcp_requests_use_unique_ids() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));
        let seen_in = Arc::clone(&seen);
        let app = Router::new().route(
            "/",
            post(move |Json(payload): Json<Value>| {
                let seen = Arc::clone(&seen_in);
                async move {
                    let id = payload.get("id").cloned();
                    if let Some(n) = id.as_ref().and_then(Value::as_u64) {
                        seen.lock().unwrap().push(n);
                    }
                    Json(json!({"jsonrpc": "2.0", "id": id, "result": {"ok": true}}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let prov = McpProvider {
            url: Some(format!("http://{addr}")),
            command: None,
            ..stdio_provider("httpids", "unused")
        };
        let transport = McpTransport::new();
        transport
            .mcp_request(&prov, "tools/list", json!({}))
            .await
            .unwrap();
        let calls = (0..10).map(|_| transport.mcp_request(&prov, "tools/call", json!({})));
        for res in futures::future::join_all(calls).await {
            res.unwrap();
        }
        let ids = seen.lock().unwrap().clone();
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(ids.len(), 12, "initialize + tools/list + 10 calls: {ids:?}");
        assert_eq!(unique.len(), ids.len(), "duplicate JSON-RPC ids: {ids:?}");
    }

    // ---- stdio MCP against a real child process (Node), through the transport ----

    /// Real stdio MCP server: `tests/fixtures/mock_mcp_server.cjs`, run as `node <script>`
    /// (see the script for its tools). `UTCP_TEST_FIXTURES` overrides the fixtures folder, so the
    /// compiled tests can run without the source tree.
    fn node_mcp_provider(name: &str) -> McpProvider {
        let script = std::env::var_os("UTCP_TEST_FIXTURES")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests")
                    .join("fixtures")
            })
            .join("mock_mcp_server.cjs");
        McpProvider {
            args: Some(vec![script.display().to_string()]),
            ..stdio_provider(name, "node")
        }
    }

    async fn call_node(
        transport: &McpTransport,
        prov: &McpProvider,
        tool: &str,
        tag: u64,
    ) -> Result<Value> {
        let mut args = HashMap::new();
        args.insert("tag".to_string(), json!(tag));
        transport
            .call_tool(&format!("{}.{tool}", prov.base.name), args, prov)
            .await
    }

    fn kill_process(pid: u64) {
        #[cfg(unix)]
        let status = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
        #[cfg(windows)]
        let status = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .status();
        assert!(status.expect("run kill").success(), "could not kill {pid}");
    }

    fn process_running(pid: u64) -> bool {
        #[cfg(unix)]
        {
            // A zombie (exited, not yet reaped) counts as stopped.
            if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                let state = stat.rsplit(')').next().unwrap_or("").trim_start();
                return !state.starts_with('Z');
            }
            // SAFETY: signal 0 only checks that the process exists.
            unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
        }
        #[cfg(windows)]
        {
            let out = std::process::Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
                .output()
                .expect("run tasklist");
            String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
        }
    }

    async fn wait_until_stopped(pid: u64) {
        for _ in 0..250 {
            if !process_running(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("MCP server process {pid} is still running");
    }

    /// `npx`, `uvx` and `docker run` start the real server as their own child. Stopping the
    /// server must stop that child too, even when it ignores stdin closing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn node_stdio_server_behind_a_launcher_is_stopped_with_its_whole_tree() {
        let mut prov = node_mcp_provider("nodetree");
        let args = prov.args.as_mut().unwrap();
        args.extend(["--wrapper".to_string(), "--stubborn".to_string()]);

        // 1. Deregistering the provider stops the server.
        let transport = McpTransport::new();
        let pid = call_node(&transport, &prov, "echo", 1).await.unwrap()["pid"]
            .as_u64()
            .unwrap();
        assert!(process_running(pid));
        transport.deregister_tool_provider(&prov).await.unwrap();
        wait_until_stopped(pid).await;

        // 2. Dropping the transport (daemon shutdown, end of a direct-mode call) stops it too.
        let transport = McpTransport::new();
        let pid = call_node(&transport, &prov, "echo", 2).await.unwrap()["pid"]
            .as_u64()
            .unwrap();
        drop(transport);
        wait_until_stopped(pid).await;

        // 3. New launch settings replace the running server.
        let transport = McpTransport::new();
        let old = call_node(&transport, &prov, "echo", 3).await.unwrap()["pid"]
            .as_u64()
            .unwrap();
        prov.env_vars = Some(HashMap::from([("TOKEN".into(), "rotated".into())]));
        let new = call_node(&transport, &prov, "echo", 4).await.unwrap()["pid"]
            .as_u64()
            .unwrap();
        assert_ne!(old, new);
        wait_until_stopped(old).await;
        assert!(process_running(new));
    }

    /// Stopping sends SIGTERM first, so a server can clean up and `docker run` / `podman run`
    /// can forward the stop into their container. What ignores it is killed after the grace.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn node_stdio_server_gets_sigterm_first_and_sigkill_after_the_grace_period() {
        let dir = tempfile::tempdir().unwrap();
        let term_file = dir.path().join("term.txt");
        for ignore_term in [false, true] {
            let mut prov = node_mcp_provider(if ignore_term {
                "nodenoterm"
            } else {
                "nodeterm"
            });
            let args = prov.args.as_mut().unwrap();
            args.extend(["--wrapper".to_string(), "--stubborn".to_string()]);
            if ignore_term {
                args.push("--ignore-term".to_string());
            }
            prov.env_vars = Some(HashMap::from([(
                "MOCK_MCP_TERM_FILE".to_string(),
                term_file.display().to_string(),
            )]));

            let transport = McpTransport::new();
            let pid = call_node(&transport, &prov, "echo", 1).await.unwrap()["pid"]
                .as_u64()
                .unwrap();
            let started = std::time::Instant::now();
            transport.deregister_tool_provider(&prov).await.unwrap();
            wait_until_stopped(pid).await;
            let elapsed = started.elapsed();

            let termed = std::fs::read_to_string(&term_file).unwrap_or_default();
            assert!(
                termed.lines().any(|l| l.trim() == pid.to_string()),
                "server {pid} was not sent SIGTERM (ignore_term={ignore_term})"
            );
            if ignore_term {
                assert!(
                    elapsed >= process_tree::STOP_GRACE - Duration::from_millis(200),
                    "killed before the grace period ended: {elapsed:?}"
                );
            } else {
                assert!(
                    elapsed < process_tree::STOP_GRACE,
                    "a server that exits on SIGTERM waited for the grace period: {elapsed:?}"
                );
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn node_stdio_server_answers_concurrent_calls_through_the_transport() {
        let prov = Arc::new(node_mcp_provider("nodemock"));
        let transport = Arc::new(McpTransport::new());

        let tools = transport
            .register_tool_provider(prov.as_ref())
            .await
            .unwrap();
        let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["echo", "stats", "silent", "crash"]);

        let n = 50u64;
        let calls = (0..n).map(|tag| {
            let (transport, prov) = (Arc::clone(&transport), Arc::clone(&prov));
            tokio::spawn(async move { call_node(&transport, &prov, "echo", tag).await })
        });
        let results = futures::future::join_all(calls).await;
        let mut pids = std::collections::HashSet::new();
        for (tag, res) in results.into_iter().enumerate() {
            let value = res.unwrap().expect("call succeeded");
            assert_eq!(
                value["tag"],
                json!(tag),
                "call {tag} got someone else's answer"
            );
            pids.insert(value["pid"].as_u64().unwrap());
        }
        assert_eq!(
            pids.len(),
            1,
            "all calls share one server process: {pids:?}"
        );

        // Each ping is answered (on the same stdin) before the tool answer after it is routed,
        // so by now the server has seen every ping answer.
        let stats = call_node(&transport, &prov, "stats", 0).await.unwrap();
        assert_eq!(stats["pingsSent"], json!(n));
        assert_eq!(
            stats["pingsAnswered"],
            json!(n),
            "every server ping gets an answer"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn node_stdio_unanswered_call_times_out_and_server_keeps_working() {
        let prov = node_mcp_provider("nodeslow");
        let transport = McpTransport::new().with_stdio_request_timeout(Duration::from_secs(2));

        let pid = call_node(&transport, &prov, "echo", 1).await.unwrap()["pid"].clone();
        let started = std::time::Instant::now();
        let (silent, normal) = tokio::join!(
            call_node(&transport, &prov, "silent", 2),
            call_node(&transport, &prov, "echo", 3),
        );
        let err = silent.unwrap_err().to_string();
        assert!(
            err.contains("'nodeslow' did not answer 'tools/call'"),
            "{err}"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(normal.unwrap()["tag"], 3);

        let after = call_node(&transport, &prov, "echo", 4).await.unwrap();
        assert_eq!(after["tag"], 4);
        assert_eq!(after["pid"], pid, "a timeout must not restart the server");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn node_stdio_server_crash_or_kill_fails_waiting_calls_and_next_call_restarts_it() {
        let prov = Arc::new(node_mcp_provider("nodecrash"));
        let transport = Arc::new(McpTransport::new());
        let first_pid = call_node(&transport, &prov, "echo", 1).await.unwrap()["pid"]
            .as_u64()
            .unwrap();

        // 1. The server exits while calls wait: they all fail at once, not after the timeout.
        let started = std::time::Instant::now();
        let waiting: Vec<_> = (0..3)
            .map(|tag| {
                let (transport, prov) = (Arc::clone(&transport), Arc::clone(&prov));
                tokio::spawn(async move { call_node(&transport, &prov, "silent", tag).await })
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(call_node(&transport, &prov, "crash", 9).await.is_err());
        for handle in waiting {
            let err = handle.await.unwrap().unwrap_err().to_string();
            assert!(err.contains("closed its output"), "{err}");
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "calls hung after exit"
        );

        // 2. The next call starts a new server process.
        let second = call_node(&transport, &prov, "echo", 2).await.unwrap();
        assert_eq!(second["tag"], 2);
        let second_pid = second["pid"].as_u64().unwrap();
        assert_ne!(second_pid, first_pid, "crashed server was not replaced");

        // 3. Killed from outside: once its output closes, the next call starts another one.
        let process = transport.get_or_create_stdio_process(&prov).await.unwrap();
        kill_process(second_pid);
        for _ in 0..250 {
            if !process.is_alive() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!process.is_alive(), "killed server not noticed");
        let third = call_node(&transport, &prov, "echo", 3).await.unwrap();
        assert_eq!(third["tag"], 3);
        assert_ne!(third["pid"].as_u64().unwrap(), second_pid);
    }

    /// `utcp daemon servers / stop-server / restart-server` at the transport level.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn node_stdio_server_can_be_listed_stopped_and_restarted() {
        use McpServerState::*;
        let prov = Arc::new(node_mcp_provider("nodemanage"));
        let transport = Arc::new(McpTransport::new());
        let pid_of = |v: Value| v["pid"].as_u64().unwrap();
        let status = || transport.stdio_server_status("nodemanage");

        assert_eq!(status().await, McpServerStatus::not_started());
        assert!(!transport.stop_stdio_server("nodemanage").await);

        // 1. Running: its pid and uptime, no restarts yet.
        let first = pid_of(call_node(&transport, &prov, "echo", 1).await.unwrap());
        let s = status().await;
        assert_eq!(
            (s.state, s.pid, s.restarts),
            (Running, Some(first as u32), 0)
        );
        assert!(s.uptime_secs.is_some());

        // 2. Stop: a waiting call fails at once with a clear reason, and the process is gone.
        let waiting = {
            let (transport, prov) = (Arc::clone(&transport), Arc::clone(&prov));
            tokio::spawn(async move { call_node(&transport, &prov, "silent", 2).await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        let started = std::time::Instant::now();
        assert!(transport.stop_stdio_server("nodemanage").await);
        let err = waiting.await.unwrap().unwrap_err().to_string();
        assert!(err.contains("'nodemanage' was stopped on request"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "waiting call hung"
        );
        wait_until_stopped(first).await;
        let s = status().await;
        assert_eq!((s.state, s.pid), (Stopped, None));

        // 3. The next call starts it again.
        let second = pid_of(call_node(&transport, &prov, "echo", 3).await.unwrap());
        assert_ne!(second, first);
        let s = status().await;
        assert_eq!(
            (s.state, s.pid, s.restarts),
            (Running, Some(second as u32), 1)
        );

        // 4. Restart: a new process right away, the old one stopped, calls go to the new one.
        let s = transport.restart_stdio_server(&prov).await.unwrap();
        assert_eq!((s.state, s.restarts), (Running, 2));
        let third = s.pid.unwrap() as u64;
        assert_ne!(third, second);
        wait_until_stopped(second).await;
        let answer = call_node(&transport, &prov, "echo", 4).await.unwrap();
        assert_eq!(pid_of(answer), third);

        // 5. Restarting a stopped server starts it.
        assert!(transport.stop_stdio_server("nodemanage").await);
        wait_until_stopped(third).await;
        let s = transport.restart_stdio_server(&prov).await.unwrap();
        assert_eq!((s.state, s.restarts), (Running, 3));

        // 6. A crash shows as exited until the next call.
        assert!(call_node(&transport, &prov, "crash", 5).await.is_err());
        for _ in 0..250 {
            if status().await.state == Exited {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(status().await.state, Exited);

        // 7. A restart that cannot start the server reports why.
        let broken = stdio_provider("nodemanage", "utcp-test-no-such-mcp-server");
        let err = transport.restart_stdio_server(&broken).await.unwrap_err();
        assert!(err.to_string().contains("failed to start"), "{err}");
        assert_eq!(status().await.state, Exited);
    }
}
