mod daemon;
mod tui;

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::time::Instant;
use utcp_core::activity::{
    activity_detail, append_activity_entry, batch_activity_summary, clear_activity_logs,
    is_logging_enabled, read_recent_activity, set_logging_enabled, ActivityEntry,
};
use utcp_core::caller::detect_caller;
use utcp_core::catalog::{
    batch_call_direct, build_doctor_report, call_tool_direct, deregister_manual, get_schema_sync,
    import_manual_to_file, import_mcp_manual, infer_mcp_manual_name, infer_name_from_source,
    list_tools_sync, load_env_map, manual_hints, normalize_manual_hint, refresh_manuals,
    register_manual, reregister_manual, search_tools_sync, set_manual_hint, BatchRequestItem,
    DeregisterOutcome, UtcpRegistryConfig,
};
use utcp_core::index::ToolSummary;
use utcp_core::ipc::{
    manual_caller_env, notify_daemon_reload, query_daemon_status, sync_caller_env_to_daemon,
    try_send_ipc_sync, IpcCommand, IpcPayload, IpcRequest, IpcResponse, McpServerInfo,
};
use utcp_core::media::{clear_media_spool, inspect_media_spool};
use utcp_core::paths::UtcpPaths;
use utcp_core::skills::{
    inspect_skill_targets, manual_label, refresh_installed_global_skills, registered_manual_labels,
    sync_ide_skills,
};

#[derive(Parser, Debug)]
#[command(
    name = "utcp",
    version,
    about = "Fast, Zero-MCP Universal Tool Calling Protocol (UTCP) CLI, Daemon & TUI"
)]
struct Cli {
    /// Explicit path to `.utcp_config.json` (overrides shared user catalog)
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Force standalone Direct Mode even if `utcp daemon` is running
    #[arg(long, global = true)]
    direct: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Fast summary of all registered tools
    List {
        /// Filter by manual name
        #[arg(long)]
        manual: Option<String>,
        /// Output raw JSON array
        #[arg(long)]
        json: bool,
    },
    /// Ranked keyword/tag search returning compact 1-line summaries by default
    Search {
        /// Search keywords
        query: String,
        /// Maximum number of matches
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Include full input/output JSON schemas in search results
        #[arg(long)]
        schemas: bool,
        /// Output raw JSON array
        #[arg(long)]
        json: bool,
    },
    /// Print the exact JSON input/output schema for a single tool on demand
    Schema {
        /// Qualified tool name (`<manual>.<tool>`)
        target: String,
    },
    /// Execute a registered UTCP tool and print response JSON to stdout
    Call {
        /// Qualified tool name (`<manual>.<tool>`)
        target: String,
        /// Positional JSON arguments object (e.g. `'{"id": 1}'`)
        json_args: Option<String>,
        /// Read JSON arguments from stdin (PowerShell & multiline safe)
        #[arg(long)]
        args_stdin: bool,
        /// Read JSON arguments from file
        #[arg(long)]
        args_file: Option<PathBuf>,
        /// Optional comma-separated fields to project from JSON response (e.g. `sha,commit.message,html_url`).
        /// Dotted paths pass through arrays (`workflow_runs.id`); item keys alone select from a wrapped list.
        #[arg(long, value_delimiter = ',')]
        fields: Vec<String>,
        /// Force pretty-printed multi-line JSON output
        #[arg(short, long, conflicts_with = "compact")]
        pretty: bool,
        /// Force compact single-line JSON output (default for agents and non-TTY pipes)
        #[arg(long)]
        compact: bool,
        /// Preview the resolved request (secrets redacted) without executing
        #[arg(long)]
        dry_run: bool,
        /// Force recording this call to `activity.jsonl` even if global logging is off
        #[arg(long)]
        log: bool,
    },
    /// Execute multiple tool calls concurrently over a shared connection pool
    Batch {
        /// Read batch JSON array from stdin
        #[arg(long)]
        stdin: bool,
        /// Read batch JSON array from file
        #[arg(long)]
        file: Option<PathBuf>,
        /// Maximum concurrent calls
        #[arg(long, default_value_t = 8)]
        concurrency: usize,
        /// Optional default comma-separated fields applied to items without their own `fields`
        #[arg(long, value_delimiter = ',')]
        fields: Vec<String>,
        /// Force pretty-printed multi-line JSON output
        #[arg(short, long, conflicts_with = "compact")]
        pretty: bool,
        /// Force compact single-line JSON output (default for agents and non-TTY pipes)
        #[arg(long)]
        compact: bool,
        /// Force recording this batch to `activity.jsonl`
        #[arg(long)]
        log: bool,
    },
    /// Manage the optional `--user` machine-wide local IPC daemon
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Convert an OpenAPI (JSON/YAML) or UTCP spec to a standalone `.utcp.json` file ("cache-before-register")
    Import {
        /// URL or local file path
        source: String,
        /// Output `.utcp.json` file path
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Manual namespace name
        #[arg(long)]
        name: Option<String>,
        /// Base URL override
        #[arg(long)]
        base_url: Option<String>,
        /// Extra static HTTP headers (`Key:Value`)
        #[arg(short = 'H', long = "header")]
        headers: Vec<String>,
        /// Also register into `.utcp_config.json` and rebuild `search_index.bin`
        #[arg(long)]
        register: bool,
        /// A few words on what the manual is for, shown next to its name in the agent skill (needs `--register`)
        #[arg(long, requires = "register")]
        hint: Option<String>,
    },
    /// Discover tools from an MCP server (`stdio` command or `http` URL) and write a lazy `.utcp.json` manual
    ImportMcp {
        /// Manual namespace name (default: inferred from the URL host or the server package)
        name: Option<String>,
        /// Local MCP server executable only, e.g. `npx` (stdio transport); pass its arguments with `--arg`
        #[arg(long, conflicts_with = "url")]
        command: Option<String>,
        /// One argument for `--command`; repeat per argument (`--arg -y --arg @scope/server`)
        #[arg(long = "arg", allow_hyphen_values = true)]
        args: Vec<String>,
        /// Environment variables passed to `--command` (`KEY=VALUE` or `KEY=${ENV_VAR}`)
        #[arg(short = 'e', long = "env")]
        envs: Vec<String>,
        /// Remote MCP server HTTP URL
        #[arg(long)]
        url: Option<String>,
        /// Extra HTTP headers for `--url` (`Key:Value`)
        #[arg(short = 'H', long = "header")]
        headers: Vec<String>,
        /// Output `.utcp.json` file path (optional when `--register` is used)
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Also register into `.utcp_config.json` and rebuild `search_index.bin`
        #[arg(long)]
        register: bool,
        /// A few words on what the manual is for, shown next to its name in the agent skill (needs `--register`)
        #[arg(long, requires = "register")]
        hint: Option<String>,
    },
    /// Register an OpenAPI spec or UTCP manual into `.utcp_config.json` and rebuild `search_index.bin`.
    /// With no source, re-registers a deregistered manual from its saved copy (offline).
    Register {
        /// Manual namespace name
        name: String,
        /// URL or local file path. Omit to re-register a deregistered manual from its saved copy
        source: Option<String>,
        /// Force caching manual to disk
        #[arg(long, conflicts_with = "no_cache")]
        cache: bool,
        /// Mark manual for live refresh instead of pinned cache
        #[arg(long)]
        no_cache: bool,
        /// Base URL override
        #[arg(long)]
        base_url: Option<String>,
        /// Extra static HTTP headers (`Key:Value`)
        #[arg(short = 'H', long = "header")]
        headers: Vec<String>,
        /// A few words on what the manual is for, shown next to its name in the agent skill
        /// (e.g. `--hint 'library and framework docs'`; kept when omitted)
        #[arg(long)]
        hint: Option<String>,
    },
    /// Set or clear a manual's hint: the few words shown next to its name in the agent skill's
    /// description, so agents know what an unclear name is for. Refreshes installed skills
    SetHint {
        /// Manual namespace name (registered or deregistered)
        name: String,
        /// Hint text, e.g. 'library and framework docs' (at most 60 characters)
        #[arg(required_unless_present = "clear", conflicts_with = "clear")]
        text: Option<String>,
        /// Remove the hint
        #[arg(long)]
        clear: bool,
    },
    /// Show a manual's hint, or every registered manual with its hint
    GetHint {
        /// Manual namespace name (lists all registered manuals when omitted)
        name: Option<String>,
    },
    /// Remove a manual from search and calls and rebuild `search_index.bin`. Its settings and saved
    /// copy are kept so `utcp register <name>` (no source) can restore it
    Deregister {
        /// Manual namespace name
        name: String,
        /// Delete it for good: remove the registry entry and the saved `manuals/<name>.utcp.json`
        /// (also works on an already-deregistered manual or an orphaned saved file)
        #[arg(long)]
        purge_cache: bool,
    },
    /// Re-fetch one or all registered manuals from their source URL and rebuild `search_index.bin`
    Refresh {
        /// Optional manual name (refreshes all if omitted)
        name: Option<String>,
    },
    /// Control opt-in disk activity logging (`activity.jsonl` + `.bak`)
    Log {
        #[command(subcommand)]
        action: LogAction,
    },
    /// Inspect or clear the bounded `tmpfs` media spool (`$XDG_RUNTIME_DIR/utcp-media`)
    Media {
        #[command(subcommand)]
        action: MediaAction,
    },
    /// Check daemon status, config paths, index freshness, credentials (and where each comes from),
    /// the `[secrets]` command, `settings.toml`, media spool, and IDE skills
    Doctor {
        /// Output raw JSON
        #[arg(long)]
        json: bool,
    },
    /// Install or repair the platform-native `utcp-tools` and `utcp-admin` skills across Zed, VS Code,
    /// Copilot CLI, Antigravity CLI (`agy`) and Hermes
    SyncIdes {
        /// Write the global skills folders (`~/.agents/skills`, `~/.copilot/skills`,
        /// `~/.gemini/antigravity-cli/skills`, `~/.hermes/skills/software-development`)
        #[arg(long)]
        global: bool,
        /// Write the skills into `<dir>/.agents/skills/`
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Only check status without writing files
        #[arg(long)]
        status: bool,
        /// Print the rendered `utcp-tools` SKILL.md to stdout (useful for packaging / ebuilds)
        #[arg(long)]
        print_skill: bool,
    },
    /// Launch the interactive Ratatui companion Terminal UI (read-only until acted upon)
    Ui,
}

