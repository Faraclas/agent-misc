use utcp_core::transfer::{stdout_closed, stdout_write_error};

/// Prints to stdout. Once stdout is closed (`utcp list | head`), output is dropped instead of panicking
/// and the command finishes normally (see `utcp_core::transfer::stdout_closed`).
fn write_stdout(args: std::fmt::Arguments) {
    use std::io::Write;
    if stdout_closed() {
        return;
    }
    if let Err(e) = std::io::stdout().lock().write_fmt(args) {
        let err = stdout_write_error(e);
        if !stdout_closed() {
            panic!("failed printing to stdout: {err:#}");
        }
    }
}

// These shadow std's `print!`/`println!` in this crate (defined before the `mod`s so they apply there).
macro_rules! print {
    ($($arg:tt)*) => { $crate::write_stdout(format_args!($($arg)*)) };
}
macro_rules! println {
    () => { $crate::write_stdout(format_args!("\n")) };
    ($($arg:tt)*) => { $crate::write_stdout(format_args!("{}\n", format_args!($($arg)*))) };
}

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
    activity_detail, append_activity_entry, batch_activity_summary, call_verb, clear_activity_logs,
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
    try_send_ipc_sync, try_send_ipc_with_files, IpcCommand, IpcPayload, IpcRequest, IpcResponse,
    McpServerInfo,
};
use utcp_core::ipc_files::ClientFiles;
use utcp_core::media::{clear_media_spool, inspect_media_spool};
use utcp_core::paths::UtcpPaths;
use utcp_core::skills::{
    inspect_skill_targets, manual_label, refresh_installed_global_skills, registered_manual_labels,
    sync_ide_skills,
};
use utcp_core::staging::{
    diff_staged, discard_staged, list_staged, promote_staged, rollback_manual, stage_manual,
    ManualDiff, Replaces,
};
use utcp_core::transfer::{DownloadFolderSource, OutTarget};

