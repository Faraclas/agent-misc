use crate::activity::{
    activity_detail, append_activity_entry, batch_activity_summary, is_logging_enabled,
    read_recent_activity, ActivityEntry,
};
use crate::catalog::{
    dry_run_tool_from_manual, execute_loaded_tool, load_daemon_startup_env_map,
    load_or_build_index_sync, load_single_cached_manual, manual_env_var_names, project_json_fields,
    query_systemd_user_unit_state, release_manual_connections, resolve_manual_and_tool,
    resolve_secret_var, retain_allowed_env, BatchRequestItem, BatchResultItem, CachedUtcpManual,
    CredentialStatus, SOURCE_CALLER_SHELL, SOURCE_DAEMON_ENV, SOURCE_SECRETS_COMMAND,
};
#[cfg(feature = "mcp")]
use crate::catalog::{manual_stdio_mcp_servers, ManualStdioServer};
use crate::index::{SearchIndex, ToolSchemaDetail, ToolSummary};
use crate::ipc_files::{ClientFiles, ConnFiles};
use crate::media::{inspect_media_spool, prune_media_spool, MediaSpoolStatus};
use crate::paths::{UtcpPaths, UtcpSettings};
use crate::secrets::{run_secrets_command, SecretsCommandStatus};
use crate::security::build_secure_http_client;
use crate::transfer::{
    json_file_meta, oversized_json, CallOutput, CallerFiles, FileDownload, FileKind, NoFiles,
};
use crate::transports::registry::CommunicationProtocolRegistry;
use crate::transports::McpServerState;
use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use futures::stream::{self, StreamExt};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, VecDeque};

#[cfg(unix)]
use std::path::Path;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, watch, RwLock};