#[derive(Subcommand, Debug)]
enum DaemonAction {
    /// Start `utcp daemon` in the background
    Start {
        /// Auto-exit after N minutes of inactivity (0 = stay resident)
        #[arg(long)]
        idle_timeout: Option<u64>,
    },
    /// Gracefully stop the running `utcp daemon`
    Stop,
    /// Show `utcp daemon` status, count of env vars used by manuals, and media spool
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Restart `utcp daemon`
    Restart {
        #[arg(long)]
        idle_timeout: Option<u64>,
    },
    /// Push the variables registered manuals use from the current shell into the running `utcp daemon`
    /// without restarting (all other variables are ignored). The daemon also re-reads its own
    /// environment and `.env`, and runs the `[secrets]` command again
    ReloadEnv,
    /// List the local MCP servers (`import-mcp --command` manuals) the daemon runs: state, pid,
    /// uptime and restart count
    Servers {
        /// Output raw JSON array
        #[arg(long)]
        json: bool,
    },
    /// Stop a manual's local MCP server now. Its waiting calls fail at once and its next call
    /// starts it again (to keep it off, `utcp deregister` the manual)
    StopServer {
        /// Manual namespace name
        manual: String,
    },
    /// Stop and start a manual's local MCP server now (for a hung or misbehaving server)
    RestartServer {
        /// Manual namespace name
        manual: String,
    },
    /// Run `utcp daemon` in the foreground (used by systemd `--user` / `daemon start`)
    Run {
        #[arg(long)]
        idle_timeout: Option<u64>,
    },
}

#[derive(Subcommand, Debug)]
enum LogAction {
    /// Enable persistent disk activity logging (`logging.enabled`)
    On,
    /// Disable persistent disk activity logging
    Off,
    /// Show current logging status and log file sizes
    Status,
    /// Delete `activity.jsonl` and `activity.jsonl.bak`
    Clear,
    /// Show the most recent calls, newest first, with the full (redacted) error text of failures.
    /// Reads the running daemon's in-memory list (last 500 calls), so it works with logging off;
    /// without a daemon it falls back to the log files.
    Recent {
        /// How many calls to show
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Only show failed calls
        #[arg(long)]
        failed: bool,
        /// Output raw JSON array
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum MediaAction {
    /// Show media spool directory, file count, byte usage, and retention policy
    Status,
    /// Delete all spooled media files immediately
    Clear,
}

fn main() {
    let result = run_cli();
    // A stdio MCP server stopped during this command (discovery, a direct-mode call, daemon
    // shutdown) gets a grace period after SIGTERM; don't exit before it is fully stopped.
    utcp_core::transports::wait_for_stopping_servers(std::time::Duration::from_secs(5));
    if let Err(err) = result {
        if let Some(Interrupted(signal)) = err.downcast_ref::<Interrupted>() {
            eprintln!("utcp: interrupted ({})", signal_name(*signal));
            std::process::exit(128 + signal);
        }
        eprintln!("Error: {:#}", err);
        std::process::exit(1);
    }
}

/// A direct-mode command stopped by a signal. `main` exits with `128 + signal`, like a shell.
#[derive(Debug)]
struct Interrupted(i32);

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "interrupted ({})", signal_name(self.0))
    }
}