#[derive(Parser, Debug)]
#[command(
    name = "utcp",
    version,
    about = "Fast, Zero-MCP Universal Tool Calling Protocol (UTCP) CLI, Daemon & TUI"
)]
struct Cli {
    /// Explicit path to `.utcp_config.json` (overrides shared user catalog; a separate setup with its own
    /// cache folder and daemon unless UTCP_CACHE_DIR / UTCP_SOCKET_PATH say otherwise)
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
    /// Fast summary of all registered tools, or of one manual's tools (`utcp list <manual>`)
    List {
        /// Only this manual's tools
        manual: Option<String>,
        /// Output raw JSON array
        #[arg(long)]
        json: bool,
        /// List the tools of the staged manuals (`utcp stage`) instead of the active ones
        #[arg(long)]
        staged: bool,
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
        /// A tool of a staged manual (`utcp stage`)
        #[arg(long)]
        staged: bool,
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
        /// Call a tool of a staged manual (`utcp stage`), to test it before `utcp promote`
        #[arg(long)]
        staged: bool,
        /// Save the answer to this file instead of printing it (an existing file is overwritten, like
        /// `curl -o`; a folder keeps the server's file name). `-` writes a file answer's bytes to stdout.
        /// Without it, a file answer goes to your Downloads folder and its path is printed.
        #[arg(long, short = 'o', value_name = "PATH")]
        out: Option<String>,
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
        /// Every item calls a staged manual (`utcp stage`)
        #[arg(long)]
        staged: bool,
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
    /// Add an OpenAPI spec or UTCP manual to the staging area without making it active: review it with
    /// `utcp diff`, test it with `utcp call --staged`, then `utcp promote` it (or `utcp discard` it).
    /// Replaces a staged copy of the same name
    Stage {
        /// Manual namespace name
        name: String,
        /// URL or local file path
        source: String,
        /// Base URL override
        #[arg(long)]
        base_url: Option<String>,
        /// Extra static HTTP headers (`Key:Value`)
        #[arg(short = 'H', long = "header")]
        headers: Vec<String>,
        /// A few words on what the manual is for, shown in the agent skill once promoted (kept when omitted)
        #[arg(long)]
        hint: Option<String>,
    },
    /// Compare staged manuals with the active ones they would replace: tools added, removed and changed
    Diff {
        /// Staged manual name (all staged manuals when omitted)
        name: Option<String>,
        /// Output raw JSON
        #[arg(long)]
        json: bool,
    },
    /// Make staged manuals active (rebuilds search, refreshes the skills, reloads the daemon). The version
    /// each replaces is kept for `utcp rollback`
    Promote {
        /// Staged manual name (all staged manuals when omitted)
        name: Option<String>,
    },
    /// Drop staged manuals without making them active
    Discard {
        /// Staged manual name (all staged manuals when omitted)
        name: Option<String>,
    },
    /// Make the version a `utcp promote` replaced active again. Run it twice to undo it
    Rollback {
        /// Manual namespace name
        name: String,
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
        /// Delete it for good: remove the registry entry and the saved `manuals/<name>.utcp.json`,
        /// and any staged copy or previous version (also works on an already-deregistered manual
        /// or an orphaned saved file)
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
    /// Check daemon status, config paths, index freshness, staged manuals, credentials (and where each
    /// comes from), the `[secrets]` command, the `.env` file, `settings.toml`, media spool, and IDE skills
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
        // The reader stopped reading (`utcp call ... --out - | head -c 100`): what failed after that
        // is a consequence, so end as quietly as when only printing stopped.
        if stdout_closed() {
            return;
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
    // The daemon started by systemd has no one to tell; the next command a person runs adds the settings.
    if !matches!(
        cli.command,
        Commands::Daemon {
            action: DaemonAction::Run { .. }
        }
    ) {
        sync_settings(&paths);
    }

    match cli.command {
        Commands::List {
            manual,
            json,
            staged,
        } => {
            let started = Instant::now();
            let caller = detect_caller();

            let tools = 'fetch: {
                if !cli.direct {
                    let req = IpcRequest {
                        caller: caller.clone(),
                        force_log: false,
                        command: IpcCommand::List {
                            manual: manual.clone(),
                            staged,
                        },
                    };
                    if let Some(resp) = try_send_ipc_sync(&paths, &req)? {
                        match resp {
                            IpcResponse::Ok {
                                data: IpcPayload::Tools(tools),
                            } => break 'fetch tools,
                            IpcResponse::Error { message } => return Err(anyhow!(message)),
                            _ => return Err(anyhow!("Unexpected IPC response payload")),
                        }
                    }
                }
                let tools = list_tools_sync(&paths, manual.as_deref(), staged)?;
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
                tools
            };
            if let (true, Some(manual)) = (tools.is_empty(), manual.as_deref()) {
                let registry = UtcpRegistryConfig::load(&paths)?;
                return Err(anyhow!(empty_list_message(&registry, manual, staged)));
            }
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

        Commands::Schema { target, staged } => {
            let started = Instant::now();
            let caller = detect_caller();

            if !cli.direct {
                let req = IpcRequest {
                    caller: caller.clone(),
                    force_log: false,
                    command: IpcCommand::Schema {
                        target: target.clone(),
                        staged,
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
                        IpcResponse::Error { message } => Err(with_staged_hint(
                            anyhow!(message),
                            &paths,
                            &target,
                            staged,
                            "schema",
                        )),
                        _ => Err(anyhow!("Unexpected IPC response payload")),
                    };
                }
            }

            let schema = get_schema_sync(&paths, &target, staged)
                .map_err(|e| with_staged_hint(e, &paths, &target, staged, "schema"))?;
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
            staged,
            out,
        } => {
            if let Some(word) = json_args.as_deref() {
                if !target.contains('.') && looks_like_tool_name(word) {
                    let registry = UtcpRegistryConfig::load(&paths)?;
                    return Err(anyhow!(split_target_message(
                        &paths, &registry, &target, word, staged
                    )));
                }
            }
            let args = parse_call_args(json_args, args_stdin, args_file)?;
            let out = out.as_deref().map(OutTarget::parse);
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
                        staged,
                    },
                };
                let mut files = ClientFiles::new(&paths);
                files.call_out = out.clone();
                files.tool = target.clone();
                if let Some(resp) = try_send_ipc_with_files(&paths, &req, &mut files)? {
                    return match resp {
                        IpcResponse::Ok {
                            data: IpcPayload::CallResult(val),
                        } => print_call_output(&val, out.as_ref(), pretty, compact, &caller),
                        IpcResponse::Error { message } => Err(with_staged_hint(
                            anyhow!(message),
                            &paths,
                            &target,
                            staged,
                            "call",
                        )),
                        _ => Err(anyhow!("Unexpected IPC response payload")),
                    };
                }
            }

            let rt = build_tokio_runtime()?;
            let outcome = block_on_interruptible(
                &rt,
                call_tool_direct(
                    &paths,
                    &target,
                    args,
                    dry_run,
                    fields_opt.as_deref(),
                    staged,
                    out.as_ref(),
                ),
            )?;
            match outcome {
                Ok(val) => {
                    maybe_log_direct(
                        &paths,
                        &caller,
                        call_verb(dry_run, staged),
                        &target,
                        true,
                        "OK",
                        started,
                        log,
                        None,
                    );
                    print_call_output(&val, out.as_ref(), pretty, compact, &caller)
                }
                Err(err) => {
                    let detail = activity_detail(&format!("{:#}", err), &load_env_map(&paths));
                    maybe_log_direct(
                        &paths,
                        &caller,
                        call_verb(dry_run, staged),
                        &target,
                        false,
                        "ERR",
                        started,
                        log,
                        Some(detail),
                    );
                    Err(with_staged_hint(err, &paths, &target, staged, "call"))
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
            staged,
        } => {
            let mut items = parse_batch_items(stdin, file)?;
            if !fields.is_empty() {
                for item in &mut items {
                    if item.fields.is_none() {
                        item.fields = Some(fields.clone());
                    }
                }
            }
            let mut item_outs = HashMap::new();
            for (i, item) in items.iter().enumerate() {
                match item.out.as_deref().map(OutTarget::parse) {
                    Some(OutTarget::Stdout) => {
                        return Err(anyhow!(
                            "batch item '{}': \"out\": \"-\" (stdout) works only with `utcp call --out -`; \
                             give a file path",
                            item.id
                        ))
                    }
                    Some(target) => {
                        item_outs.insert(i, target);
                    }
                    None => {}
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
                        staged,
                    },
                };
                let mut files = ClientFiles::new(&paths);
                files.item_outs = item_outs;
                if let Some(resp) = try_send_ipc_with_files(&paths, &req, &mut files)? {
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
            let results = block_on_interruptible(
                &rt,
                batch_call_direct(&paths, items, concurrency, staged),
            )??;
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

        Commands::Stage {
            name,
            source,
            base_url,
            headers,
            hint,
        } => {
            let parsed_headers = parse_header_flags(&headers)?;
            let rt = build_tokio_runtime()?;
            let summary = block_on_interruptible(
                &rt,
                stage_manual(
                    &paths,
                    &name,
                    &source,
                    base_url.as_deref(),
                    parsed_headers,
                    hint.as_deref(),
                ),
            )??;
            // The daemon loads staged manuals too, so `call --staged` gets the same secrets.
            notify_daemon_reload(&paths);
            let staged = list_staged(&paths)?;
            let info = staged.iter().find(|s| s.name == name);
            println!(
                "Staged '{}' ({} tools, protocols: {}) -> {}",
                summary.manual_name,
                summary.tool_count,
                summary.protocols.join(","),
                summary.output_path
            );
            if let Some(info) = info {
                println!("Replaces  : {}", replaces_text(info.replaces));
                match &info.hint {
                    Some(h) => println!("Hint      : {}", h),
                    None => println!("Hint      : none (add one with --hint)"),
                }
            }
            if !summary.required_env_vars.is_empty() {
                println!(
                    "Secrets   : {} (check with: utcp doctor)",
                    summary.required_env_vars.join(", ")
                );
            }
            println!(
                "Next      : utcp diff {n}   (it shows how to see the tools, test, and promote or discard)",
                n = name
            );
            Ok(())
        }

        Commands::Diff { name, json } => {
            let diffs = diff_staged(&paths, name.as_deref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&diffs)?);
                return Ok(());
            }
            if diffs.is_empty() {
                println!(
                    "Nothing is staged. Stage a manual with: utcp stage <name> <url_or_file_path>"
                );
                return Ok(());
            }
            for d in &diffs {
                print_manual_diff(d);
            }
            println!(
                "\nNext: test any of the staged manuals, then either promote or discard them."
            );
            println!("- See the tools: utcp list <manual> --staged");
            println!("- Test:          utcp call --staged <manual>.<tool> '<json args>'");
            println!("- Promote:       utcp promote");
            println!("- Discard:       utcp discard");
            Ok(())
        }

        Commands::Promote { name } => {
            let promoted = promote_staged(&paths, name.as_deref())?;
            if promoted.is_empty() {
                println!("Nothing is staged.");
                return Ok(());
            }
            notify_daemon_reload(&paths);
            for p in &promoted {
                let replaced = match p.replaced {
                    Replaces::Nothing => "new".to_string(),
                    Replaces::Active => "replaced the active one".to_string(),
                    Replaces::Deregistered => "replaced the deregistered one".to_string(),
                };
                if p.rollback_available {
                    println!(
                        "Promoted '{}' ({} tools, {}; undo with: utcp rollback {})",
                        p.name, p.tool_count, replaced, p.name
                    );
                } else {
                    println!(
                        "Promoted '{}' ({} tools, {})",
                        p.name, p.tool_count, replaced
                    );
                }
            }
            println!("Search, the daemon and the installed agent skills now list them.");
            Ok(())
        }

        Commands::Discard { name } => {
            let dropped = discard_staged(&paths, name.as_deref())?;
            if dropped.is_empty() {
                println!("Nothing is staged.");
                return Ok(());
            }
            notify_daemon_reload(&paths);
            println!("Discarded the staged copy of: {}", dropped.join(", "));
            Ok(())
        }

        Commands::Rollback { name } => {
            let r = rollback_manual(&paths, &name)?;
            notify_daemon_reload(&paths);
            if r.undo_available {
                println!(
                    "Rolled back '{}' to its previous version ({} tools). Run `utcp rollback {}` again to undo.",
                    r.name, r.tool_count, r.name
                );
            } else {
                println!(
                    "Rolled back '{}' to its previous version ({} tools).",
                    r.name, r.tool_count
                );
            }
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
                "Staged Manuals    : {}",
                match report.staged_count {
                    0 => "none".to_string(),
                    n => format!("{n} (review: utcp diff; activate: utcp promote)"),
                }
            );
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
            println!("Settings File     : {}", report.settings_file);
            for unknown in &report.unknown_settings {
                match &unknown.hint {
                    Some(hint) => println!(
                        "Unknown Setting   : `{}` has no effect ({hint})",
                        unknown.key
                    ),
                    None => println!(
                        "Unknown Setting   : `{}` has no effect (utcp does not know it; check the spelling)",
                        unknown.key
                    ),
                }
            }
            println!(
                "Upload Limit      : {}",
                describe_limit(report.max_upload_mb, "max_upload_mb")
            );
            println!(
                "Download Limit    : {}",
                describe_limit(report.max_download_mb, "max_download_mb")
            );
            println!(
                "Downloads Folder  : {} ({})",
                report.download_dir,
                match report.download_dir_source {
                    Some(DownloadFolderSource::Setting) => "download_dir",
                    Some(DownloadFolderSource::UserDownloads) => "your Downloads folder",
                    _ => "utcp's own: no Downloads folder found; files are deleted after a day",
                }
            );
            match &report.secrets_command {
                Some(s) => match &s.error {
                    Some(error) => println!(
                        "Secrets Command   : FAILED: {error}\n                    `{}`",
                        s.command
                    ),
                    None => println!(
                        "Secrets Command   : OK, printed {} of the wanted names\n                    `{}`",
                        s.provided.len(),
                        s.command
                    ),
                },
                None => println!(
                    "Secrets Command   : not set (optional, see \"The Secrets Command\" in operating.md)"
                ),
            }
            println!(
                "Secrets File      : {} ({})",
                report.env_file,
                if report.env_file_exists {
                    "exists"
                } else {
                    "not created"
                }
            );

            if !report.credentials.is_empty() {
                if d_info.is_some() {
                    println!("\nCredentials (as the running daemon sees them):");
                } else {
                    println!("\nCredentials (secrets command, environment, secrets file):");
                }
                for line in credential_lines(&report.credentials, color_enabled()) {
                    println!("{line}");
                }
            }

            println!("\nAgent Skills (utcp-tools + utcp-admin in each skills folder):");
            for s in skills {
                println!(
                    "  {} {} -> {}",
                    skill_tag(&s, color_enabled()),
                    s.client,
                    s.path
                );
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
                    println!(
                        "{} {} -> {}",
                        skill_tag(&s, color_enabled()),
                        s.client,
                        s.path
                    );
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

/// Create `settings.toml`, or add the settings a newer utcp knows, and say so once on stderr.
fn sync_settings(paths: &UtcpPaths) {
    use utcp_core::settings_file::{backup_path, sync_settings_file, SyncOutcome};
    let file = paths.settings_file.display();
    match sync_settings_file(paths) {
        Ok(SyncOutcome::Created) => eprintln!(
            "utcp: created {file} with the default settings (see \"Settings\" in operating.md)."
        ),
        Ok(SyncOutcome::Added(lines)) => {
            eprintln!(
                "utcp: added the settings this version knows to {file}; the old file is {}:",
                backup_path(&paths.settings_file).display()
            );
            for line in lines {
                eprintln!("  {line}");
            }
            eprintln!("A line starting with # is off. See \"Settings\" in operating.md.");
        }
        Ok(_) => {}
        Err(e) => eprintln!(
            "utcp: warning: {e:#}. utcp uses its built-in settings until the file is fixed."
        ),
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

/// `utcp doctor`: one line per credential, listing every manual that uses it. Manuals that read it under
/// another name (`via`) or get it from another place get their own line.
fn credential_lines(
    credentials: &[utcp_core::catalog::CredentialStatus],
    color: bool,
) -> Vec<String> {
    type Key = (String, bool, Option<String>, Option<String>);
    let mut groups: Vec<(Key, Vec<String>)> = Vec::new();
    for c in credentials {
        let via = c.resolved_via.clone().filter(|v| *v != c.env_var);
        let key = (c.env_var.clone(), c.is_set, via, c.source.clone());
        let manual = if c.staged {
            format!("{} (staged)", c.manual)
        } else {
            c.manual.clone()
        };
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, manuals)) => manuals.push(manual),
            None => groups.push((key, vec![manual])),
        }
    }
    groups
        .into_iter()
        .map(|((var, is_set, via, source), manuals)| {
            let manuals = format!(
                "{}: {}",
                if manuals.len() == 1 {
                    "manual"
                } else {
                    "manuals"
                },
                manuals.join(", ")
            );
            if is_set {
                let mut parts = Vec::new();
                if let Some(via) = via {
                    parts.push(format!("via {via}"));
                }
                if let Some(source) = source {
                    parts.push(format!("from {source}"));
                }
                parts.push(manuals);
                format!(
                    "  [{}]     {:<24} ({})",
                    paint("SET", GREEN, color),
                    var,
                    parts.join("; ")
                )
            } else {
                format!("  [MISSING] {:<24} ({})", var, manuals)
            }
        })
        .collect()
}

fn replaces_text(r: Replaces) -> &'static str {
    match r {
        Replaces::Nothing => "nothing (a new manual)",
        Replaces::Active => "the active manual of that name",
        Replaces::Deregistered => "the deregistered manual of that name",
    }
}

/// Why `utcp list <manual>` found no tools, and what to run instead.
fn empty_list_message(registry: &UtcpRegistryConfig, manual: &str, staged: bool) -> String {
    let active = registry.manuals.contains_key(manual);
    let is_staged = registry.staged.contains_key(manual);
    match (staged, active, is_staged) {
        (false, false, true) => format!(
            "'{manual}' is staged, not active yet. List its tools with: utcp list {manual} --staged"
        ),
        (true, true, false) => {
            format!("'{manual}' is active, not staged. List its tools with: utcp list {manual}")
        }
        (false, true, _) | (true, _, true) => format!("Manual '{manual}' has no tools"),
        _ => format!(
            "No manual named '{manual}' is registered or staged. See every tool with: utcp list \
             (staged ones: utcp list --staged)"
        ),
    }
}

/// Where `manual.tool` can be called: which side of `--staged` holds the manual, whether that
/// manual has the tool, and, when it does not, its tools whose names contain `tool`.
struct ToolLookup {
    staged: bool,
    found: bool,
    similar: Vec<String>,
}

/// `None` when no manual of that name is registered or staged.
fn lookup_tool(
    paths: &UtcpPaths,
    registry: &UtcpRegistryConfig,
    manual: &str,
    tool: &str,
    staged: bool,
) -> Option<ToolLookup> {
    let active = registry.manuals.contains_key(manual);
    let is_staged = registry.staged.contains_key(manual);
    let side = match (staged, active, is_staged) {
        (true, _, true) | (false, false, true) => true,
        (false, true, _) | (true, true, false) => false,
        _ => return None,
    };
    let tools = list_tools_sync(paths, Some(manual), side).unwrap_or_default();
    let full = format!("{manual}.{tool}");
    let needle = tool.to_ascii_lowercase();
    let similar = tools
        .iter()
        .filter(|t| {
            let short = t
                .name
                .strip_prefix(&format!("{manual}."))
                .unwrap_or(&t.name);
            short.to_ascii_lowercase().contains(&needle)
        })
        .map(|t| t.name.clone())
        .take(5)
        .collect();
    Some(ToolLookup {
        staged: side,
        found: tools.iter().any(|t| t.name == full),
        similar,
    })
}

fn staged_flag(staged: bool) -> &'static str {
    if staged {
        " --staged"
    } else {
        ""
    }
}

/// `'x' has no tool named 'y'.`, the tools with `y` in their name, and how to list them all.
fn no_such_tool(l: &ToolLookup, manual: &str, tool: &str) -> String {
    let flag = staged_flag(l.staged);
    let mut s = format!(
        "'{manual}'{} has no tool named '{tool}'.",
        if l.staged { " (staged)" } else { "" }
    );
    if !l.similar.is_empty() {
        s.push_str(&format!(
            " Tools with '{tool}' in the name: {}.",
            l.similar.join(", ")
        ));
    }
    s.push_str(&format!(
        " See all of its tools with: utcp list {manual}{flag}"
    ));
    s
}

/// `utcp call azdo_core get_all_teams`: the tool name landed where the JSON arguments go.
/// Suggests the corrected command only when that tool exists (with `--staged` when it is staged).
fn split_target_message(
    paths: &UtcpPaths,
    registry: &UtcpRegistryConfig,
    target: &str,
    word: &str,
    staged: bool,
) -> String {
    let joined = "The manual and the tool are joined by a dot: utcp call <manual>.<tool>.";
    match lookup_tool(paths, registry, target, word, staged) {
        None => format!(
            "{joined} No manual named '{target}' is registered or staged. See every tool with: \
             utcp list (staged ones: utcp list --staged)"
        ),
        Some(l) if l.found => {
            let flag = staged_flag(l.staged);
            format!(
                "{joined}\nDid you mean: utcp call{flag} {target}.{word}\n\
                 (JSON arguments, if the tool needs any, come after it: utcp call{flag} \
                 {target}.{word} '{{\"key\": \"value\"}}')"
            )
        }
        Some(l) => format!("{joined}\n{}", no_such_tool(&l, target, word)),
    }
}

/// True for `get_all_teams`: a bare name that is not JSON arguments.
fn looks_like_tool_name(word: &str) -> bool {
    !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && serde_json::from_str::<Value>(word).is_err()
}

/// A call or schema on `manual.tool` failed. When the tool exists only on the other side of
/// `--staged`, give the command that works; when the manual has no such tool, name similar ones.
/// `None` when the tool is where the command looked (it failed for another reason).
fn target_hint(
    paths: &UtcpPaths,
    registry: &UtcpRegistryConfig,
    target: &str,
    staged: bool,
    verb: &str,
) -> Option<String> {
    let (manual, tool) = target.split_once('.')?;
    let l = lookup_tool(paths, registry, manual, tool, staged)?;
    match (l.found, l.staged == staged) {
        (true, true) => None,
        (true, false) if l.staged => Some(format!(
            "'{manual}' is staged, not active yet. Add --staged: utcp {verb} --staged {target}"
        )),
        (true, false) => Some(format!(
            "'{manual}' is active, not staged. Leave out --staged: utcp {verb} {target}"
        )),
        (false, _) => Some(no_such_tool(&l, manual, tool)),
    }
}

/// Append [`target_hint`] to a failed call or schema. Reads the registry only on this error path.
fn with_staged_hint(
    err: anyhow::Error,
    paths: &UtcpPaths,
    target: &str,
    staged: bool,
    verb: &str,
) -> anyhow::Error {
    match UtcpRegistryConfig::load(paths)
        .ok()
        .and_then(|r| target_hint(paths, &r, target, staged, verb))
    {
        Some(hint) => anyhow!("{:#}\n{}", err, hint),
        None => err,
    }
}

/// `utcp diff`: one staged manual against the manual it replaces. The name is blue in a terminal.
fn print_manual_diff(d: &ManualDiff) {
    let name = paint(&d.name, BLUE, color_enabled());
    match d.replaces {
        Replaces::Nothing => println!("{}: new manual, {} tools", name, d.tool_count),
        r => {
            let what = if r == Replaces::Active {
                "active"
            } else {
                "deregistered"
            };
            println!(
                "{}: replaces the {} manual, {} tools ({} added, {} removed, {} changed, {} unchanged)",
                name,
                what,
                d.tool_count,
                d.added.len(),
                d.removed.len(),
                d.changed.len(),
                d.unchanged
            );
            for t in &d.added {
                println!("  + {}", t);
            }
            for t in &d.removed {
                println!("  - {}", t);
            }
            for t in &d.changed {
                println!("  ~ {}", t);
            }
        }
    }
    if d.replaces == Replaces::Nothing {
        if let Some(h) = &d.hint_after {
            println!("  hint: {}", h);
        }
    } else if d.hint_before != d.hint_after {
        println!(
            "  hint: {} -> {}",
            d.hint_before.as_deref().unwrap_or("none"),
            d.hint_after.as_deref().unwrap_or("none")
        );
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
    let color = color_enabled();
    for t in tools {
        println!("{}", tool_line(t, color));
    }
    Ok(())
}

/// One `list`/`search` row: a bullet, so a long description that wraps is not mistaken for a new
/// tool, then the tool name (blue in a terminal), protocol and the description's first line.
fn tool_line(t: &ToolSummary, color: bool) -> String {
    let pad = 36usize.saturating_sub(t.name.chars().count());
    format!(
        "* {}{:pad$} [{:<9}] {}",
        paint(&t.name, BLUE, color),
        "",
        t.protocol,
        first_line(&t.description)
    )
}

const BLUE: &str = "34";
const GREEN: &str = "32";

/// Whether to color text output: stdout is a terminal, `NO_COLOR` is unset and `TERM` is not `dumb`.
/// Pipes, agents and `--json` get plain text.
fn color_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::io::stdout().is_terminal()
            && !matches!(std::env::var_os("NO_COLOR"), Some(v) if !v.is_empty())
            && std::env::var("TERM").as_deref() != Ok("dumb")
            && ansi_supported()
    })
}