pub(crate) const MAX_IPC_FRAME_BYTES: usize = 32 * 1024 * 1024;
/// How many recent calls the daemon keeps in memory (TUI Tab 5, `utcp log recent`).
pub const RING_BUFFER_CAPACITY: usize = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcRequest {
    pub caller: String,
    pub force_log: bool,
    pub command: IpcCommand,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcCommand {
    Ping,
    Status,
    Reload,
    ReloadEnv {
        caller_env: HashMap<String, String>,
    },
    Shutdown,
    List {
        manual: Option<String>,
        /// The staged manuals instead of the active ones.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        staged: bool,
    },
    Search {
        query: String,
        limit: usize,
        include_schemas: bool,
    },
    Schema {
        target: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        staged: bool,
    },
    Call {
        target: String,
        args: HashMap<String, Value>,
        dry_run: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fields: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        caller_env: HashMap<String, String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        staged: bool,
    },
    Batch {
        items: Vec<BatchRequestItem>,
        concurrency: usize,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        caller_env: HashMap<String, String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        staged: bool,
    },
    RecentActivity {
        limit: usize,
    },
    /// Local (stdio) MCP servers of registered manuals and what they are doing.
    Servers,
    /// Stop a manual's local MCP server now; its next call starts it again.
    StopServer {
        manual: String,
    },
    /// Stop and start a manual's local MCP server now.
    RestartServer {
        manual: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum IpcResponse {
    Ok { data: IpcPayload },
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum IpcPayload {
    Pong,
    DaemonStatus(Box<DaemonStatusInfo>),
    Reloaded {
        manuals: usize,
        tools: usize,
    },
    EnvReloaded {
        total_vars: usize,
        /// The secrets command's run during this reload; `None` when none is configured.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secrets_command: Option<SecretsCommandStatus>,
    },
    ShuttingDown,
    Tools(Vec<ToolSummary>),
    Schema(ToolSchemaDetail),
    CallResult(Value),
    BatchResults(Vec<BatchResultItem>),
    Activity(Vec<ActivityEntry>),
    Servers(Vec<McpServerInfo>),
    ServerStopped {
        manual: String,
        was_running: bool,
    },
}

/// One local (stdio) MCP server the daemon runs for a registered manual.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerInfo {
    pub manual: String,
    /// The transport's name for the server; the manual name for `import-mcp` manuals.
    pub server: String,
    /// The program only (never its arguments or environment, which may hold secrets).
    pub command: String,
    pub state: McpServerState,
    pub pid: Option<u32>,
    pub uptime_secs: Option<u64>,
    /// Starts after the first: crash recovery, changed settings, `restart-server`.
    pub restarts: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatusInfo {
    pub pid: u32,
    pub socket_endpoint: String,
    pub uptime_secs: u64,
    pub manual_count: usize,
    pub tool_count: usize,
    pub requests_served: u64,
    pub logging_enabled: bool,
    pub env_var_count: usize,
    pub systemd_user_state: Option<String>,
    pub credentials: Vec<CredentialStatus>,
    pub media_spool: MediaSpoolStatus,
    /// The last run of `[secrets] command`; `None` when none is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets_command: Option<SecretsCommandStatus>,
}

struct InMemoryCatalog {
    index: SearchIndex,
    manuals: HashMap<String, CachedUtcpManual>,
    /// The staged manuals (`utcp stage`), callable only with `--staged`.
    staged_index: SearchIndex,
    staged: HashMap<String, CachedUtcpManual>,
    /// Only the variables registered manuals can read (see `allowed_env`), never a whole environment.
    env_map: HashMap<String, String>,
    /// Allowed shell variables pushed via `utcp daemon reload-env` or absorbed from callers
    /// (retained across file reloads)
    pushed_shell_env: HashMap<String, String>,
    /// Every env var name a loaded manual may resolve; anything else is dropped on receipt.
    allowed_env: BTreeSet<String>,
    env_mtime: Option<SystemTime>,
    /// The secrets command's values (allowed names only). They win over every other source.
    secrets: DaemonSecrets,
}

/// What the daemon holds from `[secrets] command` in `settings.toml`.
#[derive(Clone, Default)]
struct DaemonSecrets {
    values: HashMap<String, String>,
    /// The last run; `None` when no command is configured.
    status: Option<SecretsCommandStatus>,
}

/// Runs the secrets command for the `allowed` names. A failed run (or an unreadable
/// `settings.toml`) keeps the `previous` values, so a passing problem does not drop working
/// credentials. Blocks for up to the command's timeout: call it outside the catalog lock.
fn load_daemon_secrets(
    paths: &UtcpPaths,
    allowed: &BTreeSet<String>,
    previous: &DaemonSecrets,
) -> DaemonSecrets {
    let kept = |status: Option<SecretsCommandStatus>| {
        let mut values = previous.values.clone();
        retain_allowed_env(&mut values, allowed);
        DaemonSecrets { values, status }
    };
    let secrets = match UtcpSettings::load_checked(&paths.settings_file) {
        Ok(settings) => settings.secrets,
        Err(_) => return kept(previous.status.clone()),
    };
    let Some(secrets) = secrets else {
        return DaemonSecrets::default();
    };
    if allowed.is_empty() {
        return DaemonSecrets {
            values: HashMap::new(),
            status: Some(SecretsCommandStatus {
                command: secrets.command.join(" "),
                error: None,
                provided: Vec::new(),
            }),
        };
    }
    let run = run_secrets_command(&secrets, allowed);
    match run.status.error {
        None => DaemonSecrets {
            values: run.values,
            status: Some(run.status),
        },
        Some(ref error) if !previous.values.is_empty() => {
            let mut status = run.status.clone();
            status.error = Some(format!("{error} (kept the values from its last good run)"));
            kept(Some(status))
        }
        Some(_) => DaemonSecrets {
            values: HashMap::new(),
            status: Some(run.status),
        },
    }
}

/// The daemon's env map: the allowed part of its startup environment, overlaid with the allowed
/// pushed shell variables, then with the secrets command's values.
fn build_env_map(
    mut base: HashMap<String, String>,
    pushed_shell_env: &HashMap<String, String>,
    secrets: &DaemonSecrets,
    allowed_env: &BTreeSet<String>,
) -> HashMap<String, String> {
    retain_allowed_env(&mut base, allowed_env);
    for (k, v) in pushed_shell_env.iter().chain(&secrets.values) {
        if allowed_env.contains(k) {
            base.insert(k.clone(), v.clone());
        }
    }
    base
}

pub struct DaemonServerState {
    paths: UtcpPaths,
    started_at: Instant,
    catalog: RwLock<InMemoryCatalog>,
    protocols: CommunicationProtocolRegistry,
    /// The registry's `mcp` transport, kept typed for server management.
    #[cfg(feature = "mcp")]
    mcp: Arc<crate::transports::mcp::McpTransport>,
    recent_ring: RwLock<VecDeque<ActivityEntry>>,
    log_tx: mpsc::UnboundedSender<ActivityEntry>,
    requests_served: AtomicU64,
    last_activity_unix_secs: AtomicU64,
    shutdown_tx: watch::Sender<bool>,
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Loads the catalog and runs the secrets command (keeping `previous_secrets` if it fails). Blocks
/// for up to the command's timeout.
fn load_in_memory_catalog(
    paths: &UtcpPaths,
    pushed_shell_env: HashMap<String, String>,
    previous_secrets: &DaemonSecrets,
) -> Result<InMemoryCatalog> {
    let index = load_or_build_index_sync(paths)?;
    let mut manuals = HashMap::with_capacity(index.manuals.len());
    for m in &index.manuals {
        if let Ok(loaded) = load_single_cached_manual(paths, &m.name) {
            manuals.insert(m.name.clone(), loaded);
        }
    }
    let staged_index = crate::staging::staged_index(paths)?;
    let mut staged = HashMap::with_capacity(staged_index.manuals.len());
    for m in &staged_index.manuals {
        if let Ok(loaded) = crate::staging::load_staged_manual(paths, &m.name) {
            staged.insert(m.name.clone(), loaded);
        }
    }
    let env_mtime = std::fs::metadata(&paths.env_file)
        .and_then(|m| m.modified())
        .ok();
    let mut allowed_env = index.env_var_names();
    allowed_env.extend(staged_index.env_var_names());
    allowed_env.extend(manual_env_var_names(
        manuals
            .iter()
            .chain(staged.iter())
            .map(|(n, m)| (n.as_str(), m)),
    ));
    let mut pushed_shell_env = pushed_shell_env;
    retain_allowed_env(&mut pushed_shell_env, &allowed_env);
    let secrets = load_daemon_secrets(paths, &allowed_env, previous_secrets);
    let env_map = build_env_map(
        load_daemon_startup_env_map(paths),
        &pushed_shell_env,
        &secrets,
        &allowed_env,
    );
    Ok(InMemoryCatalog {
        index,
        manuals,
        staged_index,
        staged,
        env_map,
        pushed_shell_env,
        allowed_env,
        env_mtime,
        secrets,
    })
}

/// The daemon's required credentials (active and staged manuals), with where each value came from.
fn daemon_credentials(catalog: &InMemoryCatalog) -> Vec<CredentialStatus> {
    let mut credentials = Vec::new();
    let all = catalog
        .index
        .manuals
        .iter()
        .map(|m| (m, false))
        .chain(catalog.staged_index.manuals.iter().map(|m| (m, true)));
    for (m, staged) in all {
        for req_var in &m.required_env_vars {
            let resolved = resolve_secret_var(&m.name, req_var, &catalog.env_map);
            let source = resolved.as_ref().map(|(value, via)| {
                let from = |map: &HashMap<String, String>| {
                    map.get(via).map(String::as_str) == Some(*value)
                };
                if from(&catalog.secrets.values) {
                    SOURCE_SECRETS_COMMAND
                } else if from(&catalog.pushed_shell_env) {
                    SOURCE_CALLER_SHELL
                } else {
                    SOURCE_DAEMON_ENV
                }
                .to_string()
            });
            credentials.push(CredentialStatus {
                manual: m.name.clone(),
                env_var: req_var.clone(),
                is_set: resolved.is_some(),
                resolved_via: resolved.map(|(_, via)| via),
                source,
                staged,
            });
        }
    }
    credentials
}

/// Writes a line to stderr (the journal under systemd) and to `daemon.log`.
fn daemon_notice(paths: &UtcpPaths, message: &str) {
    eprintln!("utcp daemon: {message}");
    let _ = crate::daemon_log::append(paths, message);
}

/// Logs a secrets command failure when it is new (not on every reload while it keeps failing), and
/// its recovery.
fn log_secrets_change(
    paths: &UtcpPaths,
    before: Option<&SecretsCommandStatus>,
    after: Option<&SecretsCommandStatus>,
) {
    let before_error = before.and_then(|s| s.error.as_deref());
    let Some(status) = after else {
        return;
    };
    match status.error.as_deref() {
        Some(error) if before_error != Some(error) => daemon_notice(
            paths,
            &format!("secrets command `{}` failed: {error}", status.command),
        ),
        None if before_error.is_some() => daemon_notice(
            paths,
            &format!("secrets command `{}` works again", status.command),
        ),
        _ => {}
    }
}

/// What the daemon reports when it starts: an invalid `settings.toml`, a failed secrets command,
/// and every required variable that is still not set (with its manual). Empty when all is well.
fn startup_warnings(paths: &UtcpPaths, catalog: &InMemoryCatalog) -> Vec<String> {
    let mut warnings = Vec::new();
    if let Err(e) = UtcpSettings::load_checked(&paths.settings_file) {
        warnings.push(format!(
            "settings.toml is not used, the defaults apply: {e:#}"
        ));
    }
    if let Some(SecretsCommandStatus {
        command,
        error: Some(error),
        ..
    }) = &catalog.secrets.status
    {
        warnings.push(format!("secrets command `{command}` failed: {error}"));
    }
    let missing: Vec<String> = daemon_credentials(catalog)
        .into_iter()
        .filter(|c| !c.is_set)
        .map(|c| format!("{} ({})", c.env_var, c.manual))
        .collect();
    if !missing.is_empty() {
        warnings.push(format!(
            "not set, so calls that need them will fail: {}. Have the secrets command print them \
             (`[secrets]` in settings.toml), or run `utcp daemon reload-env` from a shell that has them",
            missing.join(", ")
        ));
    }
    warnings
}

impl DaemonServerState {
    fn new(
        paths: UtcpPaths,
        log_tx: mpsc::UnboundedSender<ActivityEntry>,
        shutdown_tx: watch::Sender<bool>,
    ) -> Result<Self> {
        let catalog = load_in_memory_catalog(&paths, HashMap::new(), &DaemonSecrets::default())?;
        let initial_recent: VecDeque<ActivityEntry> =
            read_recent_activity(&paths, RING_BUFFER_CAPACITY)
                .into_iter()
                .collect();
        let client = build_secure_http_client();
        let protocols = CommunicationProtocolRegistry::with_shared_http_client(client.clone());
        #[cfg(feature = "mcp")]
        let mcp = Arc::new(crate::transports::mcp::McpTransport::with_client(client));
        #[cfg(feature = "mcp")]
        protocols.register("mcp", mcp.clone());
        #[cfg(not(feature = "mcp"))]
        drop(client);

        Ok(Self {
            paths,
            started_at: Instant::now(),
            catalog: RwLock::new(catalog),
            protocols,
            #[cfg(feature = "mcp")]
            mcp,
            recent_ring: RwLock::new(initial_recent),
            log_tx,
            requests_served: AtomicU64::new(0),
            last_activity_unix_secs: AtomicU64::new(now_unix_secs()),
            shutdown_tx,
        })
    }

    pub async fn reload(&self) -> Result<(usize, usize)> {
        let (pushed, previous_secrets) = {
            let guard = self.catalog.read().await;
            (guard.pushed_shell_env.clone(), guard.secrets.clone())
        };
        let paths = self.paths.clone();
        let previous = previous_secrets.clone();
        let fresh =
            tokio::task::spawn_blocking(move || load_in_memory_catalog(&paths, pushed, &previous))
                .await
                .map_err(|e| anyhow!("reload task failed: {e}"))??;
        log_secrets_change(
            &self.paths,
            previous_secrets.status.as_ref(),
            fresh.secrets.status.as_ref(),
        );
        let counts = (fresh.index.manuals.len(), fresh.index.tools.len());
        let removed: Vec<CachedUtcpManual> = {
            let mut guard = self.catalog.write().await;
            let old = std::mem::replace(&mut *guard, fresh);
            let kept =
                |name: &String| guard.manuals.contains_key(name) || guard.staged.contains_key(name);
            old.manuals
                .into_iter()
                .chain(old.staged)
                .filter(|(name, _)| !kept(name))
                .map(|(_, manual)| manual)
                .collect()
        };
        // Stop servers of deregistered manuals (a changed manual's server is replaced on its
        // next call, when the transport sees different launch settings).
        for manual in &removed {
            release_manual_connections(manual, &self.protocols).await;
        }
        Ok(counts)
    }

    /// Re-reads the daemon's own environment, runs the secrets command again and keeps the caller's
    /// allowed variables. Returns the number of variables held afterwards (only names registered
    /// manuals use) and the secrets command's run.
    pub async fn reload_env(
        &self,
        caller_env: HashMap<String, String>,
    ) -> (usize, Option<SecretsCommandStatus>) {
        let (allowed, previous) = {
            let guard = self.catalog.read().await;
            (guard.allowed_env.clone(), guard.secrets.clone())
        };
        let paths = self.paths.clone();
        let prev = previous.clone();
        let (base, secrets) = tokio::task::spawn_blocking(move || {
            (
                load_daemon_startup_env_map(&paths),
                load_daemon_secrets(&paths, &allowed, &prev),
            )
        })
        .await
        .unwrap_or_else(|_| (HashMap::new(), previous.clone()));
        log_secrets_change(
            &self.paths,
            previous.status.as_ref(),
            secrets.status.as_ref(),
        );
        let mut guard = self.catalog.write().await;
        for (k, v) in caller_env {
            if guard.allowed_env.contains(&k) {
                guard.pushed_shell_env.insert(k, v);
            }
        }
        guard.env_mtime = std::fs::metadata(&self.paths.env_file)
            .and_then(|m| m.modified())
            .ok();
        guard.secrets = secrets;
        guard.env_map = build_env_map(
            base,
            &guard.pushed_shell_env,
            &guard.secrets,
            &guard.allowed_env,
        );
        (guard.env_map.len(), guard.secrets.status.clone())
    }

    /// Absorb non-empty variables a registered manual needs from a caller's shell when they were
    /// missing when `systemd --user` started the daemon at login/boot (eliminating cold-boot 401s).
    /// Every other caller variable is ignored and never stored.
    async fn merge_missing_caller_env(&self, caller_env: HashMap<String, String>) {
        let is_new = |guard: &InMemoryCatalog, k: &String, v: &String| {
            !v.is_empty() && guard.allowed_env.contains(k) && !guard.env_map.contains_key(k)
        };
        let has_new = {
            let guard = self.catalog.read().await;
            caller_env.iter().any(|(k, v)| is_new(&guard, k, v))
        };
        if has_new {
            let mut guard = self.catalog.write().await;
            for (k, v) in caller_env {
                if is_new(&guard, &k, &v) {
                    guard.pushed_shell_env.insert(k.clone(), v.clone());
                    guard.env_map.insert(k, v);
                }
            }
        }
    }

    async fn refresh_env_if_modified(&self) {
        let current_mtime = std::fs::metadata(&self.paths.env_file)
            .and_then(|m| m.modified())
            .ok();
        let needs_reload = {
            let guard = self.catalog.read().await;
            guard.env_mtime != current_mtime
        };
        if needs_reload {
            let fresh_env = load_daemon_startup_env_map(&self.paths);
            let mut guard = self.catalog.write().await;
            guard.env_map = build_env_map(
                fresh_env,
                &guard.pushed_shell_env,
                &guard.secrets,
                &guard.allowed_env,
            );
            guard.env_mtime = current_mtime;
        }
    }

    async fn record_activity(&self, entry: ActivityEntry, force_log: bool) {
        {
            let mut ring = self.recent_ring.write().await;
            ring.push_front(entry.clone());
            while ring.len() > RING_BUFFER_CAPACITY {
                ring.pop_back();
            }
        }
        if force_log || is_logging_enabled(&self.paths) {
            let _ = self.log_tx.send(entry);
        }
    }

    async fn handle_request(&self, req: IpcRequest, conn: Option<Arc<ConnFiles>>) -> IpcResponse {
        if matches!(
            req.command,
            IpcCommand::List { .. }
                | IpcCommand::Search { .. }
                | IpcCommand::Schema { .. }
                | IpcCommand::Call { .. }
                | IpcCommand::Batch { .. }
        ) {
            self.requests_served.fetch_add(1, Ordering::Relaxed);
            self.last_activity_unix_secs
                .store(now_unix_secs(), Ordering::Relaxed);
        }

        let started = Instant::now();
        let caller = req.caller;
        let force_log = req.force_log;

        match req.command {
            IpcCommand::Ping => IpcResponse::Ok {
                data: IpcPayload::Pong,
            },
            IpcCommand::Status => {
                self.refresh_env_if_modified().await;
                let guard = self.catalog.read().await;
                let credentials = daemon_credentials(&guard);
                IpcResponse::Ok {
                    data: IpcPayload::DaemonStatus(Box::new(DaemonStatusInfo {
                        pid: std::process::id(),
                        socket_endpoint: self.paths.socket_endpoint.clone(),
                        uptime_secs: self.started_at.elapsed().as_secs(),
                        manual_count: guard.index.manuals.len(),
                        tool_count: guard.index.tools.len(),
                        requests_served: self.requests_served.load(Ordering::Relaxed),
                        logging_enabled: is_logging_enabled(&self.paths),
                        env_var_count: guard.env_map.len(),
                        systemd_user_state: query_systemd_user_unit_state(),
                        credentials,
                        media_spool: inspect_media_spool(&self.paths),
                        secrets_command: guard.secrets.status.clone(),
                    })),
                }
            }
            IpcCommand::Reload => match self.reload().await {
                Ok((manuals, tools)) => IpcResponse::Ok {
                    data: IpcPayload::Reloaded { manuals, tools },
                },
                Err(e) => IpcResponse::Error {
                    message: e.to_string(),
                },
            },
            IpcCommand::ReloadEnv { caller_env } => {
                let (total_vars, secrets_command) = self.reload_env(caller_env).await;
                IpcResponse::Ok {
                    data: IpcPayload::EnvReloaded {
                        total_vars,
                        secrets_command,
                    },
                }
            }
            // The stop itself is triggered in `handle_async_stream` once this reply is sent:
            // stopping now would end the daemon before the caller hears back.
            IpcCommand::Shutdown => IpcResponse::Ok {
                data: IpcPayload::ShuttingDown,
            },
            IpcCommand::List { manual, staged } => {
                let tools = {
                    let guard = self.catalog.read().await;
                    if staged {
                        guard.staged_index.list_tools(manual.as_deref())
                    } else {
                        guard.index.list_tools(manual.as_deref())
                    }
                };
                self.record_activity(
                    ActivityEntry {
                        timestamp: Utc::now().to_rfc3339(),
                        mode: "daemon".into(),
                        caller,
                        command: "list".into(),
                        target: manual.unwrap_or_else(|| "*".into()),
                        ok: true,
                        status: "OK".into(),
                        latency_ms: started.elapsed().as_secs_f64() * 1000.0,
                        detail: None,
                    },
                    force_log,
                )
                .await;
                IpcResponse::Ok {
                    data: IpcPayload::Tools(tools),
                }
            }
            IpcCommand::Search {
                query,
                limit,
                include_schemas,
            } => {
                let results = {
                    let guard = self.catalog.read().await;
                    guard.index.search(&query, limit, include_schemas)
                };
                self.record_activity(
                    ActivityEntry {
                        timestamp: Utc::now().to_rfc3339(),
                        mode: "daemon".into(),
                        caller,
                        command: "search".into(),
                        target: query,
                        ok: true,
                        status: format!("{} hits", results.len()),
                        latency_ms: started.elapsed().as_secs_f64() * 1000.0,
                        detail: None,
                    },
                    force_log,
                )
                .await;
                IpcResponse::Ok {
                    data: IpcPayload::Tools(results),
                }
            }
            IpcCommand::Schema { target, staged } => {
                let schema_opt = {
                    let guard = self.catalog.read().await;
                    if staged {
                        guard.staged_index.get_schema(&target)
                    } else {
                        guard.index.get_schema(&target)
                    }
                };
                match schema_opt {
                    Some(schema) => {
                        self.record_activity(
                            ActivityEntry {
                                timestamp: Utc::now().to_rfc3339(),
                                mode: "daemon".into(),
                                caller,
                                command: "schema".into(),
                                target,
                                ok: true,
                                status: "OK".into(),
                                latency_ms: started.elapsed().as_secs_f64() * 1000.0,
                                detail: None,
                            },
                            force_log,
                        )
                        .await;
                        IpcResponse::Ok {
                            data: IpcPayload::Schema(schema),
                        }
                    }
                    None => IpcResponse::Error {
                        message: format!(
                            "Tool '{}' not found in {}",
                            target,
                            if staged {
                                "the staged manuals (utcp list --staged)"
                            } else {
                                "registered manuals"
                            }
                        ),
                    },
                }
            }
            IpcCommand::Call {
                target,
                args,
                dry_run,
                fields,
                caller_env,
                staged,
            } => {
                self.refresh_env_if_modified().await;
                if !caller_env.is_empty() {
                    self.merge_missing_caller_env(caller_env).await;
                }
                let resolved = {
                    let guard = self.catalog.read().await;
                    let (index, manuals) = if staged {
                        (&guard.staged_index, &guard.staged)
                    } else {
                        (&guard.index, &guard.manuals)
                    };
                    resolve_manual_and_tool(&target, Some(index), &self.paths).and_then(
                        |(mname, tname)| {
                            let m = manuals.get(&mname).cloned().ok_or_else(|| {
                                if staged {
                                    anyhow!("Manual '{}' is not staged", mname)
                                } else {
                                    anyhow!("Manual '{}' not loaded", mname)
                                }
                            })?;
                            Ok((m, mname, tname, guard.env_map.clone()))
                        },
                    )
                };

                let outcome = match resolved {
                    Ok((manual, mname, tname, env_map)) => {
                        if dry_run {
                            dry_run_tool_from_manual(&manual, &mname, &tname, &args, &env_map)
                        } else {
                            let files: &dyn CallerFiles = match &conn {
                                Some(c) => c.as_ref(),
                                None => &NoFiles,
                            };
                            match execute_loaded_tool(
                                &manual,
                                &mname,
                                &tname,
                                args,
                                &env_map,
                                &self.protocols,
                                &self.paths,
                                files,
                            )
                            .await
                            {
                                Ok(output) => {
                                    deliver(
                                        output,
                                        &tname,
                                        fields.as_deref(),
                                        conn.as_deref(),
                                        None,
                                    )
                                    .await
                                }
                                Err(e) => Err(e),
                            }
                        }
                    }
                    Err(e) => Err(e),
                };

                let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
                match outcome {
                    Ok(val) => {
                        self.record_activity(
                            ActivityEntry {
                                timestamp: Utc::now().to_rfc3339(),
                                mode: "daemon".into(),
                                caller,
                                command: crate::activity::call_verb(dry_run, staged).into(),
                                target,
                                ok: true,
                                status: "OK".into(),
                                latency_ms,
                                detail: None,
                            },
                            force_log,
                        )
                        .await;
                        IpcResponse::Ok {
                            data: IpcPayload::CallResult(val),
                        }
                    }
                    Err(err) => {
                        let msg = format!("{:#}", err);
                        let detail = {
                            let guard = self.catalog.read().await;
                            activity_detail(&format!("{:#}", err), &guard.env_map)
                        };
                        self.record_activity(
                            ActivityEntry {
                                timestamp: Utc::now().to_rfc3339(),
                                mode: "daemon".into(),
                                caller,
                                command: crate::activity::call_verb(dry_run, staged).into(),
                                target,
                                ok: false,
                                status: "ERR".into(),
                                latency_ms,
                                detail: Some(detail),
                            },
                            force_log,
                        )
                        .await;
                        IpcResponse::Error { message: msg }
                    }
                }
            }
            IpcCommand::Batch {
                items,
                concurrency,
                caller_env,
                staged,
            } => {
                self.refresh_env_if_modified().await;
                if !caller_env.is_empty() {
                    self.merge_missing_caller_env(caller_env).await;
                }
                let (index_snap, manuals_snap, env_snap) = {
                    let guard = self.catalog.read().await;
                    let (index, manuals) = if staged {
                        (&guard.staged_index, &guard.staged)
                    } else {
                        (&guard.index, &guard.manuals)
                    };
                    (index.clone(), manuals.clone(), guard.env_map.clone())
                };
                let max_conc = concurrency.max(1);
                let count = items.len();
                let results: Vec<(usize, BatchResultItem)> =
                    stream::iter(items.into_iter().enumerate())
                        .map(|(idx, item)| {
                            let protocols = self.protocols.clone();
                            let index = &index_snap;
                            let manuals = &manuals_snap;
                            let env_map = &env_snap;
                            let paths = &self.paths;
                            let conn = conn.clone();
                            async move {
                                let item_start = Instant::now();
                                let item_fields = item.fields.clone();
                                let res = async {
                                    let (mname, tname) =
                                        resolve_manual_and_tool(&item.tool, Some(index), paths)?;
                                    let manual = manuals
                                        .get(&mname)
                                        .ok_or_else(|| anyhow!("Manual '{}' not loaded", mname))?;
                                    let files: &dyn CallerFiles = match &conn {
                                        Some(c) => c.as_ref(),
                                        None => &NoFiles,
                                    };
                                    let output = execute_loaded_tool(
                                        manual, &mname, &tname, item.args, env_map, &protocols,
                                        paths, files,
                                    )
                                    .await?;
                                    deliver(
                                        output,
                                        &tname,
                                        item_fields.as_deref(),
                                        conn.as_deref(),
                                        Some(idx),
                                    )
                                    .await
                                }
                                .await;
                                let latency_ms = item_start.elapsed().as_secs_f64() * 1000.0;
                                match res {
                                    Ok(v) => (
                                        idx,
                                        BatchResultItem::succeeded(
                                            item.id, item.tool, v, latency_ms,
                                        ),
                                    ),
                                    Err(e) => (
                                        idx,
                                        BatchResultItem::failed(
                                            item.id,
                                            item.tool,
                                            format!("{e:#}"),
                                            latency_ms,
                                        ),
                                    ),
                                }
                            }
                        })
                        .buffer_unordered(max_conc)
                        .collect()
                        .await;

                let mut ordered = results;
                ordered.sort_by_key(|(i, _)| *i);
                let final_items: Vec<BatchResultItem> =
                    ordered.into_iter().map(|(_, r)| r).collect();

                let (all_ok, status, detail) = batch_activity_summary(&final_items, &env_snap);
                self.record_activity(
                    ActivityEntry {
                        timestamp: Utc::now().to_rfc3339(),
                        mode: "daemon".into(),
                        caller,
                        command: "batch".into(),
                        target: format!("{} calls", count),
                        ok: all_ok,
                        status,
                        latency_ms: started.elapsed().as_secs_f64() * 1000.0,
                        detail,
                    },
                    force_log,
                )
                .await;

                IpcResponse::Ok {
                    data: IpcPayload::BatchResults(final_items),
                }
            }
            IpcCommand::RecentActivity { limit } => {
                let ring = self.recent_ring.read().await;
                let items: Vec<ActivityEntry> = ring.iter().take(limit).cloned().collect();
                IpcResponse::Ok {
                    data: IpcPayload::Activity(items),
                }
            }
            cmd @ (IpcCommand::Servers
            | IpcCommand::StopServer { .. }
            | IpcCommand::RestartServer { .. }) => {
                self.manage_servers(cmd, caller, force_log).await
            }
        }
    }

    /// The local MCP servers of one manual (an error when it has none), or of every manual.
    #[cfg(feature = "mcp")]
    async fn stdio_servers(
        &self,
        manual: Option<&str>,
    ) -> Result<Vec<(String, ManualStdioServer)>> {
        self.refresh_env_if_modified().await;
        let guard = self.catalog.read().await;
        let mut names: Vec<&String> = match manual {
            Some(name) => vec![
                guard
                    .manuals
                    .get_key_value(name)
                    .ok_or_else(|| anyhow!("Manual '{}' is not registered", name))?
                    .0,
            ],
            None => guard.manuals.keys().collect(),
        };
        names.sort();
        let mut found = Vec::new();
        for name in names {
            for server in manual_stdio_mcp_servers(&guard.manuals[name], name, &guard.env_map) {
                found.push((name.clone(), server));
            }
        }
        if let (Some(name), true) = (manual, found.is_empty()) {
            return Err(anyhow!(
                "Manual '{}' has no local MCP server to manage: only manuals imported with \
                 `utcp import-mcp --command` run one",
                name
            ));
        }
        Ok(found)
    }

    /// `utcp daemon servers`, `stop-server` and `restart-server`.
    #[cfg(feature = "mcp")]
    async fn manage_servers(
        &self,
        cmd: IpcCommand,
        caller: String,
        force_log: bool,
    ) -> IpcResponse {
        let started = Instant::now();
        let (verb, manual) = match &cmd {
            IpcCommand::StopServer { manual } => ("stop-server", Some(manual.clone())),
            IpcCommand::RestartServer { manual } => ("restart-server", Some(manual.clone())),
            _ => ("servers", None),
        };
        let outcome: Result<IpcPayload> = async {
            let servers = self.stdio_servers(manual.as_deref()).await?;
            match cmd {
                IpcCommand::StopServer { manual } => {
                    let mut was_running = false;
                    for (_, s) in &servers {
                        was_running |= self.mcp.stop_stdio_server(&s.provider.base.name).await;
                    }
                    Ok(IpcPayload::ServerStopped {
                        manual,
                        was_running,
                    })
                }
                IpcCommand::RestartServer { .. } => {
                    let mut infos = Vec::new();
                    for (m, s) in servers {
                        let status = self
                            .mcp
                            .restart_stdio_server(&s.provider)
                            .await
                            .with_context(|| {
                                format!("could not restart the MCP server of '{m}'")
                            })?;
                        infos.push(server_info(m, &s, status));
                    }
                    Ok(IpcPayload::Servers(infos))
                }
                _ => {
                    let mut infos = Vec::new();
                    for (m, s) in servers {
                        let status = self.mcp.stdio_server_status(&s.provider.base.name).await;
                        infos.push(server_info(m, &s, status));
                    }
                    Ok(IpcPayload::Servers(infos))
                }
            }
        }
        .await;

        // Listing is read-only; stop and restart go in the recent-calls list, so a call that
        // failed because its server was stopped can be explained with `utcp log recent`.
        if let Some(target) = manual {
            let detail = match &outcome {
                Ok(_) => None,
                Err(e) => {
                    let guard = self.catalog.read().await;
                    Some(activity_detail(&format!("{:#}", e), &guard.env_map))
                }
            };
            self.record_activity(
                ActivityEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    mode: "daemon".into(),
                    caller,
                    command: verb.into(),
                    target,
                    ok: outcome.is_ok(),
                    status: if outcome.is_ok() { "OK" } else { "ERR" }.into(),
                    latency_ms: started.elapsed().as_secs_f64() * 1000.0,
                    detail,
                },
                force_log,
            )
            .await;
        }
        match outcome {
            Ok(data) => IpcResponse::Ok { data },
            Err(e) => IpcResponse::Error {
                message: format!("{:#}", e),
            },
        }
    }

    #[cfg(not(feature = "mcp"))]
    async fn manage_servers(
        &self,
        cmd: IpcCommand,
        _caller: String,
        _force_log: bool,
    ) -> IpcResponse {
        match cmd {
            IpcCommand::Servers => IpcResponse::Ok {
                data: IpcPayload::Servers(Vec::new()),
            },
            _ => IpcResponse::Error {
                message: "this utcp was built without MCP support (cargo feature `mcp`)".into(),
            },
        }
    }
}

#[cfg(feature = "mcp")]
fn server_info(
    manual: String,
    server: &ManualStdioServer,
    status: crate::transports::McpServerStatus,
) -> McpServerInfo {
    McpServerInfo {
        manual,
        server: server.provider.base.name.clone(),
        command: server.command.clone(),
        state: status.state,
        pid: status.pid,
        uptime_secs: status.uptime_secs,
        restarts: status.restarts,
    }
}

/// A call's output as the daemon answers it: JSON with `fields` applied, or a file (or a JSON answer
/// too large to print) streamed to the caller, who saves it. A page of a longer list keeps its
/// next-page arguments beside the answer.
async fn deliver(
    output: CallOutput,
    tool: &str,
    fields: Option<&[String]>,
    conn: Option<&ConnFiles>,
    item: Option<usize>,
) -> Result<Value> {
    match output {
        CallOutput::Json(value) => deliver_json(value, tool, fields, conn, item).await,
        CallOutput::Page { value, next_page } => {
            let result = deliver_json(value, tool, fields, conn, item).await?;
            Ok(crate::transfer::page_value(result, next_page))
        }
        CallOutput::File(file) => match conn {
            Some(conn) => conn.pump(file, item).await,
            None => Err(anyhow!(
                "{tool} answered with a file, which only `utcp call` and `utcp batch` can save"
            )),
        },
    }
}

async fn deliver_json(
    value: Value,
    tool: &str,
    fields: Option<&[String]>,
    conn: Option<&ConnFiles>,
    item: Option<usize>,
) -> Result<Value> {
    let value = match fields {
        Some(f) if !f.is_empty() => project_json_fields(value, f),
        _ => value,
    };
    match (conn, oversized_json(&value)) {
        (Some(conn), Some(bytes)) => {
            let meta = json_file_meta(FileKind::LargeAnswer, tool, bytes.len());
            conn.pump(FileDownload::from_bytes(meta, bytes), item).await
        }
        _ => Ok(value),
    }
}

async fn handle_async_stream<S>(stream: S, state: Arc<DaemonServerState>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut len_buf = [0u8; 4];
    if reader.read_exact(&mut len_buf).await.is_err() {
        return Ok(());
    }
    let frame_len = u32::from_le_bytes(len_buf) as usize;
    if frame_len == 0 || frame_len > MAX_IPC_FRAME_BYTES {
        return Err(anyhow!("Invalid IPC frame size: {}", frame_len));
    }

    let mut payload = vec![0u8; frame_len];
    reader.read_exact(&mut payload).await?;
    let req: IpcRequest = serde_json::from_slice(&payload)?;

    // A call may upload or download files, which takes the connection in both directions.
    if matches!(
        req.command,
        IpcCommand::Call { .. } | IpcCommand::Batch { .. }
    ) {
        return crate::ipc_files::serve_call(reader, writer, |conn| async move {
            state.handle_request(req, Some(conn)).await
        })
        .await;
    }

    let shutting_down = matches!(req.command, IpcCommand::Shutdown);
    let resp = state.handle_request(req, None).await;
    let resp_bytes = serde_json::to_vec(&resp)?;
    let resp_len = (resp_bytes.len() as u32).to_le_bytes();
    let sent = async {
        writer.write_all(&resp_len).await?;
        writer.write_all(&resp_bytes).await?;
        writer.flush().await
    }
    .await;
    if shutting_down {
        // Stop only once the caller has its answer: a written reply can still sit in a pipe
        // buffer, so wait (briefly) for the caller to read it and hang up.
        let mut byte = [0u8; 1];
        let _ = tokio::time::timeout(Duration::from_secs(1), reader.read(&mut byte)).await;
        let _ = state.shutdown_tx.send(true);
    }
    sent?;
    Ok(())
}

/// Synchronous, zero-Tokio IPC client call! A call's uploads are read and its file answers saved in
/// their default places; see [`try_send_ipc_with_files`] to choose where answers go.
pub fn try_send_ipc_sync(paths: &UtcpPaths, req: &IpcRequest) -> Result<Option<IpcResponse>> {
    let mut files = ClientFiles::new(paths);
    try_send_ipc_with_files(paths, req, &mut files)
}

/// [`try_send_ipc_sync`], with `files` saying where a call's answers go (`--out`).
pub fn try_send_ipc_with_files(
    paths: &UtcpPaths,
    req: &IpcRequest,
    files: &mut ClientFiles,
) -> Result<Option<IpcResponse>> {
    let payload = serde_json::to_vec(req)?;

    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;
        let sock_path = Path::new(&paths.socket_endpoint);
        if !sock_path.exists() {
            return Ok(None);
        }
        let mut stream = match UnixStream::connect(sock_path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                return Err(DaemonUnreachable(format!(
                    "Cannot use the utcp daemon at {}: permission denied. Another account (for example \
                     root, through sudo) started it, so this process may not use it. Nothing was sent. \
                     Stop it as that account and start it as yourself, or run `utcp --direct ...`.",
                    paths.socket_endpoint
                ))
                .into());
            }
            Err(_) => return Ok(None),
        };
        // Longer than the HTTP stall timeout: a transfer that keeps moving resets both.
        let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(120)));
        Ok(Some(crate::ipc_files::exchange(
            &mut stream,
            &payload,
            files,
        )?))
    }

    #[cfg(windows)]
    {
        use std::fs::OpenOptions;
        // The daemon creates its next pipe instance right after a client connects; a client
        // that arrives in that gap gets ERROR_PIPE_BUSY. That means "daemon busy", not "no
        // daemon", so retry briefly instead of silently falling back to direct mode.
        const ERROR_PIPE_BUSY: i32 = 231;
        const ERROR_ACCESS_DENIED: i32 = 5;
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut pipe = loop {
            match OpenOptions::new()
                .read(true)
                .write(true)
                .open(&paths.socket_endpoint)
            {
                Ok(p) => break p,
                Err(e)
                    if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                // The pipe exists but its default permissions shut us out: a daemon started as
                // administrator (elevated) admits only elevated callers, another account's admits
                // only that account. Say so instead of quietly running in direct mode.
                Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
                    return Err(DaemonUnreachable(format!(
                        "Cannot use the utcp daemon at {}: access denied. It was started as \
                         administrator (elevated) or by another account, so this process may not use \
                         it. Nothing was sent. Stop it where it was started (`utcp daemon stop` in an \
                         elevated shell) and start it from a normal shell, or run `utcp --direct ...`.",
                        paths.socket_endpoint
                    ))
                    .into());
                }
                Err(_) => return Ok(None),
            }
        };
        if !pipe_owned_by_current_user(&pipe) {
            return Err(DaemonUnreachable(format!(
                "Refusing to use {}: another account created it, so it is not your utcp daemon. \
                 Nothing was sent. Run `utcp --direct ...`, or set UTCP_SOCKET_PATH to another \
                 pipe name and restart the daemon.",
                paths.socket_endpoint
            ))
            .into());
        }
        Ok(Some(crate::ipc_files::exchange(
            &mut pipe, &payload, files,
        )?))
    }
}