impl std::error::Error for Interrupted {}

const SIGHUP: i32 = 1;
const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;

fn signal_name(signal: i32) -> &'static str {
    match signal {
        SIGHUP => "SIGHUP",
        SIGTERM => "SIGTERM",
        _ => "SIGINT",
    }
}

/// Waits for Ctrl+C, or on Unix also SIGTERM or SIGHUP, and returns the signal number.
async fn stop_signal() -> i32 {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        async fn recv(kind: SignalKind) {
            match signal(kind) {
                Ok(mut s) => {
                    s.recv().await;
                }
                Err(_) => std::future::pending().await,
            }
        }
        tokio::select! {
            _ = recv(SignalKind::interrupt()) => SIGINT,
            _ = recv(SignalKind::terminate()) => SIGTERM,
            _ = recv(SignalKind::hangup()) => SIGHUP,
        }
    }
    #[cfg(not(unix))]
    {
        match tokio::signal::ctrl_c().await {
            Ok(()) => SIGINT,
            Err(_) => std::future::pending().await,
        }
    }
}

/// Run direct-mode work on `rt`, stopping it on Ctrl+C / SIGTERM / SIGHUP. Dropping the work
/// stops any stdio MCP server it started (with its whole process tree), so an interrupted
/// `utcp --direct call` or `import-mcp` never leaves one running.
fn block_on_interruptible<F: std::future::Future>(
    rt: &tokio::runtime::Runtime,
    work: F,
) -> Result<F::Output> {
    rt.block_on(async {
        tokio::select! {
            out = work => Ok(out),
            signal = stop_signal() => Err(Interrupted(signal).into()),
        }
    })
}