/// Windows consoles show ANSI colors only once virtual terminal processing is on; this turns it on.
#[cfg(windows)]
fn ansi_supported() -> bool {
    crossterm::ansi_support::supports_ansi()
}

#[cfg(not(windows))]
fn ansi_supported() -> bool {
    true
}

fn paint(text: &str, code: &str, color: bool) -> String {
    if color {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// A skills folder's state for `doctor` and `sync-ides --status`: `[UP TO DATE   ]` (green), etc.
fn skill_tag(s: &utcp_core::skills::IdeSkillTargetStatus, color: bool) -> String {
    let tag = if s.up_to_date {
        "UP TO DATE"
    } else if s.exists {
        "OUTDATED"
    } else {
        "NOT INSTALLED"
    };
    let shown = if s.up_to_date {
        paint(tag, GREEN, color)
    } else {
        tag.to_string()
    };
    format!("[{shown}{:pad$}]", "", pad = 13 - tag.len())
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

/// A size limit for `doctor`: where it comes from and what it does.
fn describe_limit(mb: Option<u64>, key: &str) -> String {
    match mb {
        None => format!("no limit ({key} is not set)"),
        Some(0) => format!("OFF, every file is refused ({key} = 0)"),
        Some(mb) => format!("{mb} MB ({key})"),
    }
}

/// A call's answer: nothing more when `--out -` already wrote a file's bytes to stdout.
fn print_call_output(
    val: &Value,
    out: Option<&OutTarget>,
    pretty: bool,
    compact: bool,
    caller: &str,
) -> Result<()> {
    let streamed_to_stdout = out == Some(&OutTarget::Stdout)
        && val.get("type").and_then(Value::as_str) == Some("file")
        && val.get("path").and_then(Value::as_str) == Some("-");
    if streamed_to_stdout {
        return Ok(());
    }
    print_json_output(val, pretty, compact, caller)
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
    use super::{
        credential_lines, first_line, format_uptime, paint, skill_tag, tool_line, BLUE, GREEN,
    };
    use utcp_core::catalog::CredentialStatus;
    use utcp_core::index::ToolSummary;
    use utcp_core::skills::IdeSkillTargetStatus;

    #[test]
    fn doctor_lists_each_credential_once_with_its_manuals() {
        let cred = |manual: &str, var: &str, via: Option<&str>, staged: bool| CredentialStatus {
            manual: manual.into(),
            env_var: var.into(),
            resolved_via: via.map(Into::into),
            is_set: via.is_some(),
            source: via.map(|_| "secrets command".into()),
            staged,
        };
        let creds = [
            cred("a_core", "PAT", Some("PAT"), false),
            cred("a_git", "PAT", Some("PAT"), true),
            cred("gh", "TOKEN", Some("GH_TOKEN"), false),
            cred("x", "KEY", None, false),
        ];
        let lines = credential_lines(&creds, false);
        assert_eq!(
            lines,
            [
                format!(
                    "  [SET]     {:<24} (from secrets command; manuals: a_core, a_git (staged))",
                    "PAT"
                ),
                format!(
                    "  [SET]     {:<24} (via GH_TOKEN; from secrets command; manual: gh)",
                    "TOKEN"
                ),
                format!("  [MISSING] {:<24} (manual: x)", "KEY"),
            ]
        );
        let colored = credential_lines(&creds, true);
        assert!(colored[0].starts_with("  [\x1b[32mSET\x1b[0m]     PAT "));
        assert_eq!(colored[2], lines[2], "only SET is colored");
    }

    #[test]
    fn tool_rows_have_a_bullet_and_a_blue_name_that_keeps_the_columns() {
        let tool = ToolSummary {
            name: "pets.get_user".into(),
            manual: "pets".into(),
            protocol: "http".into(),
            description: "Fetch a user.\nMore text.".into(),
            tags: Vec::new(),
            inputs: None,
            outputs: None,
        };
        let plain = tool_line(&tool, false);
        assert_eq!(
            plain,
            format!("* {:<36} [http     ] Fetch a user.", "pets.get_user")
        );
        let colored = tool_line(&tool, true);
        assert_eq!(
            colored,
            plain.replace("pets.get_user", &paint("pets.get_user", BLUE, true))
        );
        assert_eq!(paint("x", BLUE, true), "\x1b[34mx\x1b[0m");
    }

    #[test]
    fn skill_tags_keep_their_width_and_only_up_to_date_is_green() {
        let status = |exists, up_to_date| IdeSkillTargetStatus {
            client: "Zed".into(),
            path: "/p".into(),
            exists,
            up_to_date,
        };
        assert_eq!(skill_tag(&status(true, true), false), "[UP TO DATE   ]");
        assert_eq!(skill_tag(&status(false, false), false), "[NOT INSTALLED]");
        assert_eq!(
            skill_tag(&status(true, true), true),
            format!("[{}   ]", paint("UP TO DATE", GREEN, true))
        );
        assert_eq!(skill_tag(&status(true, false), true), "[OUTDATED     ]");
    }

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