/// Named pipe names are machine-wide, so another account could create ours before our daemon
/// does and receive our requests (arguments, and API keys on `reload-env`). Only talk to a pipe
/// owned by this account: the token's user, or its default owner (Administrators in an elevated
/// shell).
#[cfg(windows)]
fn pipe_owned_by_current_user(pipe: &std::fs::File) -> bool {
    use std::os::windows::io::AsRawHandle;
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
    use windows_sys::Win32::Security::{
        EqualSid, GetTokenInformation, TokenOwner, TokenUser, OWNER_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID, TOKEN_INFORMATION_CLASS, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: TOKEN_USER and TOKEN_OWNER both start with the SID pointer, which points into
    // the same (8-byte aligned) buffer and is only used while the buffer lives.
    unsafe fn token_sid_is(token: HANDLE, class: TOKEN_INFORMATION_CLASS, sid: PSID) -> bool {
        let mut len = 0u32;
        GetTokenInformation(token, class, null_mut(), 0, &mut len);
        if len == 0 {
            return false;
        }
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        if GetTokenInformation(token, class, buf.as_mut_ptr().cast(), len, &mut len) == 0 {
            return false;
        }
        EqualSid(*(buf.as_ptr() as *const PSID), sid) != 0
    }

    // SAFETY: documented Win32 calls; the security descriptor and token are freed below.
    unsafe {
        let mut owner: PSID = null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        let status = GetSecurityInfo(
            pipe.as_raw_handle() as HANDLE,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut descriptor,
        );
        if status != 0 {
            return false;
        }
        if owner.is_null() {
            // Windows always gives a kernel object an owner (the creator's default owner when
            // none is set), so this only happens under Wine, whose pipes carry no owner and
            // where every process runs as the same Unix user anyway.
            LocalFree(descriptor);
            return true;
        }
        let mut token: HANDLE = null_mut();
        let owned = OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) != 0 && {
            let matches =
                token_sid_is(token, TokenUser, owner) || token_sid_is(token, TokenOwner, owner);
            CloseHandle(token);
            matches
        };
        LocalFree(descriptor);
        owned
    }
}