fn run_cli() -> Result<()> {
    let cli = Cli::parse();
    let paths = UtcpPaths::resolve(cli.config.as_deref())?;

    match cli.command {
        Commands::List { manual, json } => {
            let started = Instant::now();
            let caller = detect_caller();

            if !cli.direct {
                let req = IpcRequest {
                    caller: caller.clone(),
                    force_log: false,
                    command: IpcCommand::List {
                        manual: manual.clone(),
                    },
                };
                if let Some(resp) = try_send_ipc_sync(&paths, &req)? {
                    return match resp {
                        IpcResponse::Ok {
                            data: IpcPayload::Tools(tools),
                        } => {
                            print_tool_summaries(&tools, json, false)?;
                            Ok(())
                        }
                        IpcResponse::Error { message } => Err(anyhow!(message)),
                        _ => Err(anyhow!("Unexpected IPC response payload")),
                    };
                }
            }

            let tools = list_tools_sync(&paths, manual.as_deref())?;
            maybe_log_direct(
                &paths,
                &caller,
                "list",
                manual.as_deref().unwrap_or("*"),
                true,
                "OK",
                started,
                false,
                None,
            );
            print_tool_summaries(&tools, json, false)
        }

        Commands::Search {
            query,
            limit,
            schemas,
            json,
        } => {
            let started = Instant::now();
            let caller = detect_caller();

            if !cli.direct {
                let req = IpcRequest {
                    caller: caller.clone(),
                    force_log: false,
                    command: IpcCommand::Search {
                        query: query.clone(),
                        limit,
                        include_schemas: schemas,
                    },
                };
                if let Some(resp) = try_send_ipc_sync(&paths, &req)? {
                    return match resp {
                        IpcResponse::Ok {
                            data: IpcPayload::Tools(tools),
                        } => {
                            print_tool_summaries(&tools, json, schemas)?;
                            Ok(())
                        }
                        IpcResponse::Error { message } => Err(anyhow!(message)),
                        _ => Err(anyhow!("Unexpected IPC response payload")),
                    };
                }
            }

            let tools = search_tools_sync(&paths, &query, limit, schemas)?;
            maybe_log_direct(
                &paths,
                &caller,
                "search",
                &query,
                true,
                &format!("{} hits", tools.len()),
                started,
                false,
                None,
            );
            print_tool_summaries(&tools, json, schemas)
        }

        Commands::Schema { target } => {
            let started = Instant::now();
            let caller = detect_caller();

            if !cli.direct {
                let req = IpcRequest {
                    caller: caller.clone(),
                    force_log: false,
                    command: IpcCommand::Schema {
                        target: target.clone(),
                    },
                };
                if let Some(resp) = try_send_ipc_sync(&paths, &req)? {
                    return match resp {
                        IpcResponse::Ok {
                            data: IpcPayload::Schema(schema),
                        } => {
                            println!("{}", serde_json::to_string_pretty(&schema)?);
                            Ok(())
                        }
                        IpcResponse::Error { message } => Err(anyhow!(message)),
                        _ => Err(anyhow!("Unexpected IPC response payload")),
                    };
                }
            }

            let schema = get_schema_sync(&paths, &target)?;
            maybe_log_direct(
                &paths, &caller, "schema", &target, true, "OK", started, false, None,
            );
            println!("{}", serde_json::to_string_pretty(&schema)?);
            Ok(())
        }

        Commands::Call {
            target,
            json_args,
            args_stdin,
            args_file,
            fields,
            pretty,
            compact,
            dry_run,
            log,
        } => {
            let args = parse_call_args(json_args, args_stdin, args_file)?;
            let started = Instant::now();
            let caller = detect_caller();
            let fields_opt = if fields.is_empty() {
                None
            } else {
                Some(fields)
            };

            if !cli.direct {
                let req = IpcRequest {
                    caller: caller.clone(),
                    force_log: log,
                    command: IpcCommand::Call {
                        target: target.clone(),
                        args: args.clone(),
                        dry_run,
                        fields: fields_opt.clone(),
                        caller_env: manual_caller_env(&paths),
                    },
                };
                if let Some(resp) = try_send_ipc_sync(&paths, &req)? {
                    return match resp {
                        IpcResponse::Ok {
                            data: IpcPayload::CallResult(val),
                        } => print_json_output(&val, pretty, compact, &caller),
                        IpcResponse::Error { message } => Err(anyhow!(message)),
                        _ => Err(anyhow!("Unexpected IPC response payload")),
                    };
                }
            }

            let rt = build_tokio_runtime()?;
            let outcome = block_on_interruptible(
                &rt,
                call_tool_direct(&paths, &target, args, dry_run, fields_opt.as_deref()),
            )?;
            match outcome {
                Ok(val) => {
                    maybe_log_direct(
                        &paths,
                        &caller,
                        if dry_run { "dry-run" } else { "call" },
                        &target,
                        true,
                        "OK",
                        started,
                        log,
                        None,
                    );
                    print_json_output(&val, pretty, compact, &caller)
                }
                Err(err) => {
                    let detail = activity_detail(&format!("{:#}", err), &load_env_map(&paths));
                    maybe_log_direct(
                        &paths,
                        &caller,
                        if dry_run { "dry-run" } else { "call" },
                        &target,
                        false,
                        "ERR",
                        started,
                        log,
                        Some(detail),
                    );
                    Err(err)
                }
            }
        }

        Commands::Batch {
            stdin,
            file,
            concurrency,
            fields,
            pretty,
            compact,
            log,
        } => {
            let mut items = parse_batch_items(stdin, file)?;
            if !fields.is_empty() {
                for item in &mut items {
                    if item.fields.is_none() {
                        item.fields = Some(fields.clone());
                    }
                }
            }
            let started = Instant::now();
            let caller = detect_caller();
            let count = items.len();

            if !cli.direct {
                let req = IpcRequest {
                    caller: caller.clone(),
                    force_log: log,
                    command: IpcCommand::Batch {
                        items: items.clone(),
                        concurrency,
                        caller_env: manual_caller_env(&paths),
                    },
                };
                if let Some(resp) = try_send_ipc_sync(&paths, &req)? {
                    return match resp {
                        IpcResponse::Ok {
                            data: IpcPayload::BatchResults(results),
                        } => print_json_output(&results, pretty, compact, &caller),
                        IpcResponse::Error { message } => Err(anyhow!(message)),
                        _ => Err(anyhow!("Unexpected IPC response payload")),
                    };
                }
            }

            let rt = build_tokio_runtime()?;
            let results =
                block_on_interruptible(&rt, batch_call_direct(&paths, items, concurrency))??;
            let env_for_redaction = if results.iter().all(|r| r.ok) {
                HashMap::new()
            } else {
                load_env_map(&paths)
            };
            let (all_ok, status, detail) = batch_activity_summary(&results, &env_for_redaction);
            maybe_log_direct(
                &paths,
                &caller,
                "batch",
                &format!("{} calls", count),
                all_ok,
                &status,
                started,
                log,
                detail,
            );
            print_json_output(&results, pretty, compact, &caller)
        }

        Commands::Daemon { action } => match action {
            DaemonAction::Start { idle_timeout } => {
                let info =
                    daemon::start_daemon_background(&paths, cli.config.as_deref(), idle_timeout)?;
                println!(
                    "utcp daemon running (PID {}, socket: {}, manuals: {}, tools: {}, env vars used by manuals: {})",
                    info.pid,
                    info.socket_endpoint,
                    info.manual_count,
                    info.tool_count,
                    info.env_var_count
                );
                Ok(())
            }
            DaemonAction::Stop => {
                if daemon::stop_daemon(&paths)? {
                    println!("Stopped utcp daemon.");
                } else {
                    println!("utcp daemon is not running.");
                }
                Ok(())
            }
            DaemonAction::Status { json } => {
                if let Some(info) = utcp_core::ipc::query_daemon_status_checked(&paths)? {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&info)?);
                    } else {
                        println!(
                            "Status       : RUNNING\nPID          : {}\nSocket       : {}\nUptime       : {}s\nManuals      : {}\nTools        : {}\nServed       : {}\nEnv Vars     : {} (used by manuals)\nsystemd user : {}\nMedia Spool  : {} ({} files, {} bytes)\nLogging      : {}",
                            info.pid,
                            info.socket_endpoint,
                            info.uptime_secs,
                            info.manual_count,
                            info.tool_count,
                            info.requests_served,
                            info.env_var_count,
                            info.systemd_user_state.as_deref().unwrap_or("n/a"),
                            info.media_spool.dir,
                            info.media_spool.file_count,
                            info.media_spool.total_bytes,
                            if info.logging_enabled { "ON" } else { "OFF" }
                        );
                        if let Some(s) = &info.secrets_command {
                            match &s.error {
                                Some(error) => println!("Secrets cmd  : FAILED: {error}"),
                                None => println!(
                                    "Secrets cmd  : OK, printed {} of the wanted names",
                                    s.provided.len()
                                ),
                            }
                        }
                    }
                } else if json {
                    let log = utcp_core::daemon_log::daemon_log_file(&paths);
                    println!(
                        "{}",
                        serde_json::json!({
                            "running": false,
                            "socket_endpoint": paths.socket_endpoint,
                            "daemon_log": log.is_file().then(|| log.display().to_string()),
                        })
                    );
                } else {
                    println!(
                        "Status  : STOPPED (CLI operating in Standalone Direct Mode)\nSocket  : {}",
                        paths.socket_endpoint
                    );
                    if let Some(line) = daemon_log_status_line(&paths) {
                        println!("{line}");
                    }
                }
                Ok(())
            }
            DaemonAction::Restart { idle_timeout } => {
                let _ = daemon::stop_daemon(&paths)?;
                let info =
                    daemon::start_daemon_background(&paths, cli.config.as_deref(), idle_timeout)?;
                println!(
                    "Restarted utcp daemon (PID {}, socket: {}, env vars used by manuals: {})",
                    info.pid, info.socket_endpoint, info.env_var_count
                );
                Ok(())
            }
            DaemonAction::ReloadEnv => match sync_caller_env_to_daemon(&paths)? {
                Some(summary) => {
                    println!(
                        "Synchronized the variables registered manuals use from this shell into the running utcp daemon ({} held; all others ignored).",
                        summary.total_vars
                    );
                    if let Some(s) = summary.secrets_command {
                        match s.error {
                            Some(error) => println!("Secrets command failed: {error}"),
                            None => println!(
                                "Secrets command re-run: printed {} of the wanted names.",
                                s.provided.len()
                            ),
                        }
                    }
                    Ok(())
                }
                None => Err(anyhow!(
                    "utcp daemon is not currently running (start it with `utcp daemon start`)"
                )),
            },
            DaemonAction::Run { idle_timeout } => daemon::execute_daemon_run(paths, idle_timeout),
            DaemonAction::Servers { json } => {
                match daemon_server_request(&paths, IpcCommand::Servers)? {
                    IpcPayload::Servers(servers) => print_servers(&servers, json),
                    _ => Err(anyhow!("Unexpected IPC response payload")),
                }
            }
            DaemonAction::StopServer { manual } => {
                match daemon_server_request(&paths, IpcCommand::StopServer { manual })? {
                    IpcPayload::ServerStopped {
                        manual,
                        was_running: true,
                    } => {
                        println!(
                            "Stopped the MCP server of '{}'. Its next call starts it again.",
                            manual
                        );
                        Ok(())
                    }
                    IpcPayload::ServerStopped { manual, .. } => {
                        println!(
                            "The MCP server of '{}' was not running. Its next call starts it.",
                            manual
                        );
                        Ok(())
                    }
                    _ => Err(anyhow!("Unexpected IPC response payload")),
                }
            }
            DaemonAction::RestartServer { manual } => {
                match daemon_server_request(&paths, IpcCommand::RestartServer { manual })? {
                    IpcPayload::Servers(servers) => {
                        for s in &servers {
                            println!(
                                "Restarted the MCP server of '{}' (PID {}).",
                                s.manual,
                                s.pid.map_or("?".to_string(), |p| p.to_string())
                            );
                        }
                        Ok(())
                    }
                    _ => Err(anyhow!("Unexpected IPC response payload")),
                }
            }
        },

        Commands::Import {
            source,
            output,
            name,
            base_url,
            headers,
            register,
            hint,
        } => {
            let parsed_headers = parse_header_flags(&headers)?;
            let rt = build_tokio_runtime()?;
            let summary = if register {
                let reg_name = name.unwrap_or_else(|| infer_name_from_source(&source));
                let s = block_on_interruptible(
                    &rt,
                    register_manual(
                        &paths,
                        &reg_name,
                        &source,
                        Some(true),
                        base_url.as_deref(),
                        parsed_headers,
                        hint.as_deref(),
                    ),
                )??;
                notify_daemon_reload(&paths);
                // `-o` with `--register`: also write a standalone copy, as the TUI's import does.
                if let Some(out) = &output {
                    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::copy(&s.output_path, out).with_context(|| {
                        format!("registered, but copying to {} failed", out.display())
                    })?;
                }
                s
            } else {
                block_on_interruptible(
                    &rt,
                    import_manual_to_file(
                        &paths,
                        &source,
                        name.as_deref(),
                        output.as_deref(),
                        base_url.as_deref(),
                        &parsed_headers,
                    ),
                )??
            };

            print_import_summary(&summary);
            if let (true, Some(out)) = (register, &output) {
                println!("Copy      : {}", out.display());
            }
            if register {
                print_hint_line(&paths, &summary.manual_name);
            }
            Ok(())
        }

        Commands::ImportMcp {
            name,
            command,
            args,
            envs,
            url,
            headers,
            output,
            register,
            hint,
        } => {
            let parsed_headers = parse_header_flags(&headers)?;
            let parsed_envs = parse_env_flags(&envs)?;
            let name = name.unwrap_or_else(|| {
                infer_mcp_manual_name(command.as_deref(), &args, url.as_deref())
            });
            let rt = build_tokio_runtime()?;
            let summary = block_on_interruptible(
                &rt,
                import_mcp_manual(
                    &paths,
                    &name,
                    command.as_deref(),
                    args,
                    parsed_envs,
                    url.as_deref(),
                    parsed_headers,
                    output.as_deref(),
                    register,
                    hint.as_deref(),
                ),
            )??;
            if register {
                notify_daemon_reload(&paths);
            }
            print_import_summary(&summary);
            if register {
                print_hint_line(&paths, &summary.manual_name);
            }
            Ok(())
        }

        Commands::Register {
            name,
            source,
            cache,
            no_cache,
            base_url,
            headers,
            hint,
        } => {
            let Some(source) = source else {
                if cache || no_cache || base_url.is_some() || !headers.is_empty() {
                    return Err(anyhow!(
                        "--cache, --no-cache, --base-url and -H need a source; re-registering '{}' \
                         from its saved copy keeps its previous settings",
                        name
                    ));
                }
                if let Some(h) = &hint {
                    normalize_manual_hint(h)?;
                }
                let summary = reregister_manual(&paths, &name)?;
                if let Some(h) = &hint {
                    set_manual_hint(&paths, &name, Some(h))?;
                    refresh_installed_global_skills(&paths);
                }
                notify_daemon_reload(&paths);
                println!(
                    "Re-registered '{}' from its saved copy ({} tools) -> {}",
                    summary.manual_name, summary.tool_count, summary.output_path
                );
                print_hint_line(&paths, &summary.manual_name);
                return Ok(());
            };
            let cache_override = if cache {
                Some(true)
            } else if no_cache {
                Some(false)
            } else {
                None
            };
            let parsed_headers = parse_header_flags(&headers)?;
            let rt = build_tokio_runtime()?;
            let summary = block_on_interruptible(
                &rt,
                register_manual(
                    &paths,
                    &name,
                    &source,
                    cache_override,
                    base_url.as_deref(),
                    parsed_headers,
                    hint.as_deref(),
                ),
            )??;
            notify_daemon_reload(&paths);

            println!(
                "Registered '{}' ({} tools, protocols: {}) -> {}",
                summary.manual_name,
                summary.tool_count,
                summary.protocols.join(","),
                summary.output_path
            );
            if !summary.required_env_vars.is_empty() {
                println!(
                    "Required credentials (env or {}): {}",
                    paths.env_file.display(),
                    summary.required_env_vars.join(", ")
                );
            }
            print_hint_line(&paths, &summary.manual_name);
            Ok(())
        }

        Commands::SetHint { name, text, clear } => {
            let text = if clear { None } else { text };
            let update = set_manual_hint(&paths, &name, text.as_deref())?;
            if update.active {
                // Hint words are part of the search index, which the daemon keeps in memory.
                notify_daemon_reload(&paths);
                refresh_installed_global_skills(&paths);
            }
            match (&update.hint, update.active) {
                (Some(h), true) => println!(
                    "Hint for '{}' set. Installed skills now list: {}",
                    name,
                    manual_label(&name, Some(h))
                ),
                (None, true) => println!(
                    "Hint for '{}' cleared. Installed skills now list: {}",
                    name, name
                ),
                (Some(h), false) => println!(
                    "Hint for '{}' set to '{}'. It is deregistered, so skills list it again after \
                     `utcp register {}`.",
                    name, h, name
                ),
                (None, false) => println!("Hint for '{}' cleared (manual is deregistered).", name),
            }
            Ok(())
        }

        Commands::GetHint { name } => {
            let registry = UtcpRegistryConfig::load(&paths)?;
            match name {
                Some(name) => {
                    let entry = registry
                        .manuals
                        .get(&name)
                        .or_else(|| registry.deregistered.get(&name))
                        .ok_or_else(|| anyhow!("No manual named '{}'", name))?;
                    match &entry.hint {
                        Some(h) => println!("{}", h),
                        None => println!(
                            "No hint set for '{}'; agents see only its name. Add one: utcp set-hint {} \"<few words>\"",
                            name, name
                        ),
                    }
                }
                None => {
                    if registry.manuals.is_empty() {
                        println!("No manuals registered.");
                    }
                    let width = registry.manuals.keys().map(String::len).max().unwrap_or(0);
                    for (n, e) in &registry.manuals {
                        println!("{:<width$}  {}", n, e.hint.as_deref().unwrap_or("-"));
                    }
                }
            }
            Ok(())
        }

        Commands::Deregister { name, purge_cache } => {
            match deregister_manual(&paths, &name, purge_cache)? {
                DeregisterOutcome::Deregistered => {
                    notify_daemon_reload(&paths);
                    println!(
                        "Deregistered manual '{}'. Restore it with `utcp register {}`; delete it for \
                         good with `utcp deregister {} --purge-cache`.",
                        name, name, name
                    );
                }
                DeregisterOutcome::Purged => {
                    notify_daemon_reload(&paths);
                    println!("Purged manual '{}' (registry entry and saved copy).", name);
                }
                DeregisterOutcome::NotFound => {
                    println!("Manual '{}' was not registered.", name);
                }
            }
            Ok(())
        }

        Commands::Refresh { name } => {
            let rt = build_tokio_runtime()?;
            let summaries = block_on_interruptible(&rt, refresh_manuals(&paths, name.as_deref()))??;
            notify_daemon_reload(&paths);
            for s in summaries {
                println!(
                    "Refreshed '{}' ({} tools, sha256: {})",
                    s.manual_name,
                    s.tool_count,
                    &s.sha256[..12.min(s.sha256.len())]
                );
            }
            Ok(())
        }

        Commands::Log { action } => match action {
            LogAction::On => {
                set_logging_enabled(&paths, true)?;
                println!(
                    "Activity disk logging ENABLED ({}).",
                    paths.activity_log_file.display()
                );
                Ok(())
            }
            LogAction::Off => {
                set_logging_enabled(&paths, false)?;
                println!("Activity disk logging DISABLED.");
                Ok(())
            }
            LogAction::Status => {
                let enabled = is_logging_enabled(&paths);
                let cur_size = std::fs::metadata(&paths.activity_log_file)
                    .map(|m| m.len())
                    .unwrap_or(0);
                let bak_size = std::fs::metadata(&paths.activity_log_bak)
                    .map(|m| m.len())
                    .unwrap_or(0);
                println!("Logging      : {}", if enabled { "ON" } else { "OFF" });
                println!(
                    "Active file  : {} ({} bytes)",
                    paths.activity_log_file.display(),
                    cur_size
                );
                println!(
                    "Backup file  : {} ({} bytes)",
                    paths.activity_log_bak.display(),
                    bak_size
                );
                Ok(())
            }
            LogAction::Clear => {
                clear_activity_logs(&paths)?;
                println!("Cleared activity logs.");
                Ok(())
            }
            LogAction::Recent {
                limit,
                failed,
                json,
            } => print_recent_activity(&paths, limit, failed, json),
        },

        Commands::Media { action } => match action {
            MediaAction::Status => {
                let st = inspect_media_spool(&paths);
                println!("Media Spool : {}", st.dir);
                println!("Files       : {} / {} max", st.file_count, st.max_files);
                println!("Total Size  : {} bytes", st.total_bytes);
                println!("TTL         : {} minutes", st.ttl_mins);
                Ok(())
            }
            MediaAction::Clear => {
                let count = clear_media_spool(&paths)?;
                println!(
                    "Cleared {} spooled media file(s) from {}.",
                    count,
                    paths.media_dir.display()
                );
                Ok(())
            }
        },

        Commands::Doctor { json } => {
            let d_info = query_daemon_status(&paths);
            let mut report =
                build_doctor_report(&paths, d_info.is_some(), d_info.as_ref().map(|d| d.pid))?;
            // A running daemon resolves every call, so its view of the credentials is the one that
            // matters (this shell's may differ).
            if let Some(info) = &d_info {
                report.credentials = info.credentials.clone();
                report.secrets_command = info.secrets_command.clone();
            }
            let media_spool = inspect_media_spool(&paths);
            let cwd = std::env::current_dir().ok();
            let skills = inspect_skill_targets(&paths, true, cwd.as_deref());

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "doctor": report,
                        "media_spool": media_spool,
                        "skills": skills,
                    }))?
                );
                return Ok(());
            }

            println!("=== utcp doctor ===");
            println!(
                "Execution Mode    : {}",
                if let Some(pid) = report.daemon_pid {
                    format!("DAEMON RUNNING (PID {})", pid)
                } else {
                    "STANDALONE DIRECT MODE (daemon stopped)".to_string()
                }
            );
            println!("Socket Endpoint   : {}", report.socket_endpoint);
            println!("Registry Config   : {}", report.config_file);
            println!("Cache Directory   : {}", report.cache_dir);
            println!("State Directory   : {}", report.state_dir);
            println!(
                "Media Spool       : {} ({}/{} files, {} bytes, TTL {}m)",
                media_spool.dir,
                media_spool.file_count,
                media_spool.max_files,
                media_spool.total_bytes,
                media_spool.ttl_mins
            );
            println!(
                "Search Index      : {} ({} manuals, {} tools)",
                if report.index_present {
                    "PRESENT"
                } else {
                    "NOT YET BUILT"
                },
                report.manual_count,
                report.tool_count
            );
            println!("cache_on_register : {}", report.cache_on_register);
            println!(
                "Activity Logging  : {}",
                if report.logging_enabled {
                    "ON"
                } else {
                    "OFF (Opt-In)"
                }
            );

            if let Some(error) = &report.settings_error {
                println!("Settings          : NOT USED, the defaults apply: {error}");
            }
            if let Some(s) = &report.secrets_command {
                match &s.error {
                    Some(error) => println!("Secrets Command   : FAILED: {error}\n                    `{}`", s.command),
                    None => println!(
                        "Secrets Command   : OK, printed {} of the wanted names\n                    `{}`",
                        s.provided.len(),
                        s.command
                    ),
                }
            }

            if !report.credentials.is_empty() {
                if d_info.is_some() {
                    println!("\nCredentials (as the running daemon sees them):");
                } else {
                    println!(
                        "\nCredentials (secrets command, environment, {}):",
                        paths.env_file.display()
                    );
                }
                for c in &report.credentials {
                    if c.is_set {
                        let via = c.resolved_via.as_deref().unwrap_or(&c.env_var);
                        let via = match &c.source {
                            Some(source) => format!("{via}, from {source}"),
                            None => via.to_string(),
                        };
                        println!(
                            "  [SET]     {:<24} (manual: {}, via {})",
                            c.env_var, c.manual, via
                        );
                    } else {
                        println!("  [MISSING] {:<24} (manual: {})", c.env_var, c.manual);
                    }
                }
            }

            println!("\nAgent Skills (utcp-tools + utcp-admin in each skills folder):");
            for s in skills {
                let tag = if s.up_to_date {
                    "UP TO DATE"
                } else if s.exists {
                    "OUTDATED"
                } else {
                    "NOT INSTALLED"
                };
                println!("  [{:<13}] {} -> {}", tag, s.client, s.path);
            }
            Ok(())
        }

        Commands::SyncIdes {
            global,
            workspace,
            status,
            print_skill,
        } => {
            if print_skill {
                let labels = registered_manual_labels(&paths);
                print!(
                    "{}",
                    utcp_core::skills::render_skill_markdown(
                        utcp_core::skills::SkillTargetPlatform::current(),
                        &labels
                    )
                );
                return Ok(());
            }

            let include_global = global || workspace.is_none();
            if status {
                let items = inspect_skill_targets(&paths, include_global, workspace.as_deref());
                for s in items {
                    let tag = if s.up_to_date {
                        "UP TO DATE"
                    } else if s.exists {
                        "OUTDATED"
                    } else {
                        "NOT INSTALLED"
                    };
                    println!("[{:<13}] {} -> {}", tag, s.client, s.path);
                }
                return Ok(());
            }

            let synced = sync_ide_skills(&paths, include_global, workspace.as_deref())?;
            for s in synced {
                println!("[SYNCED] {} -> {}", s.client, s.path);
            }
            Ok(())
        }

        Commands::Ui => tui::run_tui(paths, cli.config.as_deref()),
    }
}