/// A daemon endpoint exists but this process may not use it (another account's, or an elevated
/// daemon seen from a normal shell). Unlike "no daemon", this is never a reason to fall back to
/// direct mode quietly or to start a second daemon.
#[derive(Debug)]
pub struct DaemonUnreachable(pub String);

impl std::fmt::Display for DaemonUnreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DaemonUnreachable {}

/// Probe whether the daemon is reachable and return its `DaemonStatusInfo` if running.
pub fn query_daemon_status(paths: &UtcpPaths) -> Option<DaemonStatusInfo> {
    query_daemon_status_checked(paths).unwrap_or(None)
}

/// Like [`query_daemon_status`], but a daemon this process may not use is an error
/// ([`DaemonUnreachable`]) instead of "not running". For `daemon status|start|stop`.
pub fn query_daemon_status_checked(paths: &UtcpPaths) -> Result<Option<DaemonStatusInfo>> {
    let req = IpcRequest {
        caller: "cli".into(),
        force_log: false,
        command: IpcCommand::Status,
    };
    match try_send_ipc_sync(paths, &req) {
        Ok(Some(IpcResponse::Ok {
            data: IpcPayload::DaemonStatus(info),
        })) => Ok(Some(*info)),
        Err(e) if e.is::<DaemonUnreachable>() => Err(e),
        _ => Ok(None),
    }
}

/// Push the current caller's live shell environment variables into the running daemon's RAM.
/// The part of this process's environment a registered or staged manual may read: the only variables the
/// CLI sends to the daemon (`call`, `batch`, `reload-env`). Sends nothing when the index cannot be
/// read, rather than falling back to the whole environment.
pub fn manual_caller_env(paths: &UtcpPaths) -> HashMap<String, String> {
    let Ok(index) = load_or_build_index_sync(paths) else {
        return HashMap::new();
    };
    let mut allowed = index.env_var_names();
    if let Ok(staged) = crate::staging::staged_index(paths) {
        allowed.extend(staged.env_var_names());
    }
    std::env::vars_os()
        .filter_map(|(k, v)| {
            let k = k.into_string().ok()?;
            if !allowed.contains(&k) {
                return None;
            }
            Some((k, v.into_string().ok()?))
        })
        .collect()
}

/// What `utcp daemon reload-env` did in the daemon.
#[derive(Debug, Clone)]
pub struct EnvReloadSummary {
    /// Variables held afterwards (only names registered manuals use).
    pub total_vars: usize,
    /// The secrets command's run; `None` when none is configured.
    pub secrets_command: Option<SecretsCommandStatus>,
}

/// Pushes this shell's allowed variables to the running daemon, which also re-reads its own
/// environment and runs the secrets command again. `None` when no daemon runs.
pub fn sync_caller_env_to_daemon(paths: &UtcpPaths) -> Result<Option<EnvReloadSummary>> {
    let caller_env = manual_caller_env(paths);
    let req = IpcRequest {
        caller: "cli".into(),
        force_log: false,
        command: IpcCommand::ReloadEnv { caller_env },
    };
    match try_send_ipc_sync(paths, &req)? {
        Some(IpcResponse::Ok {
            data:
                IpcPayload::EnvReloaded {
                    total_vars,
                    secrets_command,
                },
        }) => Ok(Some(EnvReloadSummary {
            total_vars,
            secrets_command,
        })),
        Some(IpcResponse::Error { message }) => Err(anyhow!(message)),
        _ => Ok(None),
    }
}