fn print_import_summary(summary: &utcp_core::catalog::ImportSummary) {
    println!("Manual    : {}", summary.manual_name);
    println!("Output    : {}", summary.output_path);
    println!("Source    : {}", summary.source_url);
    println!("Protocols : {}", summary.protocols.join(", "));
    println!("Tools     : {}", summary.tool_count);
    println!("SHA-256   : {}", summary.sha256);
    if summary.required_env_vars.is_empty() {
        println!("Secrets   : none required");
    } else {
        println!("Secrets   : {}", summary.required_env_vars.join(", "));
    }
}

/// After a registration: show the manual's hint, or how to add one.
fn print_hint_line(paths: &UtcpPaths, name: &str) {
    match manual_hints(paths).get(name) {
        Some(h) => println!("Hint      : {}", h),
        None => println!(
            "Hint      : none; agents see only '{}' in the skill. Add one: utcp set-hint {} \"<few words>\"",
            name, name
        ),
    }
}

fn build_tokio_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Failed to initialize Tokio runtime")
}

/// Send a server-management command to the daemon. Without a daemon there is nothing to manage:
/// each direct-mode call starts its own server and stops it when the call ends.
fn daemon_server_request(paths: &UtcpPaths, command: IpcCommand) -> Result<IpcPayload> {
    let req = IpcRequest {
        caller: detect_caller(),
        force_log: false,
        command,
    };
    match try_send_ipc_sync(paths, &req)? {
        Some(IpcResponse::Ok { data }) => Ok(data),
        Some(IpcResponse::Error { message }) => Err(anyhow!(message)),
        None => Err(anyhow!(
            "utcp daemon is not running. Without it, each `utcp call` starts its own MCP server \
             and stops it when the call ends, so there is no server to manage."
        )),
    }
}