/// Notify a running daemon to reload its in-memory catalog (no-op if daemon is stopped).
pub fn notify_daemon_reload(paths: &UtcpPaths) {
    let req = IpcRequest {
        caller: "cli".into(),
        force_log: false,
        command: IpcCommand::Reload,
    };
    let _ = try_send_ipc_sync(paths, &req);
}

/// Run the local IPC daemon in the foreground until shutdown signal or optional idle timeout.
pub async fn run_daemon_foreground(
    paths: UtcpPaths,
    idle_timeout_override_mins: Option<u64>,
) -> Result<()> {
    paths.ensure_dirs()?;
    // Held until the socket is gone, so two daemons started at once cannot both bind (the second
    // would delete the first's socket file). Windows needs no lock: the pipe is first-instance-only.
    #[cfg(unix)]
    let start_lock = acquire_daemon_lock(&paths).await?;
    if query_daemon_status(&paths).is_some() {
        return Err(anyhow!(
            "utcp daemon is already running at {}",
            paths.socket_endpoint
        ));
    }

    let settings = UtcpSettings::load(&paths);
    let idle_mins = idle_timeout_override_mins.unwrap_or(settings.daemon_idle_timeout_mins);

    let (log_tx, mut log_rx) = mpsc::unbounded_channel::<ActivityEntry>();
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    // Single-writer background task for opt-in disk activity logs
    let log_paths = paths.clone();
    tokio::spawn(async move {
        while let Some(entry) = log_rx.recv().await {
            let _ = append_activity_entry(&log_paths, &entry);
        }
    });

    // Periodic media spool TTL sweeper (every 5 minutes) so expired tmpfs screenshots are cleaned up
    let sweep_paths = paths.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(300));
        loop {
            ticker.tick().await;
            prune_media_spool(&sweep_paths);
        }
    });

    let state = Arc::new(DaemonServerState::new(
        paths.clone(),
        log_tx,
        shutdown_tx.clone(),
    )?);
    {
        let catalog = state.catalog.read().await;
        for warning in startup_warnings(&paths, &catalog) {
            daemon_notice(&paths, &warning);
        }
    }

    let (watch_tx, mut watch_rx) = mpsc::unbounded_channel::<()>();
    let mut _watcher: Option<RecommendedWatcher> =
        notify::recommended_watcher(move |res: notify::Result<Event>| {
            if let Ok(ev) = res {
                if ev.kind.is_modify() || ev.kind.is_create() || ev.kind.is_remove() {
                    let _ = watch_tx.send(());
                }
            }
        })
        .ok();

    if let Some(w) = _watcher.as_mut() {
        let _ = w.watch(&paths.config_dir, RecursiveMode::NonRecursive);
        let _ = w.watch(&paths.manuals_dir, RecursiveMode::NonRecursive);
    }

    let reload_state = state.clone();
    tokio::spawn(async move {
        while watch_rx.recv().await.is_some() {
            tokio::time::sleep(Duration::from_millis(60)).await;
            while watch_rx.try_recv().is_ok() {}
            let _ = reload_state.reload().await;
        }
    });

    if idle_mins > 0 {
        let idle_state = state.clone();
        let idle_shutdown = shutdown_tx.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(30));
            loop {
                ticker.tick().await;
                let last = idle_state.last_activity_unix_secs.load(Ordering::Relaxed);
                if now_unix_secs().saturating_sub(last) >= idle_mins * 60 {
                    let _ = idle_shutdown.send(true);
                    break;
                }
            }
        });
    }

    let sig_shutdown = shutdown_tx.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sigterm = signal(SignalKind::terminate()).ok();
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = async {
                    match sigterm.as_mut() {
                        Some(s) => { s.recv().await; }
                        None => std::future::pending::<()>().await,
                    }
                } => {}
            }
            let _ = sig_shutdown.send(true);
        }
        #[cfg(not(unix))]
        {
            if tokio::signal::ctrl_c().await.is_ok() {
                let _ = sig_shutdown.send(true);
            }
        }
    });

    #[cfg(unix)]
    {
        let sock_path = Path::new(&paths.socket_endpoint);
        if let Some(parent) = sock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Any socket file here is stale: we hold the lock, and a running daemon from a build
        // without it was caught by the status probe above.
        if sock_path.exists() {
            let _ = std::fs::remove_file(sock_path);
        }
        let listener = tokio::net::UnixListener::bind(sock_path)
            .with_context(|| format!("Failed to bind Unix socket {}", sock_path.display()))?;
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(sock_path, std::fs::Permissions::from_mode(0o600));
        }
        let bound_socket = unix_file_id(sock_path);
        write_daemon_pid_file(&paths);

        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
                accept_res = listener.accept() => {
                    if let Ok((stream, _)) = accept_res {
                        let st = state.clone();
                        tokio::spawn(async move {
                            let _ = handle_async_stream(stream, st).await;
                        });
                    }
                }
            }
        }

        if bound_socket.is_some() && unix_file_id(sock_path) == bound_socket {
            let _ = std::fs::remove_file(sock_path);
        }
        remove_daemon_pid_file_if_ours(&paths);
        // Released as soon as the socket is gone: `daemon restart` starts the next daemon then.
        drop(start_lock);
        Ok(())
    }

    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ServerOptions;
        let mut server = match ServerOptions::new()
            .first_pipe_instance(true)
            .create(&paths.socket_endpoint)
        {
            Ok(server) => server,
            Err(e) => return Err(pipe_create_error(&paths, e).await),
        };
        write_daemon_pid_file(&paths);

        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
                conn_res = server.connect() => {
                    conn_res?;
                    let connected = server;
                    server = ServerOptions::new().create(&paths.socket_endpoint)?;
                    let st = state.clone();
                    tokio::spawn(async move {
                        let _ = handle_async_stream(connected, st).await;
                    });
                }
            }
        }

        remove_daemon_pid_file_if_ours(&paths);
        Ok(())
    }
}

fn write_daemon_pid_file(paths: &UtcpPaths) {
    let _ = std::fs::write(&paths.daemon_pid_file, format!("{}\n", std::process::id()));
}

/// A first-instance pipe that already exists is "access denied". When two daemons start at once,
/// the loser gets here before the winner answers, so wait briefly for it and say what holds the
/// pipe: our running daemon, or (the error `status` gives) another account's or an elevated one.
#[cfg(windows)]
async fn pipe_create_error(paths: &UtcpPaths, err: std::io::Error) -> anyhow::Error {
    if err.kind() == std::io::ErrorKind::PermissionDenied {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match query_daemon_status_checked(paths) {
                Ok(Some(_)) => {
                    return anyhow!(
                        "utcp daemon is already running at {}",
                        paths.socket_endpoint
                    )
                }
                Err(e) => return e,
                Ok(None) => tokio::time::sleep(Duration::from_millis(40)).await,
            }
        }
    }
    anyhow::Error::new(err).context(format!(
        "Failed to create Named Pipe {}",
        paths.socket_endpoint
    ))
}

fn remove_daemon_pid_file_if_ours(paths: &UtcpPaths) {
    let ours = std::fs::read_to_string(&paths.daemon_pid_file)
        .is_ok_and(|s| s.trim() == std::process::id().to_string());
    if ours {
        let _ = std::fs::remove_file(&paths.daemon_pid_file);
    }
}

/// `<socket>.lock`, exclusively locked for the daemon's life. The OS releases it when the process
/// ends, crash included. A daemon that just deleted its socket is about to release it, so a busy
/// lock is retried briefly before giving up.
#[cfg(unix)]
async fn acquire_daemon_lock(paths: &UtcpPaths) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let lock_path = std::path::PathBuf::from(format!("{}.lock", paths.socket_endpoint));
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .with_context(|| format!("Failed to open {}", lock_path.display()))?;
    let contended = fs2::lock_contended_error().raw_os_error();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(file),
            Err(e) if e.raw_os_error() == contended => {
                // Wait only for a daemon that is shutting down; a running one will not let go.
                if Instant::now() >= deadline || query_daemon_status(paths).is_some() {
                    return Err(anyhow!(
                        "utcp daemon is already running at {} (another daemon holds {})",
                        paths.socket_endpoint,
                        lock_path.display()
                    ));
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => {
                return Err(e).with_context(|| format!("Failed to lock {}", lock_path.display()))
            }
        }
    }
}

/// Device and inode, so a daemon removes only the socket file it created itself.
#[cfg(unix)]
fn unix_file_id(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path)
        .ok()
        .map(|m| (m.dev(), m.ino()))
}