/// `utcp daemon servers`.
fn print_servers(servers: &[McpServerInfo], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(servers)?);
        return Ok(());
    }
    if servers.is_empty() {
        println!(
            "No local MCP servers. Only manuals imported with `utcp import-mcp --command` run one."
        );
        return Ok(());
    }
    let label = |s: &McpServerInfo| {
        if s.server == s.manual {
            s.manual.clone()
        } else {
            format!("{}/{}", s.manual, s.server)
        }
    };
    let width = servers
        .iter()
        .map(|s| label(s).len())
        .max()
        .unwrap_or(0)
        .max(6);
    println!(
        "{:<width$}  {:<11}  {:>8}  {:>8}  {:>8}  COMMAND",
        "MANUAL", "STATE", "PID", "UPTIME", "RESTARTS"
    );
    for s in servers {
        println!(
            "{:<width$}  {:<11}  {:>8}  {:>8}  {:>8}  {}",
            label(s),
            s.state.as_str(),
            s.pid.map_or("-".to_string(), |p| p.to_string()),
            s.uptime_secs.map_or("-".to_string(), format_uptime),
            s.restarts,
            s.command
        );
    }
    Ok(())
}

/// `45s`, `3m12s`, `2h05m`, `3d04h`.
fn format_uptime(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        s if s < 86400 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        s => format!("{}d{:02}h", s / 86400, s % 86400 / 3600),
    }
}

/// `utcp log recent`: the daemon's in-memory call list (works with logging off), else the log files.
/// `daemon status` line for a stopped daemon that left `daemon.log`: where it is and when it was last
/// written, so a daemon that failed or crashed while detached can be looked into.
fn daemon_log_status_line(paths: &UtcpPaths) -> Option<String> {
    let file = utcp_core::daemon_log::daemon_log_file(paths);
    let modified = std::fs::metadata(&file).ok()?.modified().ok()?;
    Some(format!(
        "Last daemon error or crash: {} (in {})",
        chrono::DateTime::<Utc>::from(modified).to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        file.display()
    ))
}

fn print_recent_activity(paths: &UtcpPaths, limit: usize, failed: bool, json: bool) -> Result<()> {
    let req = IpcRequest {
        caller: detect_caller(),
        force_log: false,
        command: IpcCommand::RecentActivity {
            limit: utcp_core::ipc::RING_BUFFER_CAPACITY,
        },
    };
    let mut entries = match try_send_ipc_sync(paths, &req) {
        Ok(Some(IpcResponse::Ok {
            data: IpcPayload::Activity(items),
        })) => items,
        _ => {
            eprintln!(
                "(daemon not running: showing the log files, which only hold calls made while \
                 logging was on)"
            );
            read_recent_activity(paths, utcp_core::ipc::RING_BUFFER_CAPACITY)
        }
    };
    if failed {
        entries.retain(|e| !e.ok);
    }
    entries.truncate(limit);

    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }
    if entries.is_empty() {
        println!("No recent {}calls.", if failed { "failed " } else { "" });
        return Ok(());
    }
    for e in &entries {
        println!(
            "{}  {:<4} {:>8.1} ms  {:<8} {} {}",
            e.timestamp,
            if e.ok { "ok" } else { "FAIL" },
            e.latency_ms,
            e.caller,
            e.command,
            e.target
        );
        if let Some(detail) = &e.detail {
            for line in detail.lines() {
                println!("    {line}");
            }
        }
    }
    Ok(())
}

fn print_tool_summaries(tools: &[ToolSummary], as_json: bool, with_schemas: bool) -> Result<()> {
    if as_json || with_schemas {
        println!("{}", serde_json::to_string_pretty(tools)?);
        return Ok(());
    }
    if tools.is_empty() {
        println!("No matching tools found.");
        return Ok(());
    }
    for t in tools {
        println!(
            "{:<36} [{:<9}] {}",
            t.name,
            t.protocol,
            first_line(&t.description)
        );
    }
    Ok(())
}

/// The first non-empty line of a description, so text output stays one line per tool.
fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

fn parse_call_args(
    positional: Option<String>,
    from_stdin: bool,
    from_file: Option<PathBuf>,
) -> Result<HashMap<String, Value>> {
    let raw = if from_stdin {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("Failed to read JSON arguments from stdin")?;
        buf
    } else if let Some(path) = from_file {
        std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read args file {}", path.display()))?
    } else if let Some(pos) = positional {
        pos
    } else {
        return Ok(HashMap::new());
    };

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(HashMap::new());
    }
    let val: Value = serde_json::from_str(trimmed)
        .context("Tool arguments must be a valid JSON object (e.g. '{\"key\": \"value\"}')")?;
    let obj = val
        .as_object()
        .ok_or_else(|| anyhow!("Tool arguments JSON root must be an object"))?;
    Ok(obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
}

fn parse_batch_items(
    from_stdin: bool,
    from_file: Option<PathBuf>,
) -> Result<Vec<BatchRequestItem>> {
    let raw = if let Some(path) = from_file {
        std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read batch file {}", path.display()))?
    } else if from_stdin {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("Failed to read batch JSON from stdin")?;
        buf
    } else {
        return Err(anyhow!("Pass --stdin or --file <path> to `utcp batch`"));
    };

    serde_json::from_str(raw.trim()).context("Batch input must be a JSON array of {id, tool, args}")
}

fn parse_header_flags(raw_headers: &[String]) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    for h in raw_headers {
        let (k, v) = h
            .split_once(':')
            .ok_or_else(|| anyhow!("Invalid header '{}': expected 'Key:Value'", h))?;
        map.insert(k.trim().to_string(), v.trim().to_string());
    }
    Ok(map)
}

fn parse_env_flags(raw_envs: &[String]) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    for e in raw_envs {
        let (k, v) = e
            .split_once('=')
            .ok_or_else(|| anyhow!("Invalid --env '{}': expected 'KEY=VALUE'", e))?;
        map.insert(k.trim().to_string(), v.trim().to_string());
    }
    Ok(map)
}

fn print_json_output<T: serde::Serialize>(
    value: &T,
    force_pretty: bool,
    force_compact: bool,
    caller: &str,
) -> Result<()> {
    let use_compact = if force_pretty {
        false
    } else if force_compact {
        true
    } else {
        !std::io::stdout().is_terminal() || matches!(caller, "agy" | "zed" | "hermes" | "vscode")
    };

    if use_compact {
        println!("{}", serde_json::to_string(value)?);
    } else {
        println!("{}", serde_json::to_string_pretty(value)?);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn maybe_log_direct(
    paths: &UtcpPaths,
    caller: &str,
    command: &str,
    target: &str,
    ok: bool,
    status: &str,
    started: Instant,
    force_log: bool,
    detail: Option<String>,
) {
    if !force_log && !is_logging_enabled(paths) {
        return;
    }
    let entry = ActivityEntry {
        timestamp: Utc::now().to_rfc3339(),
        mode: "direct".to_string(),
        caller: caller.to_string(),
        command: command.to_string(),
        target: target.to_string(),
        ok,
        status: status.to_string(),
        latency_ms: started.elapsed().as_secs_f64() * 1000.0,
        detail,
    };
    let _ = append_activity_entry(paths, &entry);
}

#[cfg(test)]
mod tests {
    use super::{first_line, format_uptime};

    #[test]
    fn uptime_is_short_and_readable() {
        assert_eq!(format_uptime(45), "45s");
        assert_eq!(format_uptime(192), "3m12s");
        assert_eq!(format_uptime(2 * 3600 + 5 * 60 + 9), "2h05m");
        assert_eq!(format_uptime(3 * 86400 + 4 * 3600), "3d04h");
    }

    #[test]
    fn text_summaries_use_the_first_non_empty_description_line() {
        assert_eq!(
            first_line("\n  Query docs.  \n\nUse it when:\n- a library\n"),
            "Query docs."
        );
        assert_eq!(first_line("one line"), "one line");
        assert_eq!(first_line(" \n "), "");
    }
}
