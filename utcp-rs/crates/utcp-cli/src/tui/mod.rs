use crate::daemon::{start_daemon_background, stop_daemon};
use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
        MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Table, TableState, Tabs, Wrap,
    },
    Frame, Terminal,
};
use std::cell::Cell as StdCell;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use utcp_core::activity::{
    activity_detail, clear_activity_logs, is_logging_enabled, read_recent_activity,
    set_logging_enabled, ActivityEntry,
};
use utcp_core::catalog::{
    call_tool_direct, deregister_manual, import_manual_to_file, import_mcp_manual,
    infer_mcp_manual_name, infer_name_from_source, list_unregistered_manuals, load_env_map,
    load_or_build_index_sync, normalize_manual_hint, query_systemd_user_unit_state,
    refresh_manuals, register_manual, reregister_manual, resolve_secret_var, set_manual_hint,
    DeregisterOutcome, ImportSummary, UtcpRegistryConfig, MAX_MANUAL_HINT_CHARS,
};
use utcp_core::index::{IndexedManual, SearchIndex, ToolSchemaDetail, ToolSummary};
use utcp_core::ipc::{
    manual_caller_env, notify_daemon_reload, query_daemon_status, sync_caller_env_to_daemon,
    try_send_ipc_sync, DaemonStatusInfo, IpcCommand, IpcPayload, IpcRequest, IpcResponse,
};
use utcp_core::media::{clear_media_spool, inspect_media_spool, MediaSpoolStatus};
use utcp_core::paths::{UtcpPaths, UtcpSettings};
use utcp_core::skills::{
    inspect_skill_targets, manual_label, refresh_installed_global_skills, sync_ide_skills,
    IdeSkillTargetStatus, ADMIN_SKILL, TOOLS_SKILL,
};

const TAB_TITLES: [&str; 6] = [
    "1: Manuals",
    "2: Search",
    "3: Import",
    "4: Settings & Skills",
    "5: Live Activity",
    "6: Daemon Monitor",
];

/// Which Tab 2 pane receives ↑/↓ (and wheel input when the pointer is outside both panes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchPane {
    Matches,
    Schema,
}

/// What the Tab 2 right pane shows. Toggled with `d`; kept in memory only, so every `utcp ui`
/// launch starts on `Schema` while the choice survives tab switches and tool changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SchemaView {
    Schema,
    Docs,
}

/// Rows the mouse wheel moves the Schema & Docs pane per notch.
const WHEEL_SCROLL_ROWS: u16 = 3;

/// Most output lines the Try popup shows; longer results end with a note pointing to the CLI.
const TRY_OUTPUT_MAX_LINES: usize = 5000;

/// Tab 2 `t`: the Try popup for one tool. While open it receives every key.
#[derive(Debug, Clone)]
struct TryPopup {
    /// Qualified tool name (`manual.tool`).
    tool: String,
    /// The JSON arguments as typed.
    args: String,
    /// Caret position in `args`, in chars.
    cursor: usize,
    /// Keys go into the arguments box (TYPING badge) instead of running commands.
    editing: bool,
    output: TryOutput,
    scroll: u16,
}

/// Tab 1 `h`: the hint editor for one manual. Always typing; Enter saves, Esc cancels.
#[derive(Debug, Clone)]
struct HintPopup {
    manual: String,
    text: String,
    /// Caret position in `text`, in chars.
    cursor: usize,
    /// Why the last Enter did not save (shown in the popup).
    error: Option<String>,
}

/// What the lower half of the Try popup shows.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TryOutput {
    /// Nothing ran yet: explain `d` and Enter.
    Idle,
    /// Queued; runs right after the next draw so this message is on screen first.
    Running {
        dry_run: bool,
    },
    DryRun(String),
    Result(String),
    Error(String),
}

/// Tab 2 result cap. High enough that a `<manual>.` filter lists every tool of a large manual;
/// search is in-memory and takes microseconds, so this costs nothing noticeable.
const TUI_SEARCH_LIMIT: usize = 500;

/// Tab 3: what the import form reads. `m` switches; each mode keeps its own form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImportMode {
    /// An OpenAPI 2.0/3.x spec (JSON or YAML) or a UTCP manual, from a URL or a file.
    Spec,
    /// A Model Context Protocol server, reached by URL or started as a local command.
    Mcp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImportField {
    Source,
    Name,
    /// A few words for the skill description (used by `r` only).
    Hint,
    /// `;`-separated `Header: value` (URLs) or `KEY=value` (MCP commands).
    Auth,
    Output,
}

const IMPORT_FIELDS: [ImportField; 5] = [
    ImportField::Source,
    ImportField::Name,
    ImportField::Hint,
    ImportField::Auth,
    ImportField::Output,
];

/// One Tab 3 form (one per mode, so switching modes loses nothing).
#[derive(Debug, Clone, Default)]
struct ImportForm {
    source: String,
    name: String,
    hint: String,
    auth: String,
    output: String,
}

impl ImportForm {
    fn field(&self, field: ImportField) -> &str {
        match field {
            ImportField::Source => &self.source,
            ImportField::Name => &self.name,
            ImportField::Hint => &self.hint,
            ImportField::Auth => &self.auth,
            ImportField::Output => &self.output,
        }
    }

    fn field_mut(&mut self, field: ImportField) -> &mut String {
        match field {
            ImportField::Source => &mut self.source,
            ImportField::Name => &mut self.name,
            ImportField::Hint => &mut self.hint,
            ImportField::Auth => &mut self.auth,
            ImportField::Output => &mut self.output,
        }
    }

    /// The Hint field for `register_manual` / `import_mcp_manual` (`None` when empty).
    fn hint_arg(&self) -> Option<&str> {
        Some(self.hint.trim()).filter(|h| !h.is_empty())
    }

    /// The `;`-separated entries of the Env / Headers field.
    fn auth_items(&self) -> Vec<String> {
        self.auth
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }
}

/// What the MCP form asks `import_mcp_manual` to do, parsed from the fields.
#[derive(Debug, Clone, PartialEq, Eq)]
struct McpImportPlan {
    name: String,
    url: Option<String>,
    command: Option<String>,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    headers: BTreeMap<String, String>,
    /// Some Env / Headers value has no `${VAR}`, so it is saved in the manual as typed.
    literal_values: bool,
}

fn is_http_url(s: &str) -> bool {
    s.starts_with("https://") || s.starts_with("http://")
}

/// Runs one async catalog call on a throwaway current-thread runtime (the TUI loop is sync).
fn block_on<F: std::future::Future>(fut: F) -> Result<F::Output, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map(|rt| rt.block_on(fut))
        .map_err(|e| format!("could not start the async runtime: {}", e))
}

/// Byte offset of char `char_idx` in `s` (or `s.len()` past the end), for caret edits.
fn char_byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map_or(s.len(), |(b, _)| b)
}

/// One editing key in a one-line text box with a caret (`cursor`, in chars): ←/→, Home/End,
/// Backspace/Delete and typed characters. Other keys do nothing.
fn edit_text_at_caret(text: &mut String, cursor: &mut usize, code: KeyCode) {
    let len = text.chars().count();
    *cursor = (*cursor).min(len);
    match code {
        KeyCode::Left => *cursor = cursor.saturating_sub(1),
        KeyCode::Right => *cursor = (*cursor + 1).min(len),
        KeyCode::Home => *cursor = 0,
        KeyCode::End => *cursor = len,
        KeyCode::Backspace if *cursor > 0 => {
            *cursor -= 1;
            text.remove(char_byte_index(text, *cursor));
        }
        KeyCode::Delete if *cursor < len => {
            text.remove(char_byte_index(text, *cursor));
        }
        KeyCode::Char(c) => {
            text.insert(char_byte_index(text, *cursor), c);
            *cursor += 1;
        }
        _ => {}
    }
}

/// Starting JSON for the Try box and the caret position in it: every required property with an
/// empty value of its type (`""`, `0`, `false`, `[]`, `{}`), caret inside the first `""`; or `{}`
/// with the caret between the braces when nothing is required.
fn try_args_template(inputs: &serde_json::Value) -> (String, usize) {
    let required: Vec<&str> = inputs
        .get("required")
        .and_then(|r| r.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let mut text = String::from("{");
    let mut caret = None;
    for (i, name) in required.iter().enumerate() {
        if i > 0 {
            text.push_str(", ");
        }
        text.push_str(&serde_json::Value::from(*name).to_string());
        text.push_str(": ");
        let ty = inputs
            .get("properties")
            .and_then(|p| p.get(*name))
            .and_then(|p| p.get("type"))
            .and_then(|t| t.as_str().or_else(|| t.as_array()?.first()?.as_str()));
        let empty = match ty {
            Some("integer" | "number") => "0",
            Some("boolean") => "false",
            Some("array") => "[]",
            Some("object") => "{}",
            _ => "\"\"",
        };
        if empty == "\"\"" && caret.is_none() {
            caret = Some(text.chars().count() + 1);
        }
        text.push_str(empty);
    }
    text.push('}');
    let caret = caret.unwrap_or(text.chars().count() - 1);
    (text, caret)
}

/// The Try box text as call arguments: a JSON object (empty text means `{}`).
fn parse_try_args(
    text: &str,
) -> Result<std::collections::HashMap<String, serde_json::Value>, String> {
    if text.trim().is_empty() {
        return Ok(Default::default());
    }
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(serde_json::Value::Object(map)) => Ok(map.into_iter().collect()),
        Ok(_) => Err("The arguments must be a JSON object, like {\"name\": \"value\"}.".into()),
        Err(e) => Err(format!(
            "The arguments are not valid JSON: {}. Press e to fix them.",
            e
        )),
    }
}

/// A call result as display text: strings as-is (CLI tools print text), anything else as
/// pretty JSON.
fn try_value_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}

fn validate_manual_name(name: &str) -> Result<(), String> {
    if name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        Ok(())
    } else {
        Err(format!(
            "Name '{}' may only use letters, digits, _ and -.",
            name
        ))
    }
}

/// Parses the MCP form. The server field is an http(s) URL, or a command line split on spaces
/// (no quoting) into the program and its arguments.
fn plan_mcp_import(form: &ImportForm) -> Result<McpImportPlan, String> {
    let server = form.source.trim();
    if server.is_empty() {
        return Err(
            "Enter the MCP server first: an https:// URL or a command such as \
             npx -y @upstash/context7-mcp (press e)."
                .into(),
        );
    }
    let items = form.auth_items();
    let literal_values = items.iter().any(|i| !i.contains("${"));
    let (url, command, args, env, headers) = if is_http_url(server) {
        if server.contains(char::is_whitespace) {
            return Err("A server URL cannot contain spaces.".into());
        }
        let headers = crate::parse_header_flags(&items).map_err(|e| {
            format!(
                "{}. For a URL write Header: value, e.g. Authorization: Bearer ${{TOKEN}}",
                e
            )
        })?;
        (
            Some(server.to_string()),
            None,
            Vec::new(),
            BTreeMap::new(),
            headers,
        )
    } else {
        if server.contains(['"', '\'']) {
            return Err(
                "Quotes are not supported here: each word is one argument. For arguments with \
                 spaces use the CLI: utcp import-mcp <name> --command <program> --arg <arg> ..."
                    .into(),
            );
        }
        let mut words = server.split_whitespace().map(str::to_string);
        let program = words.next().unwrap_or_default();
        let env = crate::parse_env_flags(&items).map_err(|e| {
            format!(
                "{}. For a command write KEY=value, e.g. API_KEY=${{API_KEY}}",
                e
            )
        })?;
        (None, Some(program), words.collect(), env, BTreeMap::new())
    };
    let name = match form.name.trim() {
        "" => infer_mcp_manual_name(command.as_deref(), &args, url.as_deref()),
        typed => typed.to_string(),
    };
    validate_manual_name(&name)?;
    Ok(McpImportPlan {
        name,
        url,
        command,
        args,
        env,
        headers,
        literal_values,
    })
}

/// The name Tab 3 uses when the Name field is empty.
fn inferred_import_name(mode: ImportMode, form: &ImportForm) -> Option<String> {
    let source = form.source.trim();
    if source.is_empty() {
        return None;
    }
    Some(match mode {
        ImportMode::Spec => infer_name_from_source(source),
        ImportMode::Mcp => match plan_mcp_import(&ImportForm {
            name: String::new(),
            auth: String::new(),
            ..form.clone()
        }) {
            Ok(plan) => plan.name,
            Err(_) => return None,
        },
    })
}

/// `~/x` -> `$HOME/x`; anything else unchanged.
fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

/// `path` with a leading `$HOME` shown as `~`.
fn display_with_tilde(path: &Path) -> String {
    let shown = path.to_string_lossy().into_owned();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && shown.starts_with(&home) => {
            format!("~{}", &shown[home.len()..])
        }
        _ => shown,
    }
}

/// The last `width` columns of `text`, so the end of a long field stays visible while typing.
fn tail_fit(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width || width == 0 {
        return text.to_string();
    }
    let mut out: String = "…".into();
    out.extend(text.chars().skip(count - width + 1));
    out
}

/// One Tab 1 row: a registered manual, or (with "All" on) one that is on disk but not registered.
#[derive(Debug, Clone)]
struct ManualRow {
    info: IndexedManual,
    registered: bool,
    /// Unregistered rows: `true` = deregistered with its settings kept, `false` = a saved file in
    /// `manuals/` with no registry entry.
    settings_kept: bool,
    /// Unregistered rows: whether the saved copy exists and parses, so `r` can register it.
    restorable: bool,
}

/// An action waiting for y/n in the confirmation popup.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ConfirmAction {
    Deregister(String),
    Purge(String),
    /// Tab 4 `w`: write `.agents/skills/utcp-tools/SKILL.md` into this folder.
    InstallFolderSkill(PathBuf),
}

/// Tab 4: the folder `utcp ui` was started from, seen as a per-project skill location. Agents that
/// open it as a project (Zed, Copilot CLI, agy) also load skills from its `.agents/skills/`.
#[derive(Debug, Clone)]
struct FolderSkillInfo {
    dir: PathBuf,
    /// Where `utcp-tools` would go, and whether it is there and current.
    target: IdeSkillTargetStatus,
    agents_dir_exists: bool,
    /// Other skill folders already in `.agents/skills/` (sorted).
    other_skills: Vec<String>,
}

fn inspect_folder_skill(paths: &UtcpPaths, dir: &Path) -> Option<FolderSkillInfo> {
    let target = inspect_skill_targets(paths, false, Some(dir))
        .into_iter()
        .next()?;
    let agents_dir = dir.join(".agents");
    let mut other_skills: Vec<String> = std::fs::read_dir(agents_dir.join("skills"))
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n != TOOLS_SKILL && n != ADMIN_SKILL)
                .collect()
        })
        .unwrap_or_default();
    other_skills.sort();
    Some(FolderSkillInfo {
        dir: dir.to_path_buf(),
        target,
        agents_dir_exists: agents_dir.is_dir(),
        other_skills,
    })
}

struct AppState {
    paths: UtcpPaths,
    explicit_config: Option<PathBuf>,
    active_tab: usize,
    should_quit: bool,
    status_message: String,

    // Snapshots
    daemon_info: Option<DaemonStatusInfo>,
    systemd_state_fallback: Option<String>,
    media_spool: MediaSpoolStatus,
    logging_on: bool,
    settings: UtcpSettings,
    index: SearchIndex,
    env_map: std::collections::HashMap<String, String>,
    ide_statuses: Vec<IdeSkillTargetStatus>,
    /// Tab 4: the folder `utcp ui` was started from (`None` in tests unless set) and its status.
    folder_dir: Option<PathBuf>,
    folder_skill: Option<FolderSkillInfo>,
    activities: Vec<ActivityEntry>,

    // Tab 1 state
    selected_manual: usize,
    /// `false` (default): only registered manuals. `true` (`a`): also deregistered/saved ones.
    show_all_manuals: bool,
    manual_rows: Vec<ManualRow>,
    /// Hints from the registry (active and deregistered manuals), by manual name.
    manual_hints: BTreeMap<String, String>,
    /// `h` popup; while `Some` it receives every key.
    hint_popup: Option<HintPopup>,
    /// Saving a hint rewrites the installed global skills. Only the real app does this; tests
    /// (built with `unloaded`) must never write to the real `$HOME`.
    refresh_global_skills: bool,
    /// While `Some`, the confirmation popup is open and receives every key.
    pending_confirm: Option<ConfirmAction>,
    /// Which popup button is highlighted (←/→ switch, Enter picks). Starts on No.
    confirm_yes: bool,

    // Tab 2 state
    search_query: String,
    search_editing: bool,
    search_results: Vec<ToolSummary>,
    search_latency_us: u128,
    selected_tool: usize,
    selected_schema: Option<ToolSchemaDetail>,
    schema_scroll: u16,
    /// Tab 2 was opened with Enter on a Tab 1 manual, so Esc/Backspace returns there.
    came_from_manuals: bool,
    search_focus: SearchPane,
    schema_view: SchemaView,
    // Layout facts recorded during the last draw (draw only gets `&AppState`), used to clamp
    // scrolling, size PgUp/PgDn pages, and route mouse events to the pane under the pointer.
    schema_max_scroll: StdCell<u16>,
    schema_view_rows: StdCell<u16>,
    matches_area: StdCell<Rect>,
    schema_area: StdCell<Rect>,
    /// `t` popup; `pending_try` is `Some(dry_run)` while a run waits for the next draw.
    try_popup: Option<TryPopup>,
    pending_try: Option<bool>,
    try_max_scroll: StdCell<u16>,
    try_view_rows: StdCell<u16>,

    // Tab 3 state
    import_mode: ImportMode,
    import_spec: ImportForm,
    import_mcp: ImportForm,
    import_field: ImportField,
    import_editing: bool,
    /// Set by `c`/`r` (`Some(also_register)`); the event loop runs it after the next draw, so
    /// the "working..." status is on screen while a slow server answers.
    pending_import: Option<bool>,

    // `?` help popup: open while `true`, receives every key.
    help_open: bool,
    help_scroll: u16,
    help_max_scroll: StdCell<u16>,
    help_view_rows: StdCell<u16>,

    // Tab 5 state
    selected_activity: usize,
    /// While `Some`, the details popup is open and receives every key. A copy of the entry, so
    /// the once-a-second reload (new rows arrive at the top) cannot swap it underneath.
    activity_detail: Option<ActivityEntry>,
    activity_detail_scroll: u16,
    activity_detail_max_scroll: StdCell<u16>,
    activity_detail_view_rows: StdCell<u16>,
}

impl AppState {
    fn new(paths: UtcpPaths, explicit_config: Option<PathBuf>) -> Result<Self> {
        let mut app = Self::unloaded(paths, explicit_config);
        app.folder_dir = std::env::current_dir().ok();
        app.refresh_global_skills = true;
        app.reload_snapshots();
        app.run_live_search();
        Ok(app)
    }

    /// Initial state without probing the daemon, systemd, or the catalog (also used by tests).
    fn unloaded(paths: UtcpPaths, explicit_config: Option<PathBuf>) -> Self {
        let media_spool = inspect_media_spool(&paths);
        Self {
            paths,
            explicit_config,
            active_tab: 0,
            should_quit: false,
            status_message:
                "Ready. Keys: «Tab/1-6: views» | «D: daemon» | «R: restart» | «E: sync env» | «L: log» | «?: help» | «q: quit»"
                    .to_string(),
            daemon_info: None,
            systemd_state_fallback: None,
            media_spool,
            logging_on: false,
            settings: UtcpSettings::default(),
            index: SearchIndex::default(),
            env_map: std::collections::HashMap::new(),
            ide_statuses: Vec::new(),
            folder_dir: None,
            folder_skill: None,
            activities: Vec::new(),
            selected_manual: 0,
            show_all_manuals: false,
            manual_rows: Vec::new(),
            manual_hints: BTreeMap::new(),
            hint_popup: None,
            refresh_global_skills: false,
            pending_confirm: None,
            confirm_yes: false,
            search_query: String::new(),
            search_editing: false,
            search_results: Vec::new(),
            search_latency_us: 0,
            selected_tool: 0,
            selected_schema: None,
            schema_scroll: 0,
            came_from_manuals: false,
            search_focus: SearchPane::Matches,
            schema_view: SchemaView::Schema,
            schema_max_scroll: StdCell::new(0),
            schema_view_rows: StdCell::new(0),
            matches_area: StdCell::new(Rect::default()),
            schema_area: StdCell::new(Rect::default()),
            try_popup: None,
            pending_try: None,
            try_max_scroll: StdCell::new(0),
            try_view_rows: StdCell::new(0),
            import_mode: ImportMode::Spec,
            import_spec: ImportForm::default(),
            import_mcp: ImportForm::default(),
            import_field: ImportField::Source,
            import_editing: false,
            pending_import: None,
            help_open: false,
            help_scroll: 0,
            help_max_scroll: StdCell::new(0),
            help_view_rows: StdCell::new(0),
            selected_activity: 0,
            activity_detail: None,
            activity_detail_scroll: 0,
            activity_detail_max_scroll: StdCell::new(0),
            activity_detail_view_rows: StdCell::new(0),
        }
    }

    fn reload_snapshots(&mut self) {
        self.daemon_info = query_daemon_status(&self.paths);
        self.systemd_state_fallback = if let Some(d) = &self.daemon_info {
            d.systemd_user_state.clone()
        } else {
            query_systemd_user_unit_state()
        };
        self.media_spool = if let Some(d) = &self.daemon_info {
            d.media_spool.clone()
        } else {
            inspect_media_spool(&self.paths)
        };
        self.logging_on = is_logging_enabled(&self.paths);
        self.settings = UtcpSettings::load(&self.paths);
        self.index = load_or_build_index_sync(&self.paths).unwrap_or_default();
        self.env_map = load_env_map(&self.paths);
        self.ide_statuses = inspect_skill_targets(&self.paths, true, None);
        self.reload_folder_skill();

        if self.daemon_info.is_some() {
            let req = IpcRequest {
                caller: "tui".into(),
                force_log: false,
                command: IpcCommand::RecentActivity { limit: 200 },
            };
            if let Ok(Some(IpcResponse::Ok {
                data: IpcPayload::Activity(items),
            })) = try_send_ipc_sync(&self.paths, &req)
            {
                self.activities = items;
            } else {
                self.activities = read_recent_activity(&self.paths, 200);
            }
        } else {
            self.activities = read_recent_activity(&self.paths, 200);
        }

        self.rebuild_manual_rows();
    }

    /// Rebuilds the Tab 1 rows from the index (plus unregistered manuals when "All" is on), keeping
    /// the same manual selected by name when it is still listed.
    fn rebuild_manual_rows(&mut self) {
        let selected_name = self
            .manual_rows
            .get(self.selected_manual)
            .map(|r| r.info.name.clone());
        let mut rows: Vec<ManualRow> = self
            .index
            .manuals
            .iter()
            .map(|m| ManualRow {
                info: m.clone(),
                registered: true,
                settings_kept: true,
                restorable: true,
            })
            .collect();
        if self.show_all_manuals {
            rows.extend(
                list_unregistered_manuals(&self.paths)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|u| ManualRow {
                        info: u.manual,
                        registered: false,
                        settings_kept: u.settings_kept,
                        restorable: u.restorable,
                    }),
            );
        }
        rows.sort_by(|a, b| a.info.name.cmp(&b.info.name));
        self.manual_rows = rows;
        self.manual_hints = UtcpRegistryConfig::load(&self.paths)
            .map(|cfg| {
                cfg.deregistered
                    .into_iter()
                    .chain(cfg.manuals)
                    .filter_map(|(name, e)| e.hint.map(|h| (name, h)))
                    .collect()
            })
            .unwrap_or_default();

        if let Some(pos) =
            selected_name.and_then(|n| self.manual_rows.iter().position(|r| r.info.name == n))
        {
            self.selected_manual = pos;
        } else if self.selected_manual >= self.manual_rows.len() {
            self.selected_manual = self.manual_rows.len().saturating_sub(1);
        }
    }

    fn selected_manual_row(&self) -> Option<&ManualRow> {
        self.manual_rows.get(self.selected_manual)
    }

    fn toggle_show_all_manuals(&mut self) {
        self.show_all_manuals = !self.show_all_manuals;
        self.rebuild_manual_rows();
        let unregistered = self.manual_rows.iter().filter(|r| !r.registered).count();
        self.status_message = if self.show_all_manuals {
            format!(
                "Showing all manuals ({} not registered, dimmed). «r» on a dimmed row registers it | «a: registered only».",
                unregistered
            )
        } else {
            "Showing registered manuals only | «a: show all» (incl. deregistered).".into()
        };
    }

    /// Tab 1 `h`: opens the hint editor for the selected manual, prefilled with its hint.
    fn open_hint_popup(&mut self) {
        let Some(row) = self.selected_manual_row() else {
            return;
        };
        let name = row.info.name.clone();
        if !row.registered && !row.settings_kept {
            self.status_message = format!(
                "'{}' is only a saved file; register it first («r»), then add a hint.",
                name
            );
            return;
        }
        let text = self.manual_hints.get(&name).cloned().unwrap_or_default();
        self.hint_popup = Some(HintPopup {
            manual: name,
            cursor: text.chars().count(),
            text,
            error: None,
        });
    }

    fn handle_hint_key(&mut self, code: KeyCode) {
        let Some(p) = self.hint_popup.as_mut() else {
            return;
        };
        match code {
            KeyCode::Esc => {
                self.hint_popup = None;
                self.status_message = "Hint not changed.".into();
            }
            KeyCode::Enter => self.save_hint(),
            _ => {
                edit_text_at_caret(&mut p.text, &mut p.cursor, code);
                p.error = None;
            }
        }
    }

    /// Enter in the hint popup: saves (empty text clears) and, in the real app, rewrites the
    /// installed skills so agents see the new description right away.
    fn save_hint(&mut self) {
        let Some(p) = self.hint_popup.as_mut() else {
            return;
        };
        let name = p.manual.clone();
        match set_manual_hint(&self.paths, &name, Some(&p.text)) {
            Ok(update) => {
                self.hint_popup = None;
                if update.active && self.refresh_global_skills {
                    // Hint words are searchable; the daemon keeps the index in memory.
                    notify_daemon_reload(&self.paths);
                    refresh_installed_global_skills(&self.paths);
                }
                let label = manual_label(&name, update.hint.as_deref());
                self.status_message = match (update.hint.is_some(), update.active) {
                    (true, true) => format!("Hint saved. Agents' skills now list: {}", label),
                    (false, true) => {
                        format!(
                            "Hint cleared. Agents' skills now show '{}' with no hint.",
                            name
                        )
                    }
                    (true, false) => format!(
                        "Hint saved; '{}' is deregistered, so skills show it after «r».",
                        name
                    ),
                    (false, false) => format!("Hint cleared for '{}'.", name),
                };
                if self.refresh_global_skills {
                    self.reload_snapshots();
                    self.run_live_search();
                } else {
                    self.rebuild_manual_rows();
                }
            }
            Err(e) => p.error = Some(format!("{:#}", e)),
        }
    }

    fn run_live_search(&mut self) {
        let t0 = Instant::now();
        self.search_results = self
            .index
            .search(&self.search_query, TUI_SEARCH_LIMIT, false);
        self.search_latency_us = t0.elapsed().as_micros();
        if self.selected_tool >= self.search_results.len() {
            self.selected_tool = 0;
        }
        self.update_selected_schema();
    }

    /// Tab 1 Enter: show the selected manual's tools in Tab 2 (`<manual>.` scoped search), first
    /// tool selected and the Matches pane focused so ↑/↓ step through tools immediately.
    fn open_selected_manual_in_search(&mut self) {
        let Some(row) = self.selected_manual_row() else {
            return;
        };
        if !row.registered {
            self.status_message = format!(
                "'{}' is not registered, so its tools are not searchable. Press «r» to register it.",
                row.info.name
            );
            return;
        }
        let name = row.info.name.clone();
        self.search_query = format!("{}.", name);
        self.search_editing = false;
        self.selected_tool = 0;
        self.run_live_search();
        self.search_focus = SearchPane::Matches;
        self.came_from_manuals = true;
        self.active_tab = 1;
        self.status_message = format!(
            "{} tool(s) in '{}': «↑/↓: select», «→ or wheel: read», «d: Schema/Docs», «t: try» | «Esc/Bksp: back to Manuals».",
            self.search_results.len(),
            name
        );
    }

    fn toggle_schema_view(&mut self) {
        self.schema_view = match self.schema_view {
            SchemaView::Schema => SchemaView::Docs,
            SchemaView::Docs => SchemaView::Schema,
        };
        self.schema_scroll = 0;
    }

    fn update_selected_schema(&mut self) {
        self.schema_scroll = 0;
        if let Some(item) = self.search_results.get(self.selected_tool) {
            self.selected_schema = self.index.get_schema(&item.name);
        } else {
            self.selected_schema = None;
        }
    }

    fn select_tool_delta(&mut self, forward: bool) {
        let n = self.search_results.len();
        if n == 0 {
            return;
        }
        self.selected_tool = if forward {
            (self.selected_tool + 1) % n
        } else {
            (self.selected_tool + n - 1) % n
        };
        self.update_selected_schema();
    }

    /// Scrolls the Schema & Docs pane by `delta` wrapped rows, clamped to the last drawn layout.
    fn scroll_schema(&mut self, delta: i32) {
        let max = i32::from(self.schema_max_scroll.get());
        let next = (i32::from(self.schema_scroll) + delta).clamp(0, max);
        self.schema_scroll = next as u16;
    }

    /// One PgUp/PgDn step: the pane's visible height minus one row of overlap for context.
    fn schema_page_rows(&self) -> i32 {
        i32::from(self.schema_view_rows.get().saturating_sub(1).max(1))
    }

    /// Scrolls the `?` help popup by `delta` wrapped rows, clamped to the last drawn layout.
    fn scroll_help(&mut self, delta: i32) {
        let max = i32::from(self.help_max_scroll.get());
        self.help_scroll = (i32::from(self.help_scroll) + delta).clamp(0, max) as u16;
    }

    fn handle_help_key(&mut self, code: KeyCode) {
        let page = i32::from(self.help_view_rows.get().saturating_sub(1).max(1));
        match code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('?') => {
                self.help_open = false
            }
            KeyCode::Down | KeyCode::Char('j') => self.scroll_help(1),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_help(-1),
            KeyCode::PageDown | KeyCode::Char(']') | KeyCode::Char(' ') => self.scroll_help(page),
            KeyCode::PageUp | KeyCode::Char('[') => self.scroll_help(-page),
            KeyCode::Home => self.help_scroll = 0,
            KeyCode::End => self.help_scroll = self.help_max_scroll.get(),
            _ => {}
        }
    }

    /// Tab 2 `t`: opens the Try popup for the selected tool, arguments prefilled from its schema.
    fn open_try_popup(&mut self) {
        let Some(schema) = self.selected_schema.as_ref() else {
            self.status_message = "Select a tool first («↑/↓» in Matches), then press «t».".into();
            return;
        };
        let (args, cursor) = try_args_template(&schema.inputs);
        // Required arguments need values, so start typing; with none, d/Enter work right away.
        let editing = args != "{}";
        self.try_popup = Some(TryPopup {
            tool: schema.name.clone(),
            args,
            cursor,
            editing,
            output: TryOutput::Idle,
            scroll: 0,
        });
        self.try_max_scroll.set(0);
    }

    fn scroll_try(&mut self, delta: i32) {
        let max = i32::from(self.try_max_scroll.get());
        if let Some(p) = self.try_popup.as_mut() {
            p.scroll = (i32::from(p.scroll) + delta).clamp(0, max) as u16;
        }
    }

    fn handle_try_key(&mut self, code: KeyCode) {
        let page = i32::from(self.try_view_rows.get().saturating_sub(1).max(1));
        let max_scroll = self.try_max_scroll.get();
        let Some(p) = self.try_popup.as_mut() else {
            return;
        };
        if p.editing {
            match code {
                KeyCode::Esc | KeyCode::Enter => p.editing = false,
                _ => edit_text_at_caret(&mut p.args, &mut p.cursor, code),
            }
            return;
        }
        match code {
            KeyCode::Esc | KeyCode::Char('q') => self.try_popup = None,
            KeyCode::Char('e') | KeyCode::Char('/') => p.editing = true,
            KeyCode::Char('d') => self.request_try(true),
            KeyCode::Enter => self.request_try(false),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_try(1),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_try(-1),
            KeyCode::PageDown | KeyCode::Char(']') => self.scroll_try(page),
            KeyCode::PageUp | KeyCode::Char('[') => self.scroll_try(-page),
            KeyCode::Home => p.scroll = 0,
            KeyCode::End => p.scroll = max_scroll,
            _ => {}
        }
    }

    /// `d` / Enter in the Try popup: checks the JSON, then queues the run for after the next draw
    /// (like imports), so "Calling..." is on screen while a slow tool answers.
    fn request_try(&mut self, dry_run: bool) {
        let Some(p) = self.try_popup.as_mut() else {
            return;
        };
        p.scroll = 0;
        self.try_max_scroll.set(0);
        match parse_try_args(&p.args) {
            Ok(_) => {
                p.output = TryOutput::Running { dry_run };
                self.pending_try = Some(dry_run);
            }
            Err(e) => p.output = TryOutput::Error(e),
        }
    }

    fn run_pending_try(&mut self) {
        let Some(dry_run) = self.pending_try.take() else {
            return;
        };
        let Some(p) = self.try_popup.as_ref() else {
            return;
        };
        let tool = p.tool.clone();
        let started = Instant::now();
        let outcome = parse_try_args(&p.args).and_then(|args| self.call_tool(&tool, args, dry_run));
        let ms = started.elapsed().as_millis();
        let (output, status) = match outcome {
            Ok(value) if dry_run => (
                TryOutput::DryRun(try_value_text(&value)),
                format!("Dry run of '{}': nothing was sent.", tool),
            ),
            Ok(value) => (
                TryOutput::Result(try_value_text(&value)),
                format!("Called '{}': OK in {} ms.", tool, ms),
            ),
            Err(e) => (
                TryOutput::Error(activity_detail(&e, &load_env_map(&self.paths))),
                format!(
                    "'{}' failed after {} ms; the error is in the Try popup.",
                    tool, ms
                ),
            ),
        };
        self.status_message = status;
        if let Some(p) = self.try_popup.as_mut() {
            p.output = output;
            p.scroll = 0;
        }
        self.try_max_scroll.set(0);
    }

    /// Runs one tool the way `utcp call` does: through the daemon when it is up (it logs the
    /// call), otherwise directly, logging like the CLI's direct mode. Caller is `tui`.
    fn call_tool(
        &self,
        tool: &str,
        args: std::collections::HashMap<String, serde_json::Value>,
        dry_run: bool,
    ) -> Result<serde_json::Value, String> {
        if self.daemon_info.is_some() {
            let req = IpcRequest {
                caller: "tui".into(),
                force_log: false,
                command: IpcCommand::Call {
                    target: tool.to_string(),
                    args: args.clone(),
                    dry_run,
                    fields: None,
                    filters: Vec::new(),
                    caller_env: manual_caller_env(&self.paths),
                    staged: false,
                },
            };
            match try_send_ipc_sync(&self.paths, &req) {
                Ok(Some(IpcResponse::Ok {
                    data: IpcPayload::CallResult(value),
                })) => return Ok(value),
                Ok(Some(IpcResponse::Error { message })) => return Err(message),
                Ok(Some(_)) => return Err("Unexpected reply from the daemon.".into()),
                Err(e) => return Err(format!("{:#}", e)),
                // The daemon stopped since the last check: run directly instead.
                Ok(None) => {}
            }
        }
        let started = Instant::now();
        let outcome = block_on(call_tool_direct(
            &self.paths,
            tool,
            args,
            dry_run,
            Default::default(),
            false,
            None,
        ))
        .and_then(|r| r.map_err(|e| format!("{:#}", e)));
        let ok = outcome.is_ok();
        let detail = outcome
            .as_ref()
            .err()
            .map(|e| activity_detail(e, &load_env_map(&self.paths)));
        crate::maybe_log_direct(
            &self.paths,
            "tui",
            if dry_run { "dry-run" } else { "call" },
            tool,
            ok,
            if ok { "OK" } else { "ERR" },
            started,
            false,
            detail,
        );
        outcome
    }

    /// Tab 5 Enter: opens the details popup for the selected activity row.
    fn open_activity_detail(&mut self) {
        if self.activities.is_empty() {
            return;
        }
        let i = self.selected_activity.min(self.activities.len() - 1);
        self.activity_detail = Some(self.activities[i].clone());
        self.activity_detail_scroll = 0;
        self.activity_detail_max_scroll.set(0);
    }

    /// Scrolls the details popup by `delta` wrapped rows, clamped to the last drawn layout.
    fn scroll_activity_detail(&mut self, delta: i32) {
        let max = i32::from(self.activity_detail_max_scroll.get());
        let next = (i32::from(self.activity_detail_scroll) + delta).clamp(0, max);
        self.activity_detail_scroll = next as u16;
    }

    fn handle_activity_detail_key(&mut self, code: KeyCode) {
        let page = i32::from(
            self.activity_detail_view_rows
                .get()
                .saturating_sub(1)
                .max(1),
        );
        match code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => self.activity_detail = None,
            KeyCode::Down | KeyCode::Char('j') => self.scroll_activity_detail(1),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_activity_detail(-1),
            KeyCode::PageDown | KeyCode::Char(']') => self.scroll_activity_detail(page),
            KeyCode::PageUp | KeyCode::Char('[') => self.scroll_activity_detail(-page),
            KeyCode::Home => self.activity_detail_scroll = 0,
            KeyCode::End => self.activity_detail_scroll = self.activity_detail_max_scroll.get(),
            _ => {}
        }
    }

    fn toggle_daemon(&mut self) {
        if self.daemon_info.is_some() {
            match stop_daemon(&self.paths) {
                Ok(_) => self.status_message = "Stopped utcp daemon (now in Direct Mode).".into(),
                Err(e) => self.status_message = format!("Failed to stop daemon: {}", e),
            }
        } else {
            match start_daemon_background(&self.paths, self.explicit_config.as_deref(), None) {
                Ok(info) => {
                    self.status_message = format!(
                        "Started utcp daemon (PID {}, {} env vars used by manuals).",
                        info.pid, info.env_var_count
                    );
                }
                Err(e) => self.status_message = format!("Failed to start daemon: {}", e),
            }
        }
        self.reload_snapshots();
    }

    fn restart_daemon(&mut self) {
        let _ = stop_daemon(&self.paths);
        match start_daemon_background(&self.paths, self.explicit_config.as_deref(), None) {
            Ok(info) => {
                self.status_message = format!(
                    "Restarted utcp daemon (PID {}, {} env vars used by manuals).",
                    info.pid, info.env_var_count
                );
            }
            Err(e) => self.status_message = format!("Failed to restart daemon: {}", e),
        }
        self.reload_snapshots();
    }

    fn sync_env_to_daemon(&mut self) {
        match sync_caller_env_to_daemon(&self.paths) {
            Ok(Some(summary)) => {
                let secrets = match summary.secrets_command {
                    Some(s) if s.error.is_some() => {
                        " Secrets command failed (see the Daemon Monitor)."
                    }
                    Some(_) => " Secrets command re-run.",
                    None => "",
                };
                self.status_message = format!(
                    "Synced the variables manuals use from this shell into daemon RAM ({} held).{}",
                    summary.total_vars, secrets
                );
            }
            Ok(None) => {
                self.status_message =
                    "Daemon is stopped; start it first (Shift+D) before syncing live env.".into();
            }
            Err(e) => {
                self.status_message = format!("Failed to sync environment to daemon: {}", e);
            }
        }
        self.reload_snapshots();
    }

    fn toggle_logging(&mut self) {
        let next = !self.logging_on;
        match set_logging_enabled(&self.paths, next) {
            Ok(()) => {
                self.logging_on = next;
                self.status_message = format!(
                    "Disk activity logging turned {}.",
                    if next { "ON" } else { "OFF" }
                );
            }
            Err(e) => self.status_message = format!("Failed to toggle logging: {}", e),
        }
        self.reload_snapshots();
    }

    fn toggle_cache_on_register(&mut self) {
        let wanted = !self.settings.cache_on_register;
        match utcp_core::settings_file::set_top_level_setting(
            &self.paths,
            "cache_on_register",
            wanted.into(),
        ) {
            Ok(()) => {
                self.settings.cache_on_register = wanted;
                self.status_message =
                    format!("Saved settings.toml: cache_on_register = {}", wanted);
            }
            Err(e) => self.status_message = format!("Failed saving settings.toml: {:#}", e),
        }
    }

    fn reload_folder_skill(&mut self) {
        self.folder_skill = self
            .folder_dir
            .as_deref()
            .and_then(|d| inspect_folder_skill(&self.paths, d));
    }

    /// Tab 4 `s`: rewrites the account-wide skill files. This folder's copy is only updated when it
    /// is already installed; adding it is `w`, which asks first.
    fn sync_ides(&mut self) {
        let mut msg = match sync_ide_skills(&self.paths, true, None) {
            Ok(res) => {
                let n = res.len();
                self.ide_statuses = res;
                format!(
                    "Synchronized the utcp skills in {} account-wide skills folder(s).",
                    n
                )
            }
            Err(e) => format!("Skill sync failed: {}", e),
        };
        if let Some(folder) = &self.folder_skill {
            if folder.target.exists {
                match sync_ide_skills(&self.paths, false, Some(&folder.dir)) {
                    Ok(_) => msg.push_str(" This folder's copy updated too."),
                    Err(e) => msg.push_str(&format!(" This folder's copy failed: {}", e)),
                }
            } else {
                msg.push_str(" This folder has no copy (w adds one).");
            }
        }
        self.reload_folder_skill();
        self.status_message = msg;
    }

    /// Tab 4 `w`: asks before adding the skill to the folder `utcp ui` was started from.
    fn request_install_folder_skill(&mut self) {
        let Some(folder) = &self.folder_skill else {
            self.status_message = "No current folder to add a skill to.".into();
            return;
        };
        if folder.target.up_to_date {
            self.status_message = format!("Already up to date: {}", folder.target.path);
            return;
        }
        self.confirm_yes = false;
        self.pending_confirm = Some(ConfirmAction::InstallFolderSkill(folder.dir.clone()));
    }

    fn install_folder_skill(&mut self, dir: &Path) {
        self.status_message = match sync_ide_skills(&self.paths, false, Some(dir)) {
            Ok(res) => format!(
                "Added the utcp-tools and utcp-admin skills: {}. Agents that open this folder as a project will load them.",
                res.first().map(|s| s.path.as_str()).unwrap_or_default()
            ),
            Err(e) => format!("Could not add the skills in {}: {}", dir.display(), e),
        };
        self.reload_folder_skill();
    }

    fn import_form(&self) -> &ImportForm {
        match self.import_mode {
            ImportMode::Spec => &self.import_spec,
            ImportMode::Mcp => &self.import_mcp,
        }
    }

    fn import_form_mut(&mut self) -> &mut ImportForm {
        match self.import_mode {
            ImportMode::Spec => &mut self.import_spec,
            ImportMode::Mcp => &mut self.import_mcp,
        }
    }

    fn move_import_field(&mut self, forward: bool) {
        let i = IMPORT_FIELDS
            .iter()
            .position(|f| *f == self.import_field)
            .unwrap_or(0);
        let n = IMPORT_FIELDS.len();
        self.import_field = IMPORT_FIELDS[if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        }];
    }

    /// Tab 3 `m`: switches between OpenAPI / UTCP and MCP server imports.
    fn toggle_import_mode(&mut self) {
        self.import_mode = match self.import_mode {
            ImportMode::Spec => ImportMode::Mcp,
            ImportMode::Mcp => ImportMode::Spec,
        };
        self.import_field = ImportField::Source;
        self.status_message = match self.import_mode {
            ImportMode::Spec => "Import mode: OpenAPI / UTCP spec (URL or file).".into(),
            ImportMode::Mcp => {
                "Import mode: MCP server (URL, or the command that starts it). «?» explains.".into()
            }
        };
    }

    /// Tab 3 `c` / `r`: checks the form, then queues the import so the event loop can draw the
    /// "working" status before the (possibly slow) fetch or MCP handshake blocks.
    fn request_import(&mut self, also_register: bool) {
        if self.import_form().source.trim().is_empty() {
            self.status_message = match self.import_mode {
                ImportMode::Spec => "Enter a Source URL or file path first (press «e»).".into(),
                ImportMode::Mcp => "Enter the MCP server first: an https:// URL or a command \
                                    (press e; ? shows examples)."
                    .into(),
            };
            return;
        }
        self.status_message = match self.import_mode {
            ImportMode::Spec => "Fetching and converting the source...".into(),
            ImportMode::Mcp => {
                "Asking the MCP server for its tools (initialize + tools/list, up to 60 s)..."
                    .into()
            }
        };
        self.pending_import = Some(also_register);
    }

    fn run_pending_import(&mut self) {
        if let Some(also_register) = self.pending_import.take() {
            self.execute_import(also_register);
        }
    }

    /// Where `c` (save without registering) writes when the Output field is empty: a utcp-owned
    /// folder rather than whatever directory `utcp ui` was started from.
    fn default_import_output(&self, name: &str) -> PathBuf {
        self.paths
            .cache_dir
            .join("imports")
            .join(format!("{}.utcp.json", name))
    }

    fn execute_import(&mut self, also_register: bool) {
        let form = self.import_form().clone();
        let outcome = match self.import_mode {
            ImportMode::Spec => self.import_spec_form(&form, also_register),
            ImportMode::Mcp => self.import_mcp_form(&form, also_register),
        };
        match outcome {
            Ok((summary, literal_values)) => {
                let mut msg = if also_register {
                    notify_daemon_reload(&self.paths);
                    format!(
                        "Registered '{}' ({} tools). Tab 2: type '{}.' to see them.",
                        summary.manual_name, summary.tool_count, summary.manual_name
                    )
                } else {
                    format!(
                        "Saved '{}' ({} tools) to {} (not registered; «r» registers).",
                        summary.manual_name,
                        summary.tool_count,
                        display_with_tilde(Path::new(&summary.output_path))
                    )
                };
                if !summary.required_env_vars.is_empty() {
                    msg.push_str(&format!(
                        " Needs: {} (in {}).",
                        summary.required_env_vars.join(", "),
                        short_path(&self.paths.env_file)
                    ));
                }
                if literal_values {
                    msg.push_str(" Note: values without ${VAR} were saved as typed.");
                }
                if also_register && form.hint_arg().is_none() {
                    msg.push_str(" No hint yet: Tab 1 «h» adds a few words for agents.");
                }
                self.status_message = msg;
                self.reload_snapshots();
                self.run_live_search();
            }
            Err(e) => self.status_message = format!("Import failed: {}", e),
        }
    }

    fn import_spec_form(
        &self,
        form: &ImportForm,
        also_register: bool,
    ) -> Result<(ImportSummary, bool), String> {
        let source = form.source.trim();
        let source = expand_home(source).to_string_lossy().into_owned();
        let items = form.auth_items();
        let literal_values = items.iter().any(|i| !i.contains("${"));
        let headers = crate::parse_header_flags(&items).map_err(|e| {
            format!(
                "{}. Headers are Header: value, e.g. Authorization: Bearer ${{TOKEN}}",
                e
            )
        })?;
        let name = match form.name.trim() {
            "" => infer_name_from_source(&source),
            typed => typed.to_string(),
        };
        validate_manual_name(&name)?;
        let typed_output = Some(form.output.trim())
            .filter(|s| !s.is_empty())
            .map(expand_home);

        let summary = if also_register {
            let summary = block_on(register_manual(
                &self.paths,
                &name,
                &source,
                Some(true),
                None,
                headers,
                form.hint_arg(),
            ))?
            .map_err(|e| format!("{:#}", e))?;
            if let Some(out) = &typed_output {
                if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::copy(&summary.output_path, out).map_err(|e| {
                    format!("registered, but copying to {} failed: {}", out.display(), e)
                })?;
            }
            summary
        } else {
            let out = typed_output.unwrap_or_else(|| self.default_import_output(&name));
            block_on(import_manual_to_file(
                &self.paths,
                &source,
                Some(&name),
                Some(&out),
                None,
                &headers,
            ))?
            .map_err(|e| format!("{:#}", e))?
        };
        Ok((summary, literal_values))
    }

    fn import_mcp_form(
        &self,
        form: &ImportForm,
        also_register: bool,
    ) -> Result<(ImportSummary, bool), String> {
        let plan = plan_mcp_import(form)?;
        let output = match form.output.trim() {
            "" if also_register => None,
            "" => Some(self.default_import_output(&plan.name)),
            typed => Some(expand_home(typed)),
        };
        let summary = block_on(import_mcp_manual(
            &self.paths,
            &plan.name,
            plan.command.as_deref(),
            plan.args.clone(),
            plan.env.clone(),
            plan.url.as_deref(),
            plan.headers.clone(),
            output.as_deref(),
            also_register,
            if also_register { form.hint_arg() } else { None },
        ))?
        .map_err(|e| format!("{:#}", e))?;
        Ok((summary, plan.literal_values))
    }

    /// Tab 1 `r`: refreshes a registered manual from its source, or registers an unregistered one
    /// from its saved copy.
    fn refresh_or_register_selected_manual(&mut self) {
        let Some(row) = self.selected_manual_row() else {
            return;
        };
        if row.registered {
            self.refresh_selected_manual();
            return;
        }
        let target = row.info.name.clone();
        match reregister_manual(&self.paths, &target) {
            Ok(summary) => {
                notify_daemon_reload(&self.paths);
                self.status_message = format!(
                    "Registered '{}' from its saved copy ({} tools).",
                    target, summary.tool_count
                );
                self.reload_snapshots();
                self.run_live_search();
            }
            Err(e) => self.status_message = format!("Register failed: {}", e),
        }
    }

    fn refresh_selected_manual(&mut self) {
        let Some(row) = self.selected_manual_row() else {
            return;
        };
        let target = row.info.name.clone();
        let paths = self.paths.clone();
        if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            match rt.block_on(refresh_manuals(&paths, std::slice::from_ref(&target))) {
                Ok(_) => {
                    notify_daemon_reload(&self.paths);
                    self.status_message = format!("Refreshed manual '{}'.", target);
                    self.reload_snapshots();
                    self.run_live_search();
                }
                Err(e) => self.status_message = format!("Refresh failed: {}", e),
            }
        }
    }

    /// Tab 1 `d` / `p`: opens the confirmation popup instead of acting right away.
    fn request_remove_selected_manual(&mut self, purge: bool) {
        let Some(row) = self.selected_manual_row() else {
            return;
        };
        let name = row.info.name.clone();
        if !purge && !row.registered {
            self.status_message = format!(
                "'{}' is already not registered. «r» registers it again; «p» deletes it for good.",
                name
            );
            return;
        }
        self.confirm_yes = false;
        self.pending_confirm = Some(if purge {
            ConfirmAction::Purge(name)
        } else {
            ConfirmAction::Deregister(name)
        });
    }

    /// Popup keys: `y` runs the pending action; `n`, `N`, and Esc cancel; ←/→ (or h/l, Tab) move
    /// between the Yes and No buttons and Enter picks the highlighted one. Anything else is ignored.
    fn handle_confirm_key(&mut self, code: KeyCode) {
        let confirmed = match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => true,
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => false,
            KeyCode::Enter => self.confirm_yes,
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Char('h')
            | KeyCode::Char('l')
            | KeyCode::Tab
            | KeyCode::BackTab => {
                self.confirm_yes = !self.confirm_yes;
                return;
            }
            _ => return,
        };
        let Some(action) = self.pending_confirm.take() else {
            return;
        };
        if confirmed {
            self.execute_confirmed(action);
        } else {
            self.status_message = "Cancelled; nothing changed.".into();
        }
    }

    fn execute_confirmed(&mut self, action: ConfirmAction) {
        let (target, purge) = match action {
            ConfirmAction::Deregister(n) => (n, false),
            ConfirmAction::Purge(n) => (n, true),
            ConfirmAction::InstallFolderSkill(dir) => {
                self.install_folder_skill(&dir);
                return;
            }
        };
        match deregister_manual(&self.paths, &target, purge) {
            Ok(DeregisterOutcome::Deregistered) => {
                notify_daemon_reload(&self.paths);
                self.status_message = if self.show_all_manuals {
                    format!("Deregistered '{}'. «r» registers it again.", target)
                } else {
                    format!(
                        "Deregistered '{}'. a shows all manuals; r there registers it again.",
                        target
                    )
                };
                self.reload_snapshots();
                self.run_live_search();
            }
            Ok(DeregisterOutcome::Purged) => {
                notify_daemon_reload(&self.paths);
                self.status_message =
                    format!("Purged '{}' (registry entry and saved copy).", target);
                self.reload_snapshots();
                self.run_live_search();
            }
            Ok(DeregisterOutcome::NotFound) => {
                self.status_message = format!("Manual '{}' not found.", target)
            }
            Err(e) => self.status_message = format!("Deregister failed: {}", e),
        }
    }
}

pub fn run_tui(paths: UtcpPaths, explicit_config: Option<&Path>) -> Result<()> {
    let mut app = AppState::new(paths.clone(), explicit_config.map(Path::to_path_buf))?;

    let (fs_tx, fs_rx) = mpsc::channel::<()>();
    let mut _watcher: Option<RecommendedWatcher> =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if res.is_ok() {
                let _ = fs_tx.send(());
            }
        })
        .ok();
    if let Some(w) = _watcher.as_mut() {
        let _ = w.watch(&paths.config_dir, RecursiveMode::NonRecursive);
        let _ = w.watch(&paths.manuals_dir, RecursiveMode::NonRecursive);
        let _ = w.watch(&paths.state_dir, RecursiveMode::NonRecursive);
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let loop_res = run_event_loop(&mut terminal, &mut app, &fs_rx);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;

    loop_res
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut AppState,
    fs_rx: &mpsc::Receiver<()>,
) -> Result<()> {
    let mut last_tick = Instant::now();

    while !app.should_quit {
        terminal.draw(|f| draw_ui(f, app))?;
        if app.pending_import.is_some() {
            // The "working" status was just drawn; now run the import, then redraw.
            app.run_pending_import();
            continue;
        }
        if app.pending_try.is_some() {
            // Same for a Try run: "Calling..." is on screen while the tool answers.
            app.run_pending_try();
            continue;
        }

        if event::poll(Duration::from_millis(120))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => handle_key(app, key.code),
                Event::Mouse(mouse) => handle_mouse(app, mouse),
                _ => {}
            }
        }

        let mut dirty = false;
        while fs_rx.try_recv().is_ok() {
            dirty = true;
        }
        if dirty || last_tick.elapsed() >= Duration::from_secs(2) {
            app.reload_snapshots();
            last_tick = Instant::now();
        }
    }
    Ok(())
}

fn handle_key(app: &mut AppState, code: KeyCode) {
    if app.pending_confirm.is_some() {
        app.handle_confirm_key(code);
        return;
    }
    if app.activity_detail.is_some() {
        app.handle_activity_detail_key(code);
        return;
    }
    if app.help_open {
        app.handle_help_key(code);
        return;
    }
    if app.try_popup.is_some() {
        app.handle_try_key(code);
        return;
    }
    if app.hint_popup.is_some() {
        app.handle_hint_key(code);
        return;
    }

    if app.active_tab == 1 && app.search_editing {
        match code {
            // Arrows leave the box toward the pane they point at, like Esc/Enter do.
            KeyCode::Esc | KeyCode::Enter | KeyCode::Down | KeyCode::Left => {
                app.search_editing = false;
                app.search_focus = SearchPane::Matches;
            }
            KeyCode::Right => {
                app.search_editing = false;
                app.search_focus = SearchPane::Schema;
            }
            KeyCode::Backspace => {
                app.search_query.pop();
                app.run_live_search();
            }
            KeyCode::Char(c) => {
                app.search_query.push(c);
                app.run_live_search();
            }
            _ => {}
        }
        return;
    }

    if app.active_tab == 2 && app.import_editing {
        let field = app.import_field;
        match code {
            KeyCode::Esc | KeyCode::Enter => {
                app.import_editing = false;
            }
            KeyCode::Tab | KeyCode::Down => app.move_import_field(true),
            KeyCode::BackTab | KeyCode::Up => app.move_import_field(false),
            KeyCode::Backspace => {
                app.import_form_mut().field_mut(field).pop();
            }
            KeyCode::Char(c) => app.import_form_mut().field_mut(field).push(c),
            _ => {}
        }
        return;
    }

    // Global hotkeys
    match code {
        KeyCode::Char('q') => {
            app.should_quit = true;
            return;
        }
        KeyCode::Tab => {
            app.active_tab = (app.active_tab + 1) % TAB_TITLES.len();
            return;
        }
        KeyCode::BackTab => {
            app.active_tab = (app.active_tab + TAB_TITLES.len() - 1) % TAB_TITLES.len();
            return;
        }
        KeyCode::Char('1') => {
            app.active_tab = 0;
            return;
        }
        KeyCode::Char('2') => {
            app.active_tab = 1;
            return;
        }
        KeyCode::Char('3') => {
            app.active_tab = 2;
            return;
        }
        KeyCode::Char('4') => {
            app.active_tab = 3;
            return;
        }
        KeyCode::Char('5') => {
            app.active_tab = 4;
            return;
        }
        KeyCode::Char('6') => {
            app.active_tab = 5;
            return;
        }
        KeyCode::Char('D') => {
            app.toggle_daemon();
            return;
        }
        KeyCode::Char('R') => {
            app.restart_daemon();
            return;
        }
        KeyCode::Char('E') => {
            app.sync_env_to_daemon();
            return;
        }
        KeyCode::Char('L') => {
            app.toggle_logging();
            return;
        }
        KeyCode::Char('?') => {
            app.help_open = true;
            app.help_scroll = 0;
            app.help_max_scroll.set(0);
            return;
        }
        _ => {}
    }

    match app.active_tab {
        0 => match code {
            KeyCode::Down | KeyCode::Char('j') => {
                if !app.manual_rows.is_empty() {
                    app.selected_manual = (app.selected_manual + 1) % app.manual_rows.len();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if !app.manual_rows.is_empty() {
                    app.selected_manual =
                        (app.selected_manual + app.manual_rows.len() - 1) % app.manual_rows.len();
                }
            }
            KeyCode::Enter => app.open_selected_manual_in_search(),
            KeyCode::Char('a') => app.toggle_show_all_manuals(),
            KeyCode::Char('r') => app.refresh_or_register_selected_manual(),
            KeyCode::Char('d') => app.request_remove_selected_manual(false),
            KeyCode::Char('p') => app.request_remove_selected_manual(true),
            KeyCode::Char('h') => app.open_hint_popup(),
            KeyCode::Char('i') => {
                app.active_tab = 2;
                app.import_editing = true;
            }
            _ => {}
        },
        1 => match code {
            KeyCode::Esc | KeyCode::Backspace => {
                if app.came_from_manuals {
                    // Entered via Enter on a manual: go straight back to that manual's row.
                    app.came_from_manuals = false;
                    app.search_query.clear();
                    app.run_live_search();
                    app.active_tab = 0;
                    app.status_message = "Returned to Tab 1 (Manuals).".into();
                } else if !app.search_query.is_empty() {
                    app.search_query.clear();
                    app.run_live_search();
                    app.status_message = "Cleared search filter.".into();
                } else {
                    app.active_tab = 0;
                    app.status_message = "Returned to Tab 1 (Manuals).".into();
                }
            }
            KeyCode::Char('/') | KeyCode::Char('e') => {
                app.came_from_manuals = false;
                app.search_editing = true;
            }
            KeyCode::Char('x') => {
                app.came_from_manuals = false;
                app.search_query.clear();
                app.run_live_search();
            }
            KeyCode::Left | KeyCode::Char('h') => app.search_focus = SearchPane::Matches,
            KeyCode::Right | KeyCode::Char('l') => app.search_focus = SearchPane::Schema,
            KeyCode::Down | KeyCode::Char('j') => match app.search_focus {
                SearchPane::Matches => app.select_tool_delta(true),
                SearchPane::Schema => app.scroll_schema(1),
            },
            KeyCode::Up | KeyCode::Char('k') => match app.search_focus {
                // ↑ on the first match (or with no matches) goes back up into the search box.
                SearchPane::Matches if app.selected_tool == 0 || app.search_results.is_empty() => {
                    app.came_from_manuals = false;
                    app.search_editing = true;
                }
                SearchPane::Matches => app.select_tool_delta(false),
                SearchPane::Schema => app.scroll_schema(-1),
            },
            // Page keys always page the right pane, whichever pane has focus.
            KeyCode::PageDown | KeyCode::Char(']') => {
                let page = app.schema_page_rows();
                app.scroll_schema(page);
            }
            KeyCode::PageUp | KeyCode::Char('[') => {
                let page = app.schema_page_rows();
                app.scroll_schema(-page);
            }
            KeyCode::Char('d') => app.toggle_schema_view(),
            KeyCode::Char('t') => app.open_try_popup(),
            KeyCode::Home => app.schema_scroll = 0,
            KeyCode::End => app.schema_scroll = app.schema_max_scroll.get(),
            _ => {}
        },
        2 => match code {
            KeyCode::Char('e') | KeyCode::Enter => {
                app.import_editing = true;
            }
            KeyCode::Down | KeyCode::Char('j') => app.move_import_field(true),
            KeyCode::Up | KeyCode::Char('k') => app.move_import_field(false),
            KeyCode::Char('m') => app.toggle_import_mode(),
            KeyCode::Char('x') => {
                *app.import_form_mut() = ImportForm::default();
                app.import_field = ImportField::Source;
                app.status_message = match app.import_mode {
                    ImportMode::Spec => {
                        "Cleared the OpenAPI / UTCP form. Press «e» to type a Source."
                    }
                    ImportMode::Mcp => {
                        "Cleared the MCP server form. Press «e» to type a URL or command."
                    }
                }
                .into();
            }
            KeyCode::Char('c') => app.request_import(false),
            KeyCode::Char('r') => app.request_import(true),
            _ => {}
        },
        3 => match code {
            KeyCode::Char('c') => app.toggle_cache_on_register(),
            KeyCode::Char('s') => app.sync_ides(),
            KeyCode::Char('w') => app.request_install_folder_skill(),
            _ => {}
        },
        4 => match code {
            KeyCode::Down | KeyCode::Char('j') => {
                if !app.activities.is_empty() {
                    app.selected_activity = (app.selected_activity + 1) % app.activities.len();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if !app.activities.is_empty() {
                    app.selected_activity =
                        (app.selected_activity + app.activities.len() - 1) % app.activities.len();
                }
            }
            KeyCode::Char('x') => {
                let _ = clear_activity_logs(&app.paths);
                app.reload_snapshots();
                app.status_message = "Cleared activity.jsonl and activity.jsonl.bak.".into();
            }
            KeyCode::Enter => app.open_activity_detail(),
            _ => {}
        },
        5 => {
            if let KeyCode::Char('m') = code {
                if let Ok(cleared) = clear_media_spool(&app.paths) {
                    app.reload_snapshots();
                    app.status_message = format!("Cleared {} spooled media file(s).", cleared);
                }
            }
        }
        _ => {}
    }
}

fn rect_contains(r: Rect, column: u16, row: u16) -> bool {
    column >= r.x && column < r.x + r.width && row >= r.y && row < r.y + r.height
}

fn handle_mouse(app: &mut AppState, mouse: MouseEvent) {
    if app.pending_confirm.is_some() || app.hint_popup.is_some() {
        // These popups only answer to the keyboard.
        return;
    }
    let wheel_down = match mouse.kind {
        MouseEventKind::ScrollDown => true,
        MouseEventKind::ScrollUp => false,
        MouseEventKind::Down(MouseButton::Left) => {
            // A left click on a Tab 2 pane focuses it.
            if app.active_tab == 1 && !app.search_editing && app.try_popup.is_none() {
                if rect_contains(app.matches_area.get(), mouse.column, mouse.row) {
                    app.search_focus = SearchPane::Matches;
                } else if rect_contains(app.schema_area.get(), mouse.column, mouse.row) {
                    app.search_focus = SearchPane::Schema;
                }
            }
            return;
        }
        _ => return,
    };

    if app.activity_detail.is_some() {
        let rows = i32::from(WHEEL_SCROLL_ROWS);
        app.scroll_activity_detail(if wheel_down { rows } else { -rows });
        return;
    }
    if app.help_open {
        let rows = i32::from(WHEEL_SCROLL_ROWS);
        app.scroll_help(if wheel_down { rows } else { -rows });
        return;
    }
    if app.try_popup.is_some() {
        let rows = i32::from(WHEEL_SCROLL_ROWS);
        app.scroll_try(if wheel_down { rows } else { -rows });
        return;
    }

    match app.active_tab {
        1 if !app.search_editing => {
            // Route by pointer position first, so the wheel scrolls whatever is under it.
            let pane = if rect_contains(app.schema_area.get(), mouse.column, mouse.row) {
                SearchPane::Schema
            } else if rect_contains(app.matches_area.get(), mouse.column, mouse.row) {
                SearchPane::Matches
            } else {
                app.search_focus
            };
            match pane {
                SearchPane::Schema => {
                    let rows = i32::from(WHEEL_SCROLL_ROWS);
                    app.scroll_schema(if wheel_down { rows } else { -rows });
                }
                SearchPane::Matches => app.select_tool_delta(wheel_down),
            }
        }
        // List tabs: the wheel moves the selection like ↑/↓.
        0 | 4 => handle_key(
            app,
            if wheel_down {
                KeyCode::Down
            } else {
                KeyCode::Up
            },
        ),
        _ => {}
    }
}

/// Number of terminal rows `lines` occupy once word-wrapped to `width` columns, using the same
/// wrapping algorithm the renderer uses (so scroll limits match what is actually drawn).
fn wrapped_row_count(lines: &[Line<'_>], width: u16) -> usize {
    if width == 0 {
        return lines.len();
    }
    Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .line_count(width)
}

/// Colour of key hints (`Enter: Browse tools`, `[Shift+D]`, ...) so they stand out from labels.
const KEY_HINT: Color = Color::LightBlue;

/// Splits a hint string into spans: each `«key: action»` segment is drawn in [`KEY_HINT`]
/// (markers removed), everything else in `base`. An unmatched `«` is kept as plain text.
fn hint_spans(text: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('«') {
        let inner = &rest[start + '«'.len_utf8()..];
        let Some(end) = inner.find('»') else {
            break;
        };
        if start > 0 {
            spans.push(Span::styled(rest[..start].to_string(), base));
        }
        spans.push(Span::styled(inner[..end].to_string(), base.fg(KEY_HINT)));
        rest = &inner[end + '»'.len_utf8()..];
    }
    if !rest.is_empty() {
        spans.push(Span::styled(rest.to_string(), base));
    }
    spans
}

fn hint_line(text: &str) -> Line<'static> {
    Line::from(hint_spans(text, Style::default()))
}

/// Chrome for a text box that is receiving typed keys: a thick yellow border and a black-on-yellow
/// TYPING badge, so it is obvious that letters go into the box until Esc/Enter.
fn typing_block(title: String) -> Block<'static> {
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(
            " TYPING ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    spans.extend(hint_spans(&title, Style::default().fg(Color::Yellow)));
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Thick)
        .border_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .title(Line::from(spans))
}

/// Border style/type and title marker for a Tab 2 pane, depending on whether it has focus.
fn pane_chrome(focused: bool) -> (Style, BorderType, &'static str) {
    if focused {
        (
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            BorderType::Thick,
            "● ",
        )
    } else {
        (Style::default(), BorderType::Plain, "")
    }
}

fn draw_ui(f: &mut Frame, app: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(3),
        ])
        .split(f.area());

    let mode_span = if let Some(info) = &app.daemon_info {
        Span::styled(
            format!(
                " DAEMON RUNNING (PID {}, {} reqs, {} manual env) ",
                info.pid, info.requests_served, info.env_var_count
            ),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            " STANDALONE DIRECT MODE ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    };

    let log_span = if app.logging_on {
        Span::styled(
            " LOG: ON ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            " LOG: OFF ",
            Style::default().fg(Color::White).bg(Color::DarkGray),
        )
    };

    let mut header_spans = vec![
        Span::styled(
            " utcp ui ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        mode_span,
        Span::raw(" "),
        log_span,
    ];
    header_spans.extend(hint_spans(
        &format!(
            "  Manuals: {} | Tools: {}  [«D: Daemon» | «R: Restart» | «E: Sync Env» | «L: Log» | «?: Help»]",
            app.index.manuals.len(),
            app.index.tools.len()
        ),
        Style::default(),
    ));
    let header =
        Paragraph::new(Line::from(header_spans)).block(Block::default().borders(Borders::ALL));
    f.render_widget(header, chunks[0]);

    let tab_lines: Vec<Line> = TAB_TITLES.iter().map(|t| Line::from(*t)).collect();
    let tabs = Tabs::new(tab_lines)
        .select(app.active_tab)
        .block(Block::default().borders(Borders::ALL).title(" Navigation "))
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    f.render_widget(tabs, chunks[1]);

    match app.active_tab {
        0 => draw_manuals_tab(f, app, chunks[2]),
        1 => draw_search_tab(f, app, chunks[2]),
        2 => draw_import_tab(f, app, chunks[2]),
        3 => draw_settings_tab(f, app, chunks[2]),
        4 => draw_activity_tab(f, app, chunks[2]),
        5 => draw_daemon_monitor_tab(f, app, chunks[2]),
        _ => {}
    }

    let footer = Paragraph::new(hint_line(&app.status_message))
        .block(Block::default().borders(Borders::ALL).title(" Status "));
    f.render_widget(footer, chunks[3]);

    if let Some(popup) = &app.try_popup {
        draw_try_popup(f, app, popup);
    }
    if let Some(popup) = &app.hint_popup {
        draw_hint_popup(f, popup);
    }
    if let Some(entry) = &app.activity_detail {
        draw_activity_detail_popup(f, app, entry);
    }
    if app.help_open {
        draw_help_popup(f, app);
    }
    if let Some(action) = &app.pending_confirm {
        draw_confirm_popup(f, action, app.confirm_yes);
    }
}

/// Formats an RFC 3339 activity timestamp in `tz` as `(table, popup)` text: the table shows
/// `HH:MM:SS` for `today` and `MM-DD HH:MM:SS` for other days; the popup always shows the full
/// `YYYY-MM-DD HH:MM:SS`. A timestamp that does not parse is shown unchanged.
pub(crate) fn local_activity_time<Tz: chrono::TimeZone>(
    timestamp: &str,
    tz: &Tz,
    today: chrono::NaiveDate,
) -> (String, String)
where
    Tz::Offset: std::fmt::Display,
{
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(timestamp) else {
        return (timestamp.to_string(), timestamp.to_string());
    };
    let local = parsed.with_timezone(tz);
    let table = if local.date_naive() == today {
        local.format("%H:%M:%S").to_string()
    } else {
        local.format("%m-%d %H:%M:%S").to_string()
    };
    (table, local.format("%Y-%m-%d %H:%M:%S").to_string())
}

fn activity_detail_lines(entry: &ActivityEntry) -> Vec<Line<'static>> {
    let label = Style::default().fg(Color::Yellow);
    let field = |name: &str, value: String| {
        Line::from(vec![
            Span::styled(format!("{name:<9}: "), label),
            Span::raw(value),
        ])
    };
    let status_color = if entry.ok { Color::Green } else { Color::Red };
    let (_, local_time) = local_activity_time(
        &entry.timestamp,
        &chrono::Local,
        chrono::Local::now().date_naive(),
    );
    let mut lines = vec![
        field("Time", local_time),
        field("Caller", entry.caller.clone()),
        field("Mode", entry.mode.clone()),
        field("Command", entry.command.clone()),
        field("Target", entry.target.clone()),
        Line::from(vec![
            Span::styled(format!("{:<9}: ", "Status"), label),
            Span::styled(
                entry.status.clone(),
                Style::default()
                    .fg(status_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        field("Latency", format!("{:.2} ms", entry.latency_ms)),
        Line::from(""),
    ];
    match entry.detail.as_deref() {
        Some(detail) if !detail.trim().is_empty() => {
            lines.push(section_header("Error", Color::Red));
            lines.extend(detail.lines().map(|l| Line::from(l.to_string())));
        }
        _ => lines.push(Line::styled(
            "No details recorded (only failed calls keep details; older entries have none).",
            Style::default().fg(Color::DarkGray),
        )),
    }
    lines
}

fn draw_activity_detail_popup(f: &mut Frame, app: &AppState, entry: &ActivityEntry) {
    let full = f.area();
    let area = centered_fixed_height_rect(full.width * 8 / 10, full.height * 7 / 10, full);
    f.render_widget(Clear, area);

    let lines = activity_detail_lines(entry);
    let inner_width = area.width.saturating_sub(2);
    let view_rows = area.height.saturating_sub(2);
    let total_rows = wrapped_row_count(&lines, inner_width);
    let max_scroll =
        u16::try_from(total_rows.saturating_sub(view_rows as usize)).unwrap_or(u16::MAX);
    let scroll = app.activity_detail_scroll.min(max_scroll);
    app.activity_detail_max_scroll.set(max_scroll);
    app.activity_detail_view_rows.set(view_rows);

    let popup = Paragraph::new(lines)
        .scroll((scroll, 0))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Thick)
                .border_style(Style::default().fg(Color::Yellow))
                .title(hint_line(&format!(
                    " Activity details (Line {}/{}) [«↑/↓/wheel, PgUp/PgDn, Home/End: scroll» | «Esc: close»] ",
                    scroll + 1,
                    total_rows.max(1)
                ))),
        );
    f.render_widget(popup, area);

    if max_scroll > 0 {
        let mut sb_state = ScrollbarState::new(max_scroll as usize + 1)
            .position(scroll as usize)
            .viewport_content_length(view_rows as usize);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
        f.render_stateful_widget(scrollbar, area, &mut sb_state);
    }
}

/// The Try popup's lower half: what `d` / Enter do, the run in progress, or its output.
fn try_output_lines(output: &TryOutput) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let (header, body) = match output {
        TryOutput::Idle => {
            return vec![
                hint_line("«d»: dry run. Shows the request that would be sent (secrets hidden); nothing runs."),
                hint_line("«Enter»: call the tool for real and show its result here."),
                hint_line("«e»: edit the arguments first. Calls show on Tab 5 while the log is on («L»)."),
            ];
        }
        TryOutput::Running { dry_run: true } => {
            return vec![Line::styled("Preparing the dry run...", dim)]
        }
        TryOutput::Running { dry_run: false } => {
            return vec![Line::styled("Calling the tool...", dim)]
        }
        TryOutput::DryRun(text) => (
            section_header(
                "Dry run: the request that would be sent (secrets hidden, nothing ran)",
                Color::Cyan,
            ),
            text,
        ),
        TryOutput::Result(text) => (section_header("Result", Color::Green), text),
        TryOutput::Error(text) => (section_header("Error", Color::Red), text),
    };
    let mut lines = vec![header];
    let total = body.lines().count();
    lines.extend(
        body.lines()
            .take(TRY_OUTPUT_MAX_LINES)
            .map(|l| Line::from(l.to_string())),
    );
    if total > TRY_OUTPUT_MAX_LINES {
        lines.push(Line::styled(
            format!(
                "... {} more lines not shown; run utcp call in a shell for the full output.",
                total - TRY_OUTPUT_MAX_LINES
            ),
            dim,
        ));
    }
    lines
}

fn draw_try_popup(f: &mut Frame, app: &AppState, popup: &TryPopup) {
    let full = f.area();
    let area = centered_fixed_height_rect(
        (full.width * 85 / 100).max(60),
        (full.height * 8 / 10).max(14),
        full,
    );
    f.render_widget(Clear, area);

    let title = if popup.editing {
        format!(" Try {} ", popup.tool)
    } else {
        format!(
            " Try {} [«e: edit args» | «d: dry run» | «Enter: call» | «Esc: close»] ",
            popup.tool
        )
    };
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Thick)
        .border_style(Style::default().fg(Color::Cyan))
        .title(hint_line(&title));
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(3)])
        .split(inner);

    let args_block = if popup.editing {
        typing_block(" Arguments (JSON): «Esc/Enter: done» | «←/→, Home/End: move» ".into())
    } else {
        Block::default()
            .borders(Borders::ALL)
            .title(hint_line(" Arguments (JSON) [«e: edit»] "))
    };
    let mut args_text = popup.args.clone();
    if popup.editing {
        args_text.insert(char_byte_index(&args_text, popup.cursor), '▏');
    }
    f.render_widget(
        Paragraph::new(args_text)
            .wrap(Wrap { trim: false })
            .block(args_block),
        parts[0],
    );

    let lines = try_output_lines(&popup.output);
    let out_area = parts[1];
    let inner_width = out_area.width.saturating_sub(2);
    let view_rows = out_area.height.saturating_sub(2);
    let total_rows = wrapped_row_count(&lines, inner_width);
    let max_scroll =
        u16::try_from(total_rows.saturating_sub(view_rows as usize)).unwrap_or(u16::MAX);
    let scroll = popup.scroll.min(max_scroll);
    app.try_max_scroll.set(max_scroll);
    app.try_view_rows.set(view_rows);

    let output = Paragraph::new(lines)
        .scroll((scroll, 0))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(hint_line(&format!(
                    " Output (Line {}/{}) [«↑/↓/wheel, PgUp/PgDn, Home/End: scroll»] ",
                    scroll + 1,
                    total_rows.max(1)
                ))),
        );
    f.render_widget(output, out_area);

    if max_scroll > 0 {
        let mut sb_state = ScrollbarState::new(max_scroll as usize + 1)
            .position(scroll as usize)
            .viewport_content_length(view_rows as usize);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
        f.render_stateful_widget(scrollbar, out_area, &mut sb_state);
    }
}

/// The `?` popup text: the current tab first, then the keys that work everywhere, then the
/// other tabs. Tab 3 also explains what importing (and MCP) means.
fn help_lines(active_tab: usize, paths: &UtcpPaths) -> Vec<Line<'static>> {
    let heading = |s: &str| {
        Line::from(Span::styled(
            s.to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ))
    };
    let key = |k: &str, what: &str| {
        Line::from(vec![
            Span::styled(
                format!("  {:<14}", k),
                Style::default().fg(KEY_HINT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(what.to_string()),
        ])
    };
    let text = |s: &str| Line::from(format!("  {}", s));

    let tab_sections: Vec<(&str, Vec<Line<'static>>)> = vec![
        (
            "Tab 1: Manuals (each manual is one API or server; its tools are what agents call)",
            vec![
                key("j/k, ↑/↓", "select a manual"),
                key("Enter", "open its tools on Tab 2"),
                key("a", "also show manuals that are saved but not registered"),
                key("r", "refresh from its source (registered) or register it again (saved)"),
                key("d", "deregister (keeps a saved copy; r brings it back)"),
                key("p", "purge: delete the entry and the saved copy (asks first)"),
                key("h", "edit its hint: a few words agents see next to its name in the skill"),
                key("i", "go to Import and start typing"),
            ],
        ),
        (
            "Tab 2: Search",
            vec![
                key("/ or e", "type a search; '<manual>.' lists one manual"),
                key(" while TYPING", "Enter, Esc or ↓/← go to the tool list; → goes to Schema & Docs"),
                key("x", "clear the search"),
                key("h/l, ←/→", "focus the tool list or the Schema & Docs pane"),
                key("j/k, ↑/↓", "move in the list, or scroll the right pane"),
                key(" ↑ on 1st row", "go back up to the search box (TYPING)"),
                key("PgUp/PgDn [ ]", "page the right pane; Home/End jump"),
                key("d", "switch the right pane between Schema and Docs"),
                key("t", "try the selected tool in a popup (JSON arguments, prefilled)"),
                key(" in Try", "e edit args, d dry run (nothing is sent), Enter call, Esc close"),
                key("Esc", "clear the search, or go back to Tab 1"),
            ],
        ),
        (
            "Tab 3: Import (add a new API or MCP server)",
            vec![
                key("e or Enter", "type in the highlighted field (TYPING badge shows)"),
                key(" while TYPING", "Tab/↓ next field, Shift+Tab/↑ previous, Enter or Esc done"),
                key("j/k, ↑/↓", "pick a field"),
                key("m", "switch between OpenAPI / UTCP spec and MCP server"),
                key("c", "import and save to a file only (nothing is registered)"),
                key("r", "import and register: the tools appear on Tab 1, Tab 2 and in utcp search"),
                key("x", "clear the form"),
                Line::from(""),
                text("What importing does: utcp reads a description of an API once and saves it as a"),
                text("manual, a list of tools with their inputs. Agents then find tools with"),
                text("utcp search, read one with utcp schema, and run it with utcp call."),
                Line::from(""),
                text("OpenAPI / UTCP spec: Source is a URL or file of an OpenAPI 2.0/3.x spec (JSON or"),
                text("YAML) or a UTCP manual. Each API operation becomes a tool."),
                text("  Example: https://petstore3.swagger.io/api/v3/openapi.json -> manual 'petstore3'"),
                Line::from(""),
                text("MCP server: MCP (Model Context Protocol) is how many services offer tools to AI"),
                text("apps. A server is either a URL, or a program your computer starts. utcp asks it"),
                text("once for its tool list and saves that; later calls go to the server (a command is"),
                text("started for the call). Your agents and IDEs then need no MCP setup of their own."),
                text("  URL:     https://mcp.context7.com/mcp   (library docs, no key needed)"),
                text("  Command: npx -y @modelcontextprotocol/server-everything   (a demo server)"),
                text("  A command is split on spaces: the first word is the program, the rest are its"),
                text("  arguments. Quotes are not supported; use the CLI for arguments with spaces."),
                Line::from(""),
                text("Name: the prefix of every tool (context7.query-docs). Empty = a guess from the"),
                text("source, shown in the field title."),
                text("Hint: a few words on what the manual is for (library and framework docs). Agents"),
                text("see it next to the name in the skill; Tab 1 h changes it later. Used by r only."),
                text("Env / Headers: ;-separated. Commands get env vars (API_KEY=${API_KEY}); URLs get"),
                text("headers (Authorization: Bearer ${TOKEN}). Write ${VAR} and put the real value in"),
                text(&format!(
                    "{} (or export it and press E), so no secret lands in the manual.",
                    short_path(&paths.env_file)
                )),
                text(&format!(
                    "Output file: where c saves (default {});",
                    short_path(&paths.cache_dir.join("imports").join("<name>.utcp.json"))
                )),
                text(&format!(
                    "with r it is an extra copy. Registered manuals live in {}.",
                    short_path(&paths.manuals_dir)
                )),
                Line::from(""),
                text("Check the result: Tab 1 lists the manual; Tab 2 with '<name>.' shows its tools."),
                text("Same from a shell (what an agent runs):"),
                text("  utcp import-mcp context7 --url https://mcp.context7.com/mcp --register \\"),
                text("      --hint 'library and framework docs'"),
                text("  utcp import-mcp everything --command npx --arg -y \\"),
                text("      --arg @modelcontextprotocol/server-everything --register"),
                text("  utcp register petstore3 https://petstore3.swagger.io/api/v3/openapi.json"),
            ],
        ),
        (
            "Tab 4: Settings & Skills",
            vec![
                key("c", "toggle cache-on-register (keep a local copy of each manual)"),
                key("s", "install or update the utcp-tools and utcp-admin skills for every agent found"),
                key("w", "add the skills to the folder utcp ui was started from (asks first)"),
            ],
        ),
        (
            "Tab 5: Live Activity (needs L: log on)",
            vec![
                key("j/k, ↑/↓", "select an entry"),
                key("Enter", "show its details (errors, arguments; secrets redacted)"),
                key("x", "clear the activity log"),
            ],
        ),
        (
            "Tab 6: Daemon Monitor",
            vec![key("m", "delete the spooled media files (images/audio from tool results)")],
        ),
    ];

    let mut lines = Vec::new();
    let current = active_tab.min(tab_sections.len() - 1);
    let (title, body) = &tab_sections[current];
    lines.push(heading(&format!("This tab: {}", title)));
    lines.extend(body.iter().cloned());
    lines.push(Line::from(""));
    lines.push(heading("Everywhere"));
    lines.extend([
        key("Tab, 1-6", "switch tabs (Shift+Tab goes back)"),
        key("D", "start or stop the background daemon (faster calls)"),
        key("R", "restart the daemon"),
        key(
            "E",
            "send this shell's env vars that manuals use to the daemon",
        ),
        key("L", "turn the activity log on or off"),
        key("?", "this help (Esc, ? or q closes; j/k, PgUp/PgDn scroll)"),
        key("q", "quit"),
    ]);
    for (i, (title, body)) in tab_sections.iter().enumerate() {
        if i == current {
            continue;
        }
        lines.push(Line::from(""));
        lines.push(heading(title));
        lines.extend(body.iter().cloned());
    }
    lines
}

fn draw_help_popup(f: &mut Frame, app: &AppState) {
    let full = f.area();
    let area = centered_fixed_height_rect(
        (full.width * 9 / 10).max(60),
        full.height.saturating_sub(4),
        full,
    );
    f.render_widget(Clear, area);

    let lines = help_lines(app.active_tab, &app.paths);
    let inner_width = area.width.saturating_sub(2);
    let view_rows = area.height.saturating_sub(2);
    let total_rows = wrapped_row_count(&lines, inner_width);
    let max_scroll =
        u16::try_from(total_rows.saturating_sub(view_rows as usize)).unwrap_or(u16::MAX);
    let scroll = app.help_scroll.min(max_scroll);
    app.help_max_scroll.set(max_scroll);
    app.help_view_rows.set(view_rows);

    let popup = Paragraph::new(lines)
        .scroll((scroll, 0))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Thick)
                .border_style(Style::default().fg(Color::Cyan))
                .title(hint_line(&format!(
                    " Help (line {}/{}) [«j/k, PgUp/PgDn, Home/End: scroll» | «Esc or ?: close»] ",
                    scroll + 1,
                    total_rows.max(1)
                ))),
        );
    f.render_widget(popup, area);

    if max_scroll > 0 {
        let mut sb_state = ScrollbarState::new(max_scroll as usize + 1)
            .position(scroll as usize)
            .viewport_content_length(view_rows as usize);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
        f.render_stateful_widget(scrollbar, area, &mut sb_state);
    }
}

/// A `width` x `height` rect centered in `area`, shrunk to fit when `area` is smaller.
fn centered_fixed_height_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn draw_confirm_popup(f: &mut Frame, action: &ConfirmAction, yes_selected: bool) {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let (title, question, details): (&str, Line, Vec<String>) = match action {
        ConfirmAction::Deregister(name) => (
            " Deregister manual? ",
            Line::from(vec![
                Span::raw("Deregister "),
                Span::styled(format!("'{}'", name), bold),
                Span::raw("?"),
            ]),
            vec![
                "Its tools leave search and calls. Its settings and saved copy are kept:".into(),
                "it stays listed under All manuals (a) and r registers it again.".into(),
            ],
        ),
        ConfirmAction::Purge(name) => (
            " Purge manual? ",
            Line::from(vec![
                Span::raw("Purge "),
                Span::styled(format!("'{}'", name), bold),
                Span::raw(" for good?"),
            ]),
            vec![
                "This deletes its registry entry and its saved copy.".into(),
                "Adding it back needs the original source (Import tab or utcp register).".into(),
            ],
        ),
        ConfirmAction::InstallFolderSkill(dir) => (
            " Add skills to this folder? ",
            Line::from(vec![
                Span::raw("Add the utcp-tools and utcp-admin skills to "),
                Span::styled(dir.display().to_string(), bold),
                Span::raw("?"),
            ]),
            vec![
                "This writes .agents/skills/utcp-tools/ and utcp-admin/ here. Agents that open".into(),
                "this folder as a project (Zed, Copilot CLI, agy) load them. Delete both folders to undo."
                    .into(),
            ],
        ),
    };
    let border = match action {
        ConfirmAction::Deregister(_) => Color::Yellow,
        ConfirmAction::Purge(_) => Color::Red,
        ConfirmAction::InstallFolderSkill(_) => Color::Cyan,
    };

    let button = |label: &'static str, color: Color, selected: bool| {
        let style = Style::default().fg(color).add_modifier(Modifier::BOLD);
        Span::styled(
            label,
            if selected {
                style.add_modifier(Modifier::REVERSED)
            } else {
                style
            },
        )
    };
    let mut lines = vec![question, Line::from("")];
    lines.extend(details.into_iter().map(Line::from));
    lines.push(Line::from(""));
    let mut button_row = vec![
        Span::raw("   "),
        button(" Yes (y) ", Color::Green, yes_selected),
        Span::raw("   "),
        button(" No (n) ", Color::Red, !yes_selected),
    ];
    button_row.extend(hint_spans(
        "     «←/→: choose», «Enter: confirm», «Esc: cancel»",
        Style::default().fg(Color::Gray),
    ));
    lines.push(Line::from(button_row));

    // Widen for long lines (a deep folder path) and size the height from the *wrapped* row count,
    // so the Yes/No row is never pushed below the popup's bottom border.
    let screen = f.area();
    let longest = lines.iter().map(|l| l.width()).max().unwrap_or(0) as u16;
    let width = longest.saturating_add(4).clamp(80, screen.width.max(80));
    let inner_width = width.min(screen.width).saturating_sub(2);
    let height = wrapped_row_count(&lines, inner_width) as u16 + 2;
    let area = centered_fixed_height_rect(width, height, screen);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Thick)
                .border_style(Style::default().fg(border).add_modifier(Modifier::BOLD))
                .title(title),
        ),
        area,
    );
}

fn row_highlight_style() -> Style {
    Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
}

fn draw_manuals_tab(f: &mut Frame, app: &AppState, area: Rect) {
    let header = Row::new(vec![
        "Name",
        "Protocols",
        "Mode",
        "Tools",
        "Size",
        "SHA-256",
        "Credentials (env / .env)",
        "State",
    ])
    .style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );

    let rows: Vec<Row> = app
        .manual_rows
        .iter()
        .map(|row| {
            let m = &row.info;
            let cred_text = if m.required_env_vars.is_empty() {
                ("NONE".to_string(), None)
            } else {
                let missing: Vec<&str> = m
                    .required_env_vars
                    .iter()
                    .filter(|v| resolve_secret_var(&m.name, v, &app.env_map).is_none())
                    .map(String::as_str)
                    .collect();
                if missing.is_empty() {
                    ("SET".to_string(), Some(Color::Green))
                } else {
                    (format!("MISSING: {}", missing.join(",")), Some(Color::Red))
                }
            };

            let (state_cell, name_cell, cred_cell) = if row.registered {
                (
                    Cell::from("● registered").style(Style::default().fg(Color::Green)),
                    Cell::from(m.name.clone()).style(Style::default().fg(Color::Cyan)),
                    match cred_text.1 {
                        Some(c) => Cell::from(cred_text.0).style(Style::default().fg(c)),
                        None => Cell::from(cred_text.0),
                    },
                )
            } else {
                // Unregistered rows stay uncolored so the dim/italic row style marks them.
                let label = if !row.restorable {
                    "○ copy missing"
                } else if row.settings_kept {
                    "○ deregistered"
                } else {
                    "○ saved file"
                };
                (
                    Cell::from(label),
                    Cell::from(m.name.clone()),
                    Cell::from(cred_text.0),
                )
            };

            let r = Row::new(vec![
                name_cell,
                Cell::from(m.protocols.join(",")),
                Cell::from(if m.cached { "Cached Disk" } else { "Live" }),
                Cell::from(m.tool_count.to_string()),
                Cell::from(format!("{:.1} KB", m.file_size_bytes as f64 / 1024.0)),
                Cell::from(m.sha256.chars().take(10).collect::<String>()),
                cred_cell,
                state_cell,
            ]);
            if row.registered {
                r
            } else {
                r.style(
                    Style::default()
                        .fg(Color::Gray)
                        .add_modifier(Modifier::DIM | Modifier::ITALIC),
                )
            }
        })
        .collect();

    let mut mstate = TableState::default();
    if !app.manual_rows.is_empty() {
        mstate.select(Some(app.selected_manual.min(app.manual_rows.len() - 1)));
    }

    let selected_registered = app.selected_manual_row().map(|r| r.registered);
    let keys = match selected_registered {
        Some(false) => "«r: Register» | «p: Purge»",
        _ => "«Enter: Browse tools» | «r: Refresh» | «d: Deregister» | «p: Purge»",
    };
    let title = if app.show_all_manuals {
        let registered = app.manual_rows.iter().filter(|r| r.registered).count();
        format!(
            " All Manuals ({} registered, {} not) [{} | «a: Registered only» | «i: Import»] ",
            registered,
            app.manual_rows.len() - registered,
            keys
        )
    } else {
        format!(
            " Registered Manuals [{} | «a: Show all» | «i: Import»] ",
            keys
        )
    };

    let table = Table::new(
        rows,
        [
            Constraint::Fill(16),
            Constraint::Fill(12),
            Constraint::Length(12),
            Constraint::Length(6),
            Constraint::Length(9),
            Constraint::Length(11),
            Constraint::Fill(26),
            Constraint::Length(15),
        ],
    )
    .header(header)
    .row_highlight_style(row_highlight_style())
    .highlight_symbol("▸ ")
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(hint_line(&title)),
    );
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(4), Constraint::Length(3)])
        .split(area);
    f.render_stateful_widget(table, parts[0], &mut mstate);

    let dim = Style::default().fg(Color::DarkGray);
    let line = match app.selected_manual_row() {
        None => Line::styled("No manual selected.", dim),
        Some(row) => {
            let name = &row.info.name;
            match (app.manual_hints.get(name), row.registered) {
                (Some(h), true) => Line::from(vec![
                    Span::raw("Agents' skill lists it as: "),
                    Span::styled(
                        manual_label(name, Some(h)),
                        Style::default().fg(Color::Cyan),
                    ),
                ]),
                (None, true) => hint_line(&format!(
                    "No hint: agents' skill shows just the name '{}'. Press «h» to add a few words on what it is for.",
                    name
                )),
                (Some(h), false) => Line::styled(
                    format!("Not in the skill while unregistered (hint kept: {}).", h),
                    dim,
                ),
                (None, false) => Line::styled("Not in the skill while unregistered.", dim),
            }
        }
    };
    f.render_widget(
        Paragraph::new(line).block(
            Block::default()
                .borders(Borders::ALL)
                .title(hint_line(" Skill hint [«h: edit»] ")),
        ),
        parts[1],
    );
}

fn draw_hint_popup(f: &mut Frame, popup: &HintPopup) {
    let full = f.area();
    let area = centered_fixed_height_rect((full.width * 8 / 10).max(60), 11, full);
    f.render_widget(Clear, area);

    let outer = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Thick)
        .border_style(Style::default().fg(Color::Cyan))
        .title(hint_line(&format!(
            " Hint for '{}' [«Enter: save» | «Esc: cancel»] ",
            popup.manual
        )));
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Min(2),
        ])
        .split(inner);

    f.render_widget(
        Paragraph::new(
            "A few words on what this manual is for. Agents see them next to its name in the \
             utcp-tools skill description. Empty text removes the hint.",
        )
        .wrap(Wrap { trim: false }),
        parts[0],
    );

    let mut text = popup.text.clone();
    text.insert(char_byte_index(&text, popup.cursor), '▏');
    f.render_widget(
        Paragraph::new(text).block(typing_block(
            " Hint: «←/→, Home/End: move» | «Bksp/Del: erase» ".into(),
        )),
        parts[1],
    );

    let count = popup.text.split_whitespace().collect::<Vec<_>>().join(" ");
    let count = count.chars().count();
    let mut lines = Vec::new();
    let preview = match &popup.error {
        Some(e) => Err(e.clone()),
        None => normalize_manual_hint(&popup.text).map_err(|e| format!("{:#}", e)),
    };
    match preview {
        Ok(hint) => lines.push(Line::from(vec![
            Span::raw("Agents will see: "),
            Span::styled(
                manual_label(&popup.manual, hint.as_deref()),
                Style::default().fg(Color::Cyan),
            ),
        ])),
        Err(e) => lines.push(Line::styled(e, Style::default().fg(Color::Red))),
    }
    let over = count > MAX_MANUAL_HINT_CHARS;
    lines.push(Line::styled(
        format!("{}/{} characters", count, MAX_MANUAL_HINT_CHARS),
        Style::default().fg(if over { Color::Red } else { Color::DarkGray }),
    ));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), parts[2]);
}

fn first_line_summary(desc: &str) -> String {
    desc.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

fn truncate_with_ellipsis(s: &str, max_chars: usize) -> String {
    if max_chars <= 3 {
        return s.chars().take(max_chars).collect();
    }
    let count = s.chars().count();
    if count <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars - 1).collect();
        out.push('…');
        out
    }
}

fn draw_search_tab(f: &mut Frame, app: &AppState, area: Rect) {
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(6)])
        .split(area);

    let search_title = if app.search_editing {
        format!(
            " Live Search: «Esc/Enter/↓: done, to Matches» | «→: to Schema» — {} us ",
            app.search_latency_us
        )
    } else if app.came_from_manuals {
        format!(
            " Manual Filter («Esc/Bksp: back to Manuals» | «/: new search» | «x: show all» | «t: try») — {} us ",
            app.search_latency_us
        )
    } else {
        format!(
            " Live Search («/: edit» | «x: clear» | «←/→: pane» | «d: Schema/Docs» | «t: try» | «Esc: back») — {} us ",
            app.search_latency_us
        )
    };
    let search_block = if app.search_editing {
        typing_block(search_title)
    } else {
        Block::default()
            .borders(Borders::ALL)
            .title(hint_line(&search_title))
    };
    let mut search_text = app.search_query.clone();
    if app.search_editing {
        search_text.push('▏');
    }
    let search_box = Paragraph::new(search_text).block(search_block);
    f.render_widget(search_box, split[0]);

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(44), Constraint::Percentage(56)])
        .split(split[1]);

    let summary_col_width = ((panes[0].width as usize).saturating_mul(44) / 100).saturating_sub(3);
    let rows: Vec<Row> = app
        .search_results
        .iter()
        .map(|t| {
            Row::new(vec![
                Cell::from(t.name.clone()).style(Style::default().fg(Color::Cyan)),
                Cell::from(t.protocol.clone()).style(Style::default().fg(Color::Yellow)),
                Cell::from(truncate_with_ellipsis(
                    &first_line_summary(&t.description),
                    summary_col_width,
                )),
            ])
        })
        .collect();

    let mut list_state = TableState::default();
    if !app.search_results.is_empty() {
        list_state.select(Some(app.selected_tool.min(app.search_results.len() - 1)));
    }

    let list_focused = app.search_focus == SearchPane::Matches && !app.search_editing;
    let (list_border, list_border_type, list_marker) = pane_chrome(list_focused);
    let list_title = format!(
        " {}Matches ({}){} ",
        list_marker,
        app.search_results.len(),
        if list_focused { " — focused" } else { "" }
    );
    let list = Table::new(
        rows,
        [
            Constraint::Percentage(44),
            Constraint::Percentage(12),
            Constraint::Percentage(44),
        ],
    )
    .header(Row::new(vec!["Tool", "Proto", "Summary"]).style(Style::default().fg(Color::Yellow)))
    .row_highlight_style(row_highlight_style())
    .highlight_symbol("▸ ")
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(list_border_type)
            .border_style(list_border)
            .title(list_title),
    );
    f.render_stateful_widget(list, panes[0], &mut list_state);
    app.matches_area.set(panes[0]);

    let preview_lines = build_preview_lines(app.selected_schema.as_ref(), app.schema_view);

    // Scroll limits are measured in *wrapped* rows: long description lines wrap onto several
    // terminal rows, so counting logical lines would stop scrolling well before the end.
    let inner_width = panes[1].width.saturating_sub(2);
    let view_rows = panes[1].height.saturating_sub(2);
    let total_rows = wrapped_row_count(&preview_lines, inner_width);
    let max_scroll =
        u16::try_from(total_rows.saturating_sub(view_rows as usize)).unwrap_or(u16::MAX);
    let clamped_scroll = app.schema_scroll.min(max_scroll);
    app.schema_max_scroll.set(max_scroll);
    app.schema_view_rows.set(view_rows);
    app.schema_area.set(panes[1]);

    let schema_focused = app.search_focus == SearchPane::Schema && !app.search_editing;
    let (schema_border, schema_border_type, schema_marker) = pane_chrome(schema_focused);
    let schema_hint = if schema_focused {
        "focused: «↑/↓/wheel, PgUp/PgDn, Home/End: scroll»"
    } else {
        "«→: focus»"
    };
    let (view_name, other_view) = match app.schema_view {
        SchemaView::Schema => ("Schema", "Docs"),
        SchemaView::Docs => ("Docs", "Schema"),
    };
    let preview = Paragraph::new(preview_lines)
        .scroll((clamped_scroll, 0))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(schema_border_type)
                .border_style(schema_border)
                .title(hint_line(&format!(
                    " {}{} [«d: {}»] (Line {}/{} — {}) ",
                    schema_marker,
                    view_name,
                    other_view,
                    clamped_scroll + 1,
                    total_rows.max(1),
                    schema_hint
                ))),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(preview, panes[1]);

    if max_scroll > 0 {
        let mut sb_state = ScrollbarState::new(max_scroll as usize + 1)
            .position(clamped_scroll as usize)
            .viewport_content_length(view_rows as usize);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
        f.render_stateful_widget(scrollbar, panes[1], &mut sb_state);
    }
}

fn section_header(title: &str, color: Color) -> Line<'static> {
    Line::from(Span::styled(
        format!("── {title} ──"),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    ))
}

fn push_pretty_json(lines: &mut Vec<Line<'static>>, value: &serde_json::Value) {
    if let Ok(pretty) = serde_json::to_string_pretty(value) {
        lines.extend(pretty.lines().map(|l| Line::from(l.to_string())));
    }
}

/// Short type label for a JSON Schema property: `string`, `integer[]`, `string | null`, `any`.
fn json_schema_type_label(prop: &serde_json::Value) -> String {
    match prop.get("type") {
        Some(serde_json::Value::String(t)) if t == "array" => {
            let item = prop
                .get("items")
                .map(json_schema_type_label)
                .unwrap_or_else(|| "any".into());
            format!("{item}[]")
        }
        Some(serde_json::Value::String(t)) => t.clone(),
        Some(serde_json::Value::Array(ts)) => ts
            .iter()
            .filter_map(|t| t.as_str())
            .collect::<Vec<_>>()
            .join(" | "),
        _ => "any".into(),
    }
}

/// Human-readable parameter list derived from a tool's input JSON Schema, for the Docs view.
fn parameter_doc_lines(inputs: &serde_json::Value) -> Vec<Line<'static>> {
    let Some(props) = inputs.get("properties").and_then(|p| p.as_object()) else {
        return vec![Line::from("(no parameters)")];
    };
    if props.is_empty() {
        return vec![Line::from("(no parameters)")];
    }
    let required: Vec<&str> = inputs
        .get("required")
        .and_then(|r| r.as_array())
        .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    // Required parameters first, then optional ones; each group keeps schema order.
    let mut ordered: Vec<(&String, &serde_json::Value)> = props.iter().collect();
    ordered.sort_by_key(|(name, _)| !required.contains(&name.as_str()));

    let mut lines = Vec::new();
    for (name, prop) in ordered {
        let is_required = required.contains(&name.as_str());
        let mut spans = vec![
            Span::raw("• "),
            Span::styled(
                name.clone(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" ({}", json_schema_type_label(prop))),
        ];
        if is_required {
            spans.push(Span::raw(", "));
            spans.push(Span::styled("required", Style::default().fg(Color::Yellow)));
        }
        spans.push(Span::raw(")"));
        if let Some(desc) = prop.get("description").and_then(|d| d.as_str()) {
            spans.push(Span::raw(format!(": {}", desc.trim())));
        }
        lines.push(Line::from(spans));

        let mut extras = Vec::new();
        if let Some(values) = prop.get("enum").and_then(|e| e.as_array()) {
            let shown: Vec<String> = values.iter().map(|v| v.to_string()).collect();
            extras.push(format!("one of: {}", shown.join(", ")));
        }
        if let Some(default) = prop.get("default") {
            extras.push(format!("default: {default}"));
        }
        if !extras.is_empty() {
            lines.push(Line::from(format!("    {}", extras.join(" | "))));
        }
    }
    lines
}

/// Content of the Tab 2 right pane. `Schema` shows the raw input/output JSON Schemas;
/// `Docs` shows the full description plus a readable parameter list.
fn build_preview_lines(schema: Option<&ToolSchemaDetail>, view: SchemaView) -> Vec<Line<'static>> {
    let Some(schema) = schema else {
        return vec![Line::from(
            "Select a tool on the left to inspect its schema ('d' toggles Schema / Docs).",
        )];
    };

    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!("{} ", schema.name),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("[{}]", schema.protocol),
            Style::default().fg(Color::Yellow),
        ),
    ])];

    match view {
        SchemaView::Schema => {
            let summary_line = first_line_summary(&schema.description);
            if !summary_line.is_empty() {
                lines.push(Line::from(summary_line));
            }
            lines.push(Line::from(""));
            lines.push(section_header("Input Schema (Parameters)", Color::Green));
            push_pretty_json(&mut lines, &schema.inputs);
            lines.push(Line::from(""));
            lines.push(section_header("Output Schema", Color::Cyan));
            push_pretty_json(&mut lines, &schema.outputs);
        }
        SchemaView::Docs => {
            lines.push(Line::from(""));
            lines.push(section_header("Description & Usage Notes", Color::Yellow));
            let description = schema.description.trim();
            if description.is_empty() {
                lines.push(Line::from("(this manual provides no description)"));
            } else {
                lines.extend(description.lines().map(|l| Line::from(l.to_string())));
            }
            lines.push(Line::from(""));
            lines.push(section_header("Parameters", Color::Green));
            lines.extend(parameter_doc_lines(&schema.inputs));
            if !schema.tags.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(format!("Tags: {}", schema.tags.join(", "))));
            }
        }
    }
    lines
}

fn draw_import_tab(f: &mut Frame, app: &AppState, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(4),
        ])
        .split(area);

    let mode = app.import_mode;
    let form = app.import_form();
    let picked = Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let mode_tab = |label: &'static str, on: bool| {
        if on {
            Span::styled(format!(" {} ", label), picked)
        } else {
            Span::raw(format!(" {} ", label))
        }
    };
    let mut mode_line = vec![
        Span::raw(" Import from: "),
        mode_tab("OpenAPI / UTCP spec", mode == ImportMode::Spec),
        Span::raw(" "),
        mode_tab("MCP server", mode == ImportMode::Mcp),
    ];
    mode_line.extend(hint_spans(
        "   «m: switch» | «?: explains»",
        Style::default().fg(Color::DarkGray),
    ));
    f.render_widget(Paragraph::new(Line::from(mode_line)), chunks[0]);

    let source = form.source.trim();
    let name_hint = inferred_import_name(mode, form);
    let shown_name = match form.name.trim() {
        "" => name_hint.clone().unwrap_or_else(|| "<name>".into()),
        typed => typed.to_string(),
    };
    let source_title = match mode {
        ImportMode::Spec => " Source: OpenAPI or UTCP URL, or a local file path ".to_string(),
        ImportMode::Mcp => {
            " MCP server: https:// URL, or the command that starts it (words split on spaces) "
                .to_string()
        }
    };
    let name_title = match &name_hint {
        Some(n) => format!(" Name (optional; empty = '{}') ", n),
        None => " Name (optional; inferred from the source) ".to_string(),
    };
    let auth_title = match mode {
        ImportMode::Spec => {
            " Headers (optional): Header: value; ...  e.g. Authorization: Bearer ${TOKEN} "
        }
        ImportMode::Mcp if is_http_url(source) => {
            " Headers (optional): Header: value; ...  e.g. Authorization: Bearer ${TOKEN} "
        }
        ImportMode::Mcp if !source.is_empty() => {
            " Env vars for the server (optional): KEY=value; ...  e.g. API_KEY=${API_KEY} "
        }
        ImportMode::Mcp => " Env vars (command) or Headers (URL), optional, ;-separated ",
    }
    .to_string();
    let output_title = format!(
        " Output file (optional; empty = {}) ",
        display_with_tilde(&app.default_import_output(&shown_name))
    );

    let titles = [
        source_title,
        name_title,
        " Hint (used by r): a few words for agents, e.g. library and framework docs ".to_string(),
        auth_title,
        output_title,
    ];
    for (i, field) in IMPORT_FIELDS.iter().enumerate() {
        let active = app.import_field == *field;
        let editing = active && app.import_editing;
        let inner_width = chunks[i + 1].width.saturating_sub(3) as usize;
        let mut text = tail_fit(form.field(*field), inner_width);
        if editing {
            text.push('▏');
        }
        let block = if editing {
            typing_block(titles[i].clone())
        } else {
            Block::default()
                .borders(Borders::ALL)
                .title(titles[i].clone())
                .border_style(if active {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default()
                })
        };
        f.render_widget(Paragraph::new(text).block(block), chunks[i + 1]);
    }

    let keys = if app.import_editing {
        "TYPING: letters go into the box | «Tab/↓: next field» | «Shift+Tab/↑: previous» | «Enter or Esc: done»"
    } else {
        "«e/Enter: edit» | «j/k: field» | «m: mode» | «c: save file only» | «r: import + register» | «x: clear» | «?: help»"
    };
    let dim = Style::default().fg(Color::DarkGray);
    let mut help = vec![Line::from(hint_spans(
        keys,
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    let env_file = short_path(&app.paths.env_file);
    match mode {
        ImportMode::Spec => help.extend([
            Line::from("Turns an OpenAPI 2.0/3.x spec (JSON/YAML) or a UTCP manual into tools: one per API operation."),
            Line::from(vec![Span::raw("Try: "), Span::styled("https://petstore3.swagger.io/api/v3/openapi.json", dim), Span::raw("  then r (name 'petstore3')")]),
            Line::from(format!("Secrets: write ${{VAR}} in Headers and keep the value in {}, never in the manual.", env_file)),
            Line::from("After r: Tab 1 lists it; Tab 2 with '<name>.' shows its tools; agents use utcp search / call."),
        ]),
        ImportMode::Mcp => help.extend([
            Line::from("Asks the server once for its tool list and saves it; utcp contacts the server again only for calls."),
            Line::from(vec![Span::raw("URL:     "), Span::styled("https://mcp.context7.com/mcp", dim), Span::raw("  (no key needed; r registers 'context7')")]),
            Line::from(vec![Span::raw("Command: "), Span::styled("npx -y @modelcontextprotocol/server-everything", dim), Span::raw("  (no quotes; one word = one argument)")]),
            Line::from(format!("Secrets: API_KEY=${{API_KEY}} (command) or Authorization: Bearer ${{TOKEN}} (URL); values in {}", env_file)),
        ]),
    }
    let help = Paragraph::new(help).wrap(Wrap { trim: false }).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" How to import "),
    );
    f.render_widget(help, chunks[6]);
}

fn draw_settings_tab(f: &mut Frame, app: &AppState, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),
            Constraint::Length(app.ide_statuses.len() as u16 + 3),
            Constraint::Min(9),
        ])
        .split(area);

    let settings_lines = vec![
        hint_line(&format!(
            "  «[Shift+D]» Local IPC Daemon      : {}",
            if let Some(d) = &app.daemon_info {
                format!("RUNNING (PID {}, {})", d.pid, d.socket_endpoint)
            } else {
                format!("STOPPED ({})", app.paths.socket_endpoint)
            }
        )),
        hint_line(&format!(
            "  «[c]»       cache_on_register     : {} ({})",
            app.settings.cache_on_register,
            app.paths.settings_file.display()
        )),
        hint_line(&format!(
            "  «[Shift+L]» record_activity_log   : {} ({})",
            app.logging_on,
            app.paths.logging_sentinel.display()
        )),
        Line::from(format!(
            "            Config Registry       : {}",
            app.paths.config_file.display()
        )),
    ];
    f.render_widget(
        Paragraph::new(settings_lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Settings & Runtime Toggles "),
        ),
        chunks[0],
    );

    let rows: Vec<Row> = app
        .ide_statuses
        .iter()
        .map(|s| {
            Row::new(vec![
                Cell::from(s.client.clone()),
                Cell::from(Line::from(skill_status_span(s))),
                Cell::from(s.path.clone()),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(20),
            Constraint::Length(15),
            Constraint::Fill(1),
        ],
    )
    .header(
        Row::new(vec!["Agent", "Status", "Skill File"]).style(Style::default().fg(Color::Yellow)),
    )
    .block(Block::default().borders(Borders::ALL).title(hint_line(
        " Agent Skills: Your Account («s: Sync / Repair») ",
    )));
    f.render_widget(table, chunks[1]);

    f.render_widget(
        Paragraph::new(folder_skill_lines(app.folder_skill.as_ref()))
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(hint_line(" This Folder: Project Skills («w: Add Here») ")),
            ),
        chunks[2],
    );
}

/// A path for display: `~/...` under the home folder on Unix, the full path elsewhere.
fn short_path(path: &Path) -> String {
    #[cfg(unix)]
    if let Some(home) = std::env::var_os("HOME") {
        if let Ok(rest) = path.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

fn skill_status_span(s: &IdeSkillTargetStatus) -> Span<'static> {
    if s.up_to_date {
        Span::styled("UP TO DATE", Style::default().fg(Color::Green))
    } else if s.exists {
        Span::styled("OUTDATED", Style::default().fg(Color::Yellow))
    } else {
        Span::styled("NOT INSTALLED", Style::default().fg(Color::Red))
    }
}

/// Tab 4 lower panel: what the start folder holds and what `w` / `s` would do there.
fn folder_skill_lines(folder: Option<&FolderSkillInfo>) -> Vec<Line<'static>> {
    let hint = Style::default().fg(Color::Gray);
    let Some(folder) = folder else {
        return vec![Line::styled(
            "  No current folder: utcp ui could not read its working directory.",
            hint,
        )];
    };
    let agents = if !folder.agents_dir_exists {
        Span::styled("none here", hint)
    } else if folder.other_skills.is_empty() {
        Span::raw("present, no other skills")
    } else {
        const SHOWN: usize = 6;
        let mut names = folder.other_skills[..folder.other_skills.len().min(SHOWN)].join(", ");
        if folder.other_skills.len() > SHOWN {
            names.push_str(", ...");
        }
        Span::raw(format!(
            "present, {} other skill(s): {}",
            folder.other_skills.len(),
            names
        ))
    };
    let next_step = if folder.target.up_to_date {
        "  Installed here. «s» keeps this copy current along with the account-wide ones."
    } else if folder.target.exists {
        "  «w» updates this copy (or press «s»)."
    } else {
        "  «w» adds the utcp skills here (asks first). After that, «s» keeps them current."
    };
    vec![
        Line::from(vec![
            Span::raw("  Folder     : "),
            Span::styled(
                folder.dir.display().to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled("   (where utcp ui was started)", hint),
        ]),
        Line::from(vec![Span::raw("  .agents/   : "), agents]),
        Line::from(vec![
            Span::raw("  utcp skills: "),
            skill_status_span(&folder.target),
            Span::styled(format!("   {}", folder.target.path), hint),
        ]),
        Line::from(""),
        Line::styled(
            "  Agents that open this folder as a project (Zed, Copilot CLI, agy) also load the skills in its",
            hint,
        ),
        Line::styled(
            "  .agents/skills/. To manage another repo, project, or workspace, start utcp ui there.",
            hint,
        ),
        Line::from(hint_spans(next_step, hint)),
    ]
}

fn draw_activity_tab(f: &mut Frame, app: &AppState, area: Rect) {
    let today = chrono::Local::now().date_naive();
    let rows: Vec<Row> = app
        .activities
        .iter()
        .map(|a| {
            let status_cell = if a.ok {
                Cell::from(a.status.clone()).style(Style::default().fg(Color::Green))
            } else {
                Cell::from(a.status.clone()).style(Style::default().fg(Color::Red))
            };
            let (time, _) = local_activity_time(&a.timestamp, &chrono::Local, today);
            Row::new(vec![
                Cell::from(time),
                Cell::from(a.mode.clone()),
                Cell::from(a.caller.clone()),
                Cell::from(a.command.clone()),
                Cell::from(a.target.clone()),
                status_cell,
                Cell::from(format!("{:.2} ms", a.latency_ms)),
            ])
        })
        .collect();

    let mut astate = TableState::default();
    if !app.activities.is_empty() {
        astate.select(Some(app.selected_activity.min(app.activities.len() - 1)));
    }

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(24),
            Constraint::Percentage(10),
            Constraint::Percentage(12),
            Constraint::Percentage(10),
            Constraint::Percentage(22),
            Constraint::Percentage(10),
            Constraint::Percentage(12),
        ],
    )
    .header(
        Row::new(vec![
            "Time",
            "Mode",
            "Caller",
            "Cmd",
            "Target",
            "Status",
            "Latency",
        ])
        .style(Style::default().fg(Color::Yellow)),
    )
    .row_highlight_style(row_highlight_style())
    .highlight_symbol("▸ ")
    .block(Block::default().borders(Borders::ALL).title(hint_line(
        " Live Activity (last 500, kept by the daemon) [«Enter: details» | «Shift+L: disk log» | «x: clear disk log»] ",
    )));
    f.render_stateful_widget(table, area, &mut astate);
}

fn draw_daemon_monitor_tab(f: &mut Frame, app: &AppState, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(10), Constraint::Min(6)])
        .split(area);

    let systemd_label = app
        .systemd_state_fallback
        .as_deref()
        .unwrap_or("not configured / non-systemd");

    let info_lines = if let Some(d) = &app.daemon_info {
        vec![
            Line::from(format!("  Daemon State        : RUNNING (PID {})", d.pid)),
            Line::from(format!("  IPC Endpoint        : {}", d.socket_endpoint)),
            Line::from(format!(
                "  Uptime & Traffic    : {} uptime | {} IPC requests served",
                crate::format_uptime(d.uptime_secs),
                d.requests_served
            )),
            Line::from(format!(
                "  Loaded in RAM       : {} manuals | {} tools | {} env vars used by manuals",
                d.manual_count, d.tool_count, d.env_var_count
            )),
            Line::from(format!(
                "  Secrets Command     : {}",
                match &d.secrets_command {
                    None => "not configured ([secrets] in settings.toml)".to_string(),
                    Some(s) => match &s.error {
                        Some(error) => format!("FAILED: {error}"),
                        None => format!("OK, printed {} of the wanted names", s.provided.len()),
                    },
                }
            )),
            Line::from(format!(
                "  systemd --user unit : utcp-daemon.service ({})",
                systemd_label
            )),
            Line::from(format!(
                "  tmpfs Media Spool   : {} ({}/{} files, {} bytes, TTL {}m)",
                app.media_spool.dir,
                app.media_spool.file_count,
                app.media_spool.max_files,
                app.media_spool.total_bytes,
                app.media_spool.ttl_mins
            )),
            hint_line(
                "  Controls            : «[Shift+D] Stop» | «[Shift+R] Restart» | «[Shift+E] Sync Current Shell Env -> Daemon» | «[m] Clear Media Spool»",
            ),
        ]
    } else {
        vec![
            Line::from("  Daemon State        : STOPPED (CLI operating in Standalone Direct Mode)"),
            Line::from(format!(
                "  IPC Endpoint        : {}",
                app.paths.socket_endpoint
            )),
            Line::from(format!(
                "  systemd --user unit : utcp-daemon.service ({})",
                systemd_label
            )),
            Line::from(format!(
                "  tmpfs Media Spool   : {} ({}/{} files, {} bytes, TTL {}m)",
                app.media_spool.dir,
                app.media_spool.file_count,
                app.media_spool.max_files,
                app.media_spool.total_bytes,
                app.media_spool.ttl_mins
            )),
            hint_line("  Controls            : «[Shift+D] Start Daemon» | «[m] Clear Media Spool»"),
        ]
    };

    f.render_widget(
        Paragraph::new(info_lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" `--user` Daemon & tmpfs Media Spool Monitor "),
        ),
        chunks[0],
    );

    let cred_rows: Vec<Row> = if let Some(d) = &app.daemon_info {
        d.credentials
            .iter()
            .map(|c| {
                let status_cell = if c.is_set {
                    Cell::from("SET IN DAEMON RAM").style(Style::default().fg(Color::Green))
                } else {
                    Cell::from("MISSING IN DAEMON").style(Style::default().fg(Color::Red))
                };
                Row::new(vec![
                    Cell::from(c.manual.clone()),
                    Cell::from(c.env_var.clone()),
                    status_cell,
                    Cell::from(c.resolved_via.clone().unwrap_or_else(|| "-".into())),
                    Cell::from(c.source.clone().unwrap_or_else(|| "-".into())),
                ])
            })
            .collect()
    } else {
        Vec::new()
    };

    let cred_title = if app.daemon_info.is_some() {
        " Daemon In-Memory Credential Status (Secret values hidden — Press «Shift+E» to push shell env) "
    } else {
        " Daemon Stopped (Start daemon with «Shift+D» to inspect live daemon RAM credentials) "
    };

    let cred_table = Table::new(
        cred_rows,
        [
            Constraint::Percentage(15),
            Constraint::Percentage(25),
            Constraint::Percentage(20),
            Constraint::Percentage(20),
            Constraint::Percentage(20),
        ],
    )
    .header(
        Row::new(vec![
            "Manual",
            "Required Var",
            "Daemon Status",
            "Resolved Via",
            "Source",
        ])
        .style(Style::default().fg(Color::Yellow)),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(hint_line(cred_title)),
    );
    f.render_widget(cred_table, chunks[1]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_line_summary_and_ellipsis_truncation_work() {
        let multiline = "\n  Resolves a package/product name.\n\nYou MUST call this function first.\nMore details.";
        assert_eq!(
            first_line_summary(multiline),
            "Resolves a package/product name."
        );
        assert_eq!(first_line_summary(""), "");
        assert_eq!(truncate_with_ellipsis("short", 10), "short");
        assert_eq!(truncate_with_ellipsis("abcdefghij", 6), "abcde…");
    }

    #[test]
    fn wrapped_row_count_matches_word_wrapping() {
        // Lines that fit count once each; empty lines still occupy a row.
        let fits = vec![Line::from("abc"), Line::from(""), Line::from("defgh")];
        assert_eq!(wrapped_row_count(&fits, 10), 3);

        // An unbroken 25-char token hard-wraps into ceil(25 / 10) rows.
        let long_token = vec![Line::from("x".repeat(25))];
        assert_eq!(wrapped_row_count(&long_token, 10), 3);

        // Word wrapping needs MORE rows than a naive ceil(chars / width) estimate (20/10 = 2),
        // because words are not split mid-word: "aaaaaa" | "bbbbbb" | "cccccc".
        let words = vec![Line::from("aaaaaa bbbbbb cccccc")];
        assert_eq!(wrapped_row_count(&words, 10), 3);

        // Zero width (pane collapsed) falls back to the logical line count instead of 0.
        assert_eq!(wrapped_row_count(&fits, 0), 3);
    }

    fn test_paths(root: &Path) -> UtcpPaths {
        let config_dir = root.join("config");
        let state_dir = root.join("state");
        UtcpPaths {
            config_file: config_dir.join(".utcp_config.json"),
            settings_file: config_dir.join("settings.toml"),
            env_file: config_dir.join(".env"),
            lock_file: config_dir.join(".utcp.lock"),
            manuals_dir: config_dir.join("manuals"),
            search_index_file: root.join("cache/index.json"),
            logging_sentinel: state_dir.join("logging.enabled"),
            activity_log_file: state_dir.join("activity.jsonl"),
            activity_log_bak: state_dir.join("activity.jsonl.bak"),
            socket_endpoint: root.join("utcp.sock").to_string_lossy().into_owned(),
            daemon_pid_file: state_dir.join("daemon.pid"),
            separate_setup: true,
            cache_dir: root.join("cache"),
            media_dir: root.join("media"),
            config_dir,
            state_dir,
        }
    }

    fn summary(name: &str) -> ToolSummary {
        ToolSummary {
            name: name.into(),
            manual: "demo".into(),
            protocol: "http".into(),
            description: "Demo tool.".into(),
            tags: Vec::new(),
            inputs: None,
            outputs: None,
            staged: false,
        }
    }

    /// A Tab 2 state with two matches and a schema whose description wraps heavily.
    fn search_tab_app(root: &Path) -> AppState {
        let mut app = AppState::unloaded(test_paths(root), None);
        app.active_tab = 1;
        app.search_results = vec![summary("demo.first"), summary("demo.second")];
        let long_para = "word ".repeat(60);
        app.selected_schema = Some(ToolSchemaDetail {
            name: "demo.first".into(),
            manual: "demo".into(),
            protocol: "http".into(),
            description: vec![long_para; 3].join("\n"),
            tags: Vec::new(),
            inputs: serde_json::json!({"type": "object"}),
            outputs: serde_json::json!({}),
        });
        app
    }

    fn draw_once(app: &AppState) {
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| draw_ui(f, app)).unwrap();
    }

    #[test]
    fn enter_on_manual_opens_its_tools_in_search_and_esc_returns() {
        fn manual(name: &str, tool_count: usize) -> utcp_core::index::IndexedManual {
            utcp_core::index::IndexedManual {
                name: name.into(),
                source_url: String::new(),
                cached: true,
                cached_file: None,
                sha256: String::new(),
                fetched_at: String::new(),
                protocols: vec!["http".into()],
                tool_count,
                file_size_bytes: 0,
                required_env_vars: Vec::new(),
                env_var_names: Vec::new(),
            }
        }
        fn tool(manual: &str, i: usize) -> utcp_core::index::IndexedTool {
            let qualified = format!("{manual}.tool_{i:02}");
            utcp_core::index::IndexedTool {
                search_blob: qualified.clone(),
                qualified_name: qualified,
                manual_name: manual.into(),
                tool_name: format!("tool_{i:02}"),
                protocol: "http".into(),
                description: "Demo tool.".into(),
                tags: Vec::new(),
                input_schema_json: "{}".into(),
                output_schema_json: "{}".into(),
            }
        }

        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = AppState::unloaded(test_paths(tmp.path()), None);
        // 'beta' has more tools than the old 50-result cap, so all of them must still be listed.
        app.index.manuals = vec![manual("alpha", 2), manual("beta", 60)];
        app.index.tools = (0..2).map(|i| tool("alpha", i)).collect();
        app.index.tools.extend((0..60).map(|i| tool("beta", i)));
        app.rebuild_manual_rows();

        handle_key(&mut app, KeyCode::Down);
        assert_eq!(app.selected_manual, 1);
        handle_key(&mut app, KeyCode::Enter);
        assert_eq!(
            app.active_tab, 1,
            "Enter goes straight to Tab 2, no intermediate popup"
        );
        assert_eq!(app.search_query, "beta.");
        assert_eq!(app.search_results.len(), 60);
        assert!(app.search_results.iter().all(|t| t.manual == "beta"));
        assert_eq!(app.search_focus, SearchPane::Matches);
        assert_eq!(app.selected_tool, 0);
        assert!(app.selected_schema.is_some());

        handle_key(&mut app, KeyCode::Esc);
        assert_eq!(app.active_tab, 0, "one Esc returns to Manuals");
        assert_eq!(app.selected_manual, 1, "the manual row stays selected");
        assert!(app.search_query.is_empty());
        assert!(!app.came_from_manuals);
    }

    fn manual_info(name: &str) -> utcp_core::index::IndexedManual {
        utcp_core::index::IndexedManual {
            name: name.into(),
            source_url: String::new(),
            cached: true,
            cached_file: None,
            sha256: String::new(),
            fetched_at: String::new(),
            protocols: vec!["http".into()],
            tool_count: 1,
            file_size_bytes: 0,
            required_env_vars: Vec::new(),
            env_var_names: Vec::new(),
        }
    }

    /// Tab 1 with registered `alpha` and `gamma` in the index, plus a deregistered `beta` in the
    /// config whose saved copy does not exist (so nothing here can reach the real `$HOME`).
    fn manuals_tab_app(root: &Path) -> AppState {
        let paths = test_paths(root);
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        let missing = root.join("no-such-copy.utcp.json");
        let config = serde_json::json!({
            "version": "1.0",
            "manuals": {},
            "deregistered": {
                "beta": {
                    "name": "beta",
                    "source_url": "https://example.invalid/beta.json",
                    "cached": true,
                    "cached_file": missing.to_string_lossy(),
                }
            }
        });
        std::fs::write(&paths.config_file, config.to_string()).unwrap();

        let mut app = AppState::unloaded(paths, None);
        app.index.manuals = vec![manual_info("alpha"), manual_info("gamma")];
        app.rebuild_manual_rows();
        app
    }

    fn row_names(app: &AppState) -> Vec<(String, bool)> {
        app.manual_rows
            .iter()
            .map(|r| (r.info.name.clone(), r.registered))
            .collect()
    }

    fn screen_text(app: &AppState) -> String {
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(110, 30)).unwrap();
        terminal.draw(|f| draw_ui(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn hint_spans_colour_only_the_marked_keys() {
        let base = Style::default().fg(Color::Gray);
        let spans = hint_spans(" Title [«r: Refresh» | «d: Drop»] tail « open", base);
        let parts: Vec<(&str, Option<Color>)> = spans
            .iter()
            .map(|s| (s.content.as_ref(), s.style.fg))
            .collect();
        assert_eq!(
            parts,
            vec![
                (" Title [", Some(Color::Gray)),
                ("r: Refresh", Some(KEY_HINT)),
                (" | ", Some(Color::Gray)),
                ("d: Drop", Some(KEY_HINT)),
                ("] tail « open", Some(Color::Gray)),
            ]
        );
    }

    #[test]
    fn every_view_shows_key_hints_in_blue_without_markers() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = AppState::unloaded(test_paths(tmp.path()), None);
        app.index.manuals = vec![manual_info("alpha")];
        app.rebuild_manual_rows();

        let render = |app: &AppState| {
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(220, 40)).unwrap();
            terminal.draw(|f| draw_ui(f, app)).unwrap();
            terminal.backend().buffer().clone()
        };
        let row_text = |buf: &ratatui::buffer::Buffer, y: u16| -> Vec<String> {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect()
        };
        // Colour of the first cell of `needle` on screen.
        let colour_of = |buf: &ratatui::buffer::Buffer, needle: &str| -> Option<Color> {
            for y in 0..buf.area.height {
                let cells = row_text(buf, y);
                let first: Vec<char> = needle.chars().collect();
                let n = first.len();
                for x in 0..cells.len().saturating_sub(n) {
                    if (0..n).all(|i| cells[x + i] == first[i].to_string()) {
                        return buf[(x as u16, y)].fg.into();
                    }
                }
            }
            None
        };

        let mut screens = Vec::new();
        for tab in 0..TAB_TITLES.len() {
            app.active_tab = tab;
            screens.push(render(&app));
        }
        app.active_tab = 0;
        app.help_open = true;
        screens.push(render(&app));
        app.help_open = false;
        app.pending_confirm = Some(ConfirmAction::Purge("alpha".into()));
        screens.push(render(&app));
        app.pending_confirm = None;
        app.active_tab = 1;
        app.try_popup = Some(TryPopup {
            tool: "alpha.tool".into(),
            args: "{}".into(),
            cursor: 1,
            editing: false,
            output: TryOutput::Idle,
            scroll: 0,
        });
        screens.push(render(&app));
        app.try_popup = None;
        app.active_tab = 0;
        app.hint_popup = Some(HintPopup {
            manual: "alpha".into(),
            text: "docs".into(),
            cursor: 4,
            error: None,
        });
        screens.push(render(&app));
        for buf in &screens {
            let text: String = (0..buf.area.height)
                .flat_map(|y| row_text(buf, y))
                .collect();
            assert!(!text.contains('«') && !text.contains('»'), "{text}");
        }

        assert_eq!(
            colour_of(&screens[0], "Enter: Browse tools"),
            Some(KEY_HINT)
        );
        assert_eq!(
            colour_of(&screens[0], "Registered Manuals"),
            Some(Color::Reset)
        );
        assert_eq!(colour_of(&screens[0], "D: Daemon"), Some(KEY_HINT));
        assert_eq!(colour_of(&screens[1], "x: clear"), Some(KEY_HINT));
        assert_eq!(
            colour_of(&screens[2], "r: import + register"),
            Some(KEY_HINT)
        );
        assert_eq!(colour_of(&screens[3], "s: Sync / Repair"), Some(KEY_HINT));
        assert_eq!(colour_of(&screens[4], "Enter: details"), Some(KEY_HINT));
        assert_eq!(colour_of(&screens[5], "[Shift+D]"), Some(KEY_HINT));
        assert_eq!(colour_of(&screens[7], "Esc: cancel"), Some(KEY_HINT));
        assert_eq!(colour_of(&screens[8], "d: dry run"), Some(KEY_HINT));
        assert_eq!(colour_of(&screens[8], "Enter: call"), Some(KEY_HINT));
        assert_eq!(colour_of(&screens[0], "h: edit"), Some(KEY_HINT));
        assert_eq!(colour_of(&screens[9], "Enter: save"), Some(KEY_HINT));
        assert_eq!(colour_of(&screens[9], "Esc: cancel"), Some(KEY_HINT));
    }

    /// Tab 1 with registered `alpha` (hint "first letter") and `gamma` (no hint) in both the
    /// index and the config, plus a deregistered `beta`. Nothing here reaches the real `$HOME`.
    fn hint_tab_app(root: &Path) -> AppState {
        let paths = test_paths(root);
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        let entry = |name: &str| {
            serde_json::json!({
                "name": name,
                "source_url": format!("https://example.invalid/{name}.json"),
                "cached": true,
            })
        };
        let mut alpha = entry("alpha");
        alpha["hint"] = "first letter".into();
        let config = serde_json::json!({
            "version": "1.0",
            "manuals": { "alpha": alpha, "gamma": entry("gamma") },
            "deregistered": { "beta": entry("beta") },
        });
        std::fs::write(&paths.config_file, config.to_string()).unwrap();
        let mut app = AppState::unloaded(paths, None);
        app.index.manuals = vec![manual_info("alpha"), manual_info("gamma")];
        app.rebuild_manual_rows();
        app
    }

    fn saved_hint(app: &AppState, name: &str) -> Option<String> {
        let cfg = UtcpRegistryConfig::load(&app.paths).unwrap();
        cfg.manuals
            .get(name)
            .or_else(|| cfg.deregistered.get(name))
            .and_then(|e| e.hint.clone())
    }

    /// Sends each character of `s` as a key press.
    fn type_text(app: &mut AppState, s: &str) {
        for c in s.chars() {
            handle_key(app, KeyCode::Char(c));
        }
    }

    #[test]
    fn manuals_tab_shows_the_selected_manuals_skill_label() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = hint_tab_app(tmp.path());
        assert!(
            !app.refresh_global_skills,
            "tests never rewrite real skills"
        );
        let screen = screen_text(&app);
        assert!(screen.contains("Skill hint [h: edit]"), "{screen}");
        assert!(
            screen.contains("lists it as: alpha (first letter)"),
            "{screen}"
        );

        handle_key(&mut app, KeyCode::Down);
        let screen = screen_text(&app);
        assert!(
            screen.contains("No hint: agents' skill shows just the name 'gamma'. Press h"),
            "{screen}"
        );
    }

    #[test]
    fn h_edits_saves_clears_and_cancels_a_hint() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = hint_tab_app(tmp.path());
        handle_key(&mut app, KeyCode::Down);
        handle_key(&mut app, KeyCode::Char('h'));
        let p = app.hint_popup.as_ref().expect("popup open");
        assert_eq!((p.manual.as_str(), p.text.as_str()), ("gamma", ""));
        let screen = screen_text(&app);
        assert!(screen.contains("Hint for 'gamma'"), "{screen}");
        assert!(screen.contains("TYPING"), "{screen}");

        // Every key types, even ones that are commands elsewhere (q, d, 1).
        type_text(&mut app, "third  q d1");
        assert!(!app.should_quit);
        assert_eq!(app.active_tab, 0);
        let screen = screen_text(&app);
        assert!(
            screen.contains("Agents will see: gamma (third q d1)"),
            "{screen}"
        );
        assert!(screen.contains("10/60 characters"), "{screen}");
        handle_key(&mut app, KeyCode::Enter);
        assert!(app.hint_popup.is_none());
        assert_eq!(saved_hint(&app, "gamma").as_deref(), Some("third q d1"));
        assert!(
            app.status_message.contains("gamma (third q d1)"),
            "{}",
            app.status_message
        );
        assert!(screen_text(&app).contains("lists it as: gamma (third q d1)"));

        // Prefilled with the saved hint, caret at the end; Esc leaves it unchanged.
        handle_key(&mut app, KeyCode::Char('h'));
        assert_eq!(app.hint_popup.as_ref().unwrap().cursor, 10);
        handle_key(&mut app, KeyCode::Backspace);
        handle_key(&mut app, KeyCode::Esc);
        assert!(app.hint_popup.is_none());
        assert_eq!(saved_hint(&app, "gamma").as_deref(), Some("third q d1"));

        // Emptying the text and saving clears it.
        handle_key(&mut app, KeyCode::Char('h'));
        for _ in 0..10 {
            handle_key(&mut app, KeyCode::Backspace);
        }
        handle_key(&mut app, KeyCode::Enter);
        assert_eq!(saved_hint(&app, "gamma"), None);
        assert_eq!(
            app.status_message,
            "Hint cleared. Agents' skills now show 'gamma' with no hint."
        );
    }

    #[test]
    fn too_long_hint_stays_open_with_the_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = hint_tab_app(tmp.path());
        handle_key(&mut app, KeyCode::Char('h'));
        type_text(&mut app, &" and more".repeat(7));
        let screen = screen_text(&app);
        assert!(screen.contains("75/60 characters"), "{screen}");
        handle_key(&mut app, KeyCode::Enter);
        let p = app.hint_popup.as_ref().expect("still open");
        assert!(p.error.as_deref().unwrap().contains("a few words"));
        assert_eq!(saved_hint(&app, "alpha").as_deref(), Some("first letter"));
        // Editing clears the error; Home then Delete edits at the caret.
        handle_key(&mut app, KeyCode::Home);
        assert!(app.hint_popup.as_ref().unwrap().error.is_none());
        handle_key(&mut app, KeyCode::Delete);
        assert!(app.hint_popup.as_ref().unwrap().text.starts_with("irst"));
    }

    #[test]
    fn deregistered_manual_keeps_a_hint_and_saved_files_need_registering() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = hint_tab_app(tmp.path());
        let manuals_dir = app.paths.manuals_dir.clone();
        std::fs::create_dir_all(&manuals_dir).unwrap();
        let orphan = serde_json::json!({
            "manual_version": "1.0.0", "utcp_version": "1.0.0",
            "info": {"title": "zeta", "version": "1"},
            "_meta": {"manual_name": "zeta", "source_url": "x", "sha256": "0",
                      "fetched_at": "t", "tool_count": 0, "protocols": [],
                      "required_env_vars": []},
            "tools": [],
        });
        std::fs::write(manuals_dir.join("zeta.utcp.json"), orphan.to_string()).unwrap();
        handle_key(&mut app, KeyCode::Char('a'));
        let pos = |app: &AppState, n: &str| {
            app.manual_rows
                .iter()
                .position(|r| r.info.name == n)
                .unwrap()
        };

        app.selected_manual = pos(&app, "beta");
        handle_key(&mut app, KeyCode::Char('h'));
        type_text(&mut app, "second");
        handle_key(&mut app, KeyCode::Enter);
        assert_eq!(saved_hint(&app, "beta").as_deref(), Some("second"));
        assert!(
            app.status_message.contains("deregistered"),
            "{}",
            app.status_message
        );
        app.selected_manual = pos(&app, "beta");
        assert!(screen_text(&app).contains("hint kept: second"));

        app.selected_manual = pos(&app, "zeta");
        handle_key(&mut app, KeyCode::Char('h'));
        assert!(app.hint_popup.is_none());
        assert!(
            app.status_message.contains("register it first"),
            "{}",
            app.status_message
        );
    }

    #[test]
    fn a_toggles_between_registered_and_all_manuals() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = manuals_tab_app(tmp.path());
        assert_eq!(
            row_names(&app),
            vec![("alpha".into(), true), ("gamma".into(), true)],
            "Registered is the default view"
        );
        assert!(screen_text(&app).contains("Registered Manuals"));
        let header = screen_text(&app)
            .lines()
            .find(|l| l.contains("Credentials"))
            .unwrap()
            .to_string();
        assert!(
            header.find("Name") < header.find("Credentials")
                && header.find("Credentials") < header.find("State"),
            "Name first, State last: {header}"
        );
        let alpha_row = screen_text(&app)
            .lines()
            .find(|l| l.contains("alpha"))
            .unwrap()
            .to_string();
        assert!(
            alpha_row.find("alpha") < alpha_row.find("● registered"),
            "row cells follow the header: name first, state last: {alpha_row}"
        );

        handle_key(&mut app, KeyCode::Down);
        assert_eq!(app.selected_manual, 1, "gamma selected");
        handle_key(&mut app, KeyCode::Char('a'));
        assert!(app.show_all_manuals);
        assert_eq!(
            row_names(&app),
            vec![
                ("alpha".into(), true),
                ("beta".into(), false),
                ("gamma".into(), true)
            ],
            "unregistered rows are merged in by name"
        );
        assert_eq!(
            app.selected_manual, 2,
            "selection follows gamma, not the index"
        );
        let screen = screen_text(&app);
        assert!(
            screen.contains("All Manuals (2 registered, 1 not)"),
            "{screen}"
        );
        assert!(screen.contains("○ copy missing"), "{screen}");

        // Enter and d on the unregistered row only explain; they open nothing.
        handle_key(&mut app, KeyCode::Up);
        assert_eq!(app.selected_manual_row().unwrap().info.name, "beta");
        handle_key(&mut app, KeyCode::Enter);
        assert_eq!(app.active_tab, 0);
        assert!(
            app.status_message.contains("Press «r» to register"),
            "{}",
            app.status_message
        );
        handle_key(&mut app, KeyCode::Char('d'));
        assert!(app.pending_confirm.is_none());

        // r on a row whose saved copy is gone fails with a hint and changes nothing.
        handle_key(&mut app, KeyCode::Char('r'));
        assert!(
            app.status_message.starts_with("Register failed"),
            "{}",
            app.status_message
        );
        assert!(app
            .status_message
            .contains("https://example.invalid/beta.json"));

        handle_key(&mut app, KeyCode::Char('a'));
        assert!(!app.show_all_manuals);
        assert_eq!(row_names(&app).len(), 2);
    }

    #[test]
    fn deregister_and_purge_wait_for_confirmation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = manuals_tab_app(tmp.path());

        handle_key(&mut app, KeyCode::Char('d'));
        assert_eq!(
            app.pending_confirm,
            Some(ConfirmAction::Deregister("alpha".into()))
        );
        let screen = screen_text(&app);
        assert!(screen.contains("Deregister manual?"), "{screen}");
        assert!(screen.contains("Deregister 'alpha'?"), "{screen}");

        // While the popup is open, other keys (even q and tab switches) do nothing.
        for code in [KeyCode::Char('q'), KeyCode::Char('2'), KeyCode::Down] {
            handle_key(&mut app, code);
        }
        assert!(!app.should_quit);
        assert_eq!(app.active_tab, 0);
        assert_eq!(app.selected_manual, 0);
        assert!(app.pending_confirm.is_some());
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 5,
                row: 10,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
        );
        assert_eq!(app.selected_manual, 0, "mouse is ignored while confirming");

        handle_key(&mut app, KeyCode::Char('n'));
        assert!(app.pending_confirm.is_none());
        assert_eq!(app.status_message, "Cancelled; nothing changed.");
        assert_eq!(row_names(&app).len(), 2);

        handle_key(&mut app, KeyCode::Char('p'));
        assert_eq!(
            app.pending_confirm,
            Some(ConfirmAction::Purge("alpha".into()))
        );
        assert!(screen_text(&app).contains("Purge 'alpha' for good?"));
        handle_key(&mut app, KeyCode::Esc);
        assert!(app.pending_confirm.is_none());

        // y runs the action. 'alpha' is only in the in-memory index (not the config, not on
        // disk), so the real deregister reports it as not found without touching anything.
        handle_key(&mut app, KeyCode::Char('d'));
        handle_key(&mut app, KeyCode::Char('y'));
        assert!(app.pending_confirm.is_none());
        assert_eq!(app.status_message, "Manual 'alpha' not found.");
    }

    /// Tab 4 pointed at `dir` as the start folder, without probing `$HOME` or the daemon.
    fn folder_tab_app(root: &Path, dir: PathBuf) -> AppState {
        let mut app = AppState::unloaded(test_paths(root), None);
        app.active_tab = 3;
        app.folder_dir = Some(dir);
        app.reload_folder_skill();
        app
    }

    #[test]
    fn w_adds_skill_to_start_folder_after_confirm() {
        let tmp = tempfile::TempDir::new().unwrap();
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(proj.join(".agents/skills/other-skill")).unwrap();
        let skill = proj.join(".agents/skills/utcp-tools/SKILL.md");
        let admin = proj.join(".agents/skills/utcp-admin");
        let mut app = folder_tab_app(tmp.path(), proj.clone());

        let screen = screen_text(&app);
        assert!(screen.contains("4: Settings & Skills"), "{screen}");
        assert!(screen.contains("This Folder: Project Skills"), "{screen}");
        assert!(screen.contains("1 other skill(s): other-skill"), "{screen}");
        assert!(screen.contains("NOT INSTALLED"), "{screen}");

        // The popup defaults to No, so a bare Enter writes nothing.
        handle_key(&mut app, KeyCode::Char('w'));
        assert_eq!(
            app.pending_confirm,
            Some(ConfirmAction::InstallFolderSkill(proj.clone()))
        );
        assert!(screen_text(&app).contains("Add skills to this folder?"));
        handle_key(&mut app, KeyCode::Enter);
        assert!(!skill.exists());

        handle_key(&mut app, KeyCode::Char('w'));
        handle_key(&mut app, KeyCode::Char('y'));
        assert!(skill.exists(), "{}", app.status_message);
        assert!(admin.join("SKILL.md").exists() && admin.join("operating.md").exists());
        assert!(app
            .status_message
            .starts_with("Added the utcp-tools and utcp-admin skills"));
        let folder = app.folder_skill.as_ref().unwrap();
        assert!(folder.target.up_to_date);
        assert_eq!(folder.other_skills, vec!["other-skill".to_string()]);
        assert!(screen_text(&app).contains("UP TO DATE"));

        handle_key(&mut app, KeyCode::Char('w'));
        assert!(app.pending_confirm.is_none(), "nothing to ask when current");
        assert!(app.status_message.starts_with("Already up to date"));
    }

    #[test]
    fn w_reports_failure_when_folder_cannot_hold_skills() {
        // A regular file stands in for a folder we cannot write: creating `.agents/` under it fails.
        let tmp = tempfile::TempDir::new().unwrap();
        let not_dir = tmp.path().join("not-a-dir");
        std::fs::write(&not_dir, "x").unwrap();
        let mut app = folder_tab_app(tmp.path(), not_dir.clone());
        assert!(screen_text(&app).contains("none here"));

        handle_key(&mut app, KeyCode::Char('w'));
        handle_key(&mut app, KeyCode::Char('y'));
        assert!(
            app.status_message.starts_with("Could not add the skills"),
            "{}",
            app.status_message
        );
        assert!(!app.folder_skill.as_ref().unwrap().target.exists);
    }

    #[test]
    fn confirm_popup_keeps_buttons_visible_for_long_folder_paths() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Longer than the 110-column test screen, so the question and details must wrap.
        let deep = tmp
            .path()
            .join("a-very-long-directory-name-for-wrapping".repeat(2))
            .join("another-quite-long-directory-segment".repeat(2));
        std::fs::create_dir_all(&deep).unwrap();
        let mut app = folder_tab_app(tmp.path(), deep);

        handle_key(&mut app, KeyCode::Char('w'));
        let screen = screen_text(&app);
        assert!(screen.contains("Add skills to this folder?"), "{screen}");
        assert!(
            screen.contains("Yes (y)") && screen.contains("No (n)"),
            "buttons must not be clipped by wrapped text: {screen}"
        );
    }

    #[test]
    fn confirm_popup_arrows_choose_and_enter_picks() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = manuals_tab_app(tmp.path());

        // No is highlighted by default, so a bare Enter cancels.
        handle_key(&mut app, KeyCode::Char('d'));
        assert!(!app.confirm_yes);
        assert!(screen_text(&app).contains("Yes (y)"));
        handle_key(&mut app, KeyCode::Enter);
        assert!(app.pending_confirm.is_none());
        assert_eq!(app.status_message, "Cancelled; nothing changed.");

        // ←/→ toggle the highlight; Enter on Yes runs the action.
        handle_key(&mut app, KeyCode::Char('p'));
        handle_key(&mut app, KeyCode::Left);
        assert!(app.confirm_yes);
        handle_key(&mut app, KeyCode::Right);
        assert!(!app.confirm_yes);
        handle_key(&mut app, KeyCode::Right);
        assert!(app.confirm_yes);
        assert!(
            app.pending_confirm.is_some(),
            "arrows only move the highlight"
        );
        handle_key(&mut app, KeyCode::Enter);
        assert!(app.pending_confirm.is_none());
        assert_eq!(app.status_message, "Manual 'alpha' not found.");

        // Reopening starts on No again.
        handle_key(&mut app, KeyCode::Char('d'));
        assert!(!app.confirm_yes);
        handle_key(&mut app, KeyCode::Esc);
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn all_text(lines: &[Line<'_>]) -> String {
        lines.iter().map(line_text).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn schema_and_docs_views_render_different_content() {
        let detail = ToolSchemaDetail {
            name: "github.get_file_contents".into(),
            manual: "github".into(),
            protocol: "http".into(),
            description: "Get file contents.\n\nLong usage notes here.".into(),
            tags: vec!["files".into()],
            inputs: serde_json::json!({
                "type": "object",
                "properties": {
                    "ref": {"type": "string", "description": "Optional git ref"},
                    "owner": {"type": "string", "description": "Repository owner"},
                    "mode": {"type": "string", "enum": ["raw", "json"], "default": "raw"},
                    "paths": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["owner"]
            }),
            outputs: serde_json::json!({"type": "object"}),
        };

        let schema = all_text(&build_preview_lines(Some(&detail), SchemaView::Schema));
        assert!(schema.contains("Input Schema (Parameters)"));
        assert!(schema.contains("Output Schema"));
        assert!(schema.contains("\"required\": ["), "raw JSON is shown");
        assert!(
            !schema.contains("Long usage notes here."),
            "full docs live in the Docs view"
        );

        let docs_lines = build_preview_lines(Some(&detail), SchemaView::Docs);
        let docs = all_text(&docs_lines);
        assert!(docs.contains("Long usage notes here."));
        assert!(!docs.contains("Output Schema"));
        assert!(docs.contains("Tags: files"));

        // Readable parameter list: required first, types, descriptions, enum and default.
        let params: Vec<String> = docs_lines
            .iter()
            .map(line_text)
            .filter(|l| l.starts_with("• "))
            .collect();
        assert_eq!(params[0], "• owner (string, required): Repository owner");
        assert!(params.contains(&"• ref (string): Optional git ref".to_string()));
        assert!(params.contains(&"• paths (string[])".to_string()));
        assert!(docs.contains("one of: \"raw\", \"json\" | default: \"raw\""));

        let no_params = ToolSchemaDetail {
            inputs: serde_json::json!({"type": "object", "properties": {}}),
            ..detail
        };
        let docs = all_text(&build_preview_lines(Some(&no_params), SchemaView::Docs));
        assert!(docs.contains("(no parameters)"));
    }

    #[test]
    fn d_toggles_schema_docs_and_the_choice_persists_for_the_session() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = search_tab_app(tmp.path());
        assert_eq!(
            app.schema_view,
            SchemaView::Schema,
            "every launch starts on Schema"
        );

        app.schema_scroll = 4;
        handle_key(&mut app, KeyCode::Char('d'));
        assert_eq!(app.schema_view, SchemaView::Docs);
        assert_eq!(
            app.schema_scroll, 0,
            "toggling starts the new view at the top"
        );

        // Works from either pane, and survives tool changes and tab switches.
        handle_key(&mut app, KeyCode::Down);
        handle_key(&mut app, KeyCode::Right);
        handle_key(&mut app, KeyCode::Char('1'));
        handle_key(&mut app, KeyCode::Char('2'));
        assert_eq!(app.active_tab, 1);
        assert_eq!(app.schema_view, SchemaView::Docs);

        // While typing in the search box, 'd' is just a letter.
        handle_key(&mut app, KeyCode::Char('/'));
        handle_key(&mut app, KeyCode::Char('d'));
        handle_key(&mut app, KeyCode::Enter);
        assert!(app.search_query.ends_with('d'));
        assert_eq!(app.schema_view, SchemaView::Docs);

        handle_key(&mut app, KeyCode::Char('d'));
        assert_eq!(app.schema_view, SchemaView::Schema);

        // A fresh UI instance (the next launch) is back on Schema.
        let fresh = AppState::unloaded(test_paths(tmp.path()), None);
        assert_eq!(fresh.schema_view, SchemaView::Schema);
    }

    #[test]
    fn schema_pane_scrolls_through_wrapped_rows_and_clamps() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = search_tab_app(tmp.path());
        // The long paragraphs live in the Docs view.
        app.schema_view = SchemaView::Docs;
        draw_once(&app);

        let view = app.schema_view_rows.get();
        let max = app.schema_max_scroll.get();
        assert!(view > 0);
        // The Docs view has only ~10 logical lines (fewer than the 16-row pane), so the old
        // logical-line limit would be 0. Wrapping the 300-char paragraphs makes it scrollable.
        assert!(
            max > 0,
            "wrapped content must be scrollable, got max_scroll=0"
        );

        // PgDn pages by the visible height minus one row of overlap.
        handle_key(&mut app, KeyCode::PageDown);
        assert_eq!(app.schema_scroll, (view - 1).min(max));

        // Scrolling never runs past the end, so PgUp responds immediately afterwards.
        for _ in 0..50 {
            handle_key(&mut app, KeyCode::PageDown);
        }
        assert_eq!(app.schema_scroll, max);
        handle_key(&mut app, KeyCode::PageUp);
        assert_eq!(app.schema_scroll, max.saturating_sub(view - 1));

        handle_key(&mut app, KeyCode::Home);
        assert_eq!(app.schema_scroll, 0);
        handle_key(&mut app, KeyCode::End);
        assert_eq!(app.schema_scroll, max);
    }

    #[test]
    fn arrow_keys_follow_pane_focus() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = search_tab_app(tmp.path());
        app.schema_view = SchemaView::Docs;
        draw_once(&app);
        assert_eq!(app.search_focus, SearchPane::Matches);

        // Focus the Schema pane: ↓/↑ scroll it one row and leave the selection alone.
        handle_key(&mut app, KeyCode::Right);
        assert_eq!(app.search_focus, SearchPane::Schema);
        handle_key(&mut app, KeyCode::Down);
        handle_key(&mut app, KeyCode::Down);
        handle_key(&mut app, KeyCode::Up);
        assert_eq!(app.schema_scroll, 1);
        assert_eq!(app.selected_tool, 0);

        // Back on Matches: ↓ moves the selection (which resets the schema scroll).
        handle_key(&mut app, KeyCode::Left);
        assert_eq!(app.search_focus, SearchPane::Matches);
        handle_key(&mut app, KeyCode::Down);
        assert_eq!(app.selected_tool, 1);
        assert_eq!(app.schema_scroll, 0);
    }

    #[test]
    fn typing_box_is_marked_and_arrows_leave_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = search_tab_app(tmp.path());
        assert!(!screen_text(&app).contains("TYPING"));

        // `/` starts typing: the box carries the TYPING badge and arrows are not typed.
        handle_key(&mut app, KeyCode::Char('/'));
        assert!(app.search_editing);
        assert!(screen_text(&app).contains("TYPING"));
        handle_key(&mut app, KeyCode::Down);
        assert!(!app.search_editing);
        assert_eq!(app.search_focus, SearchPane::Matches);
        assert!(!screen_text(&app).contains("TYPING"));

        handle_key(&mut app, KeyCode::Char('/'));
        handle_key(&mut app, KeyCode::Right);
        assert!(!app.search_editing);
        assert_eq!(app.search_focus, SearchPane::Schema);

        handle_key(&mut app, KeyCode::Char('/'));
        handle_key(&mut app, KeyCode::Left);
        assert!(!app.search_editing);
        assert_eq!(app.search_focus, SearchPane::Matches);

        // Tab 3: the field being typed in carries the badge; ↑/↓ move between fields.
        app.active_tab = 2;
        handle_key(&mut app, KeyCode::Char('e'));
        assert!(screen_text(&app).contains("TYPING"));
        handle_key(&mut app, KeyCode::Down);
        assert_eq!(app.import_field, ImportField::Name);
        handle_key(&mut app, KeyCode::Up);
        assert_eq!(app.import_field, ImportField::Source);
        assert!(app.import_editing);
        handle_key(&mut app, KeyCode::Esc);
        assert!(!screen_text(&app).contains("TYPING"));
    }

    #[test]
    fn up_on_the_first_match_returns_to_the_search_box() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = search_tab_app(tmp.path());
        assert_eq!(app.search_focus, SearchPane::Matches);

        // ↑ on a lower row only moves the selection.
        handle_key(&mut app, KeyCode::Down);
        handle_key(&mut app, KeyCode::Up);
        assert_eq!(app.selected_tool, 0);
        assert!(!app.search_editing);

        // ↑ (or k) on the first row goes into the box; ↓ comes back to the same row.
        handle_key(&mut app, KeyCode::Up);
        assert!(app.search_editing);
        assert!(screen_text(&app).contains("TYPING"));
        handle_key(&mut app, KeyCode::Down);
        assert!(!app.search_editing);
        assert_eq!(app.selected_tool, 0);
        handle_key(&mut app, KeyCode::Char('k'));
        assert!(app.search_editing);
        handle_key(&mut app, KeyCode::Esc);

        // From a Tab 1 manual filter it starts a new search, like `/`.
        app.came_from_manuals = true;
        handle_key(&mut app, KeyCode::Up);
        assert!(app.search_editing);
        assert!(!app.came_from_manuals);
        handle_key(&mut app, KeyCode::Esc);

        // The Schema pane still scrolls, and the wheel still wraps the list.
        handle_key(&mut app, KeyCode::Right);
        handle_key(&mut app, KeyCode::Up);
        assert!(!app.search_editing);
        handle_key(&mut app, KeyCode::Left);
        draw_once(&app);
        let matches = app.matches_area.get();
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: matches.x + 2,
                row: matches.y + 2,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
        );
        assert_eq!(app.selected_tool, 1);
        assert!(!app.search_editing);

        // With no matches, ↑ also goes to the box.
        app.search_results.clear();
        handle_key(&mut app, KeyCode::Up);
        assert!(app.search_editing);
    }

    #[test]
    fn mouse_wheel_routes_to_the_pane_under_the_pointer() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = search_tab_app(tmp.path());
        app.schema_view = SchemaView::Docs;
        draw_once(&app);

        let wheel = |kind, area: Rect| MouseEvent {
            kind,
            column: area.x + 2,
            row: area.y + 2,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };

        // Wheel over the Schema pane scrolls it even while Matches has keyboard focus.
        let schema = app.schema_area.get();
        handle_mouse(&mut app, wheel(MouseEventKind::ScrollDown, schema));
        assert_eq!(
            app.schema_scroll,
            WHEEL_SCROLL_ROWS.min(app.schema_max_scroll.get())
        );
        assert_eq!(app.selected_tool, 0);
        handle_mouse(&mut app, wheel(MouseEventKind::ScrollUp, schema));
        assert_eq!(app.schema_scroll, 0);

        // Wheel over Matches moves the selection.
        let matches = app.matches_area.get();
        handle_mouse(&mut app, wheel(MouseEventKind::ScrollDown, matches));
        assert_eq!(app.selected_tool, 1);

        // A left click focuses the clicked pane.
        handle_mouse(
            &mut app,
            wheel(MouseEventKind::Down(MouseButton::Left), schema),
        );
        assert_eq!(app.search_focus, SearchPane::Schema);
    }

    fn activity(ok: bool, detail: Option<&str>) -> ActivityEntry {
        ActivityEntry {
            timestamp: "2026-11-16T10:00:00Z".into(),
            mode: "daemon".into(),
            caller: "zed".into(),
            command: "call".into(),
            target: "d2.validate".into(),
            ok,
            status: if ok { "OK" } else { "ERR" }.into(),
            latency_ms: 12.5,
            detail: detail.map(str::to_string),
        }
    }

    fn activity_tab_app(root: &Path, entries: Vec<ActivityEntry>) -> AppState {
        let mut app = AppState::unloaded(test_paths(root), None);
        app.activities = entries;
        app.active_tab = 4;
        app
    }

    #[test]
    fn activity_times_show_local_clock_and_date_only_for_other_days() {
        let berlin = chrono::FixedOffset::east_opt(3600).unwrap();
        let today = chrono::NaiveDate::from_ymd_opt(2026, 11, 16).unwrap();
        assert_eq!(
            local_activity_time("2026-11-16T17:04:43.182175900+00:00", &berlin, today),
            ("18:04:43".into(), "2026-11-16 18:04:43".into())
        );
        // 23:30 UTC on the 15th is already the 16th at UTC+1, so it counts as today.
        assert_eq!(
            local_activity_time("2026-11-15T23:30:00Z", &berlin, today).0,
            "00:30:00"
        );
        assert_eq!(
            local_activity_time("2026-11-14T09:00:00Z", &berlin, today),
            ("11-14 10:00:00".into(), "2026-11-14 10:00:00".into())
        );
        assert_eq!(
            local_activity_time("not a time", &berlin, today),
            ("not a time".into(), "not a time".into())
        );
    }

    #[test]
    fn enter_on_activity_row_shows_error_detail_and_esc_closes_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = activity_tab_app(
            tmp.path(),
            vec![
                activity(true, None),
                activity(
                    false,
                    Some("connection missing destination\nmaps must be terminated with }"),
                ),
            ],
        );
        assert!(screen_text(&app).contains("Enter: details"));
        assert!(!screen_text(&app).contains("connection missing destination"));

        handle_key(&mut app, KeyCode::Down);
        handle_key(&mut app, KeyCode::Enter);
        let screen = screen_text(&app);
        assert!(screen.contains("Activity details"), "{screen}");
        assert!(
            screen.contains("connection missing destination"),
            "{screen}"
        );
        assert!(
            screen.contains("maps must be terminated with }"),
            "{screen}"
        );

        // The popup keeps its copy when the list reloads with a new row on top.
        app.activities.insert(0, activity(true, None));
        assert!(screen_text(&app).contains("connection missing destination"));

        // While open, keys go to the popup: Tab and digits do not switch tabs.
        handle_key(&mut app, KeyCode::Char('1'));
        assert_eq!(app.active_tab, 4);
        assert!(app.activity_detail.is_some());

        handle_key(&mut app, KeyCode::Esc);
        assert!(app.activity_detail.is_none());
        assert!(!screen_text(&app).contains("connection missing destination"));

        // q closes the popup instead of quitting the app.
        handle_key(&mut app, KeyCode::Enter);
        assert!(app.activity_detail.is_some());
        handle_key(&mut app, KeyCode::Char('q'));
        assert!(app.activity_detail.is_none());
        assert!(!app.should_quit);
    }

    #[test]
    fn activity_detail_popup_without_detail_says_so() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = activity_tab_app(tmp.path(), vec![activity(true, None)]);
        handle_key(&mut app, KeyCode::Enter);
        let screen = screen_text(&app);
        assert!(screen.contains("No details recorded"), "{screen}");
        assert!(screen.contains("d2.validate"), "{screen}");
    }

    #[test]
    fn enter_with_no_activity_opens_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = activity_tab_app(tmp.path(), Vec::new());
        handle_key(&mut app, KeyCode::Enter);
        assert!(app.activity_detail.is_none());
    }

    #[test]
    fn activity_detail_popup_scrolls_with_keys_and_wheel() {
        let tmp = tempfile::TempDir::new().unwrap();
        let long: Vec<String> = (1..=100).map(|i| format!("error line {i:03}")).collect();
        let mut app = activity_tab_app(tmp.path(), vec![activity(false, Some(&long.join("\n")))]);
        handle_key(&mut app, KeyCode::Enter);
        let screen = screen_text(&app);
        assert!(screen.contains("error line 001"));
        assert!(!screen.contains("error line 100"));
        let max = app.activity_detail_max_scroll.get();
        assert!(max > 0);

        handle_key(&mut app, KeyCode::Down);
        assert_eq!(app.activity_detail_scroll, 1);
        handle_key(&mut app, KeyCode::Up);
        handle_key(&mut app, KeyCode::Up);
        assert_eq!(app.activity_detail_scroll, 0, "clamped at the top");

        handle_key(&mut app, KeyCode::PageDown);
        let page = app.activity_detail_view_rows.get() - 1;
        assert_eq!(app.activity_detail_scroll, page);

        handle_key(&mut app, KeyCode::End);
        assert_eq!(app.activity_detail_scroll, max);
        let screen = screen_text(&app);
        assert!(screen.contains("error line 100"), "{screen}");
        assert!(!screen.contains("error line 001"));
        handle_key(&mut app, KeyCode::Down);
        assert_eq!(app.activity_detail_scroll, max, "clamped at the bottom");

        handle_key(&mut app, KeyCode::Home);
        assert_eq!(app.activity_detail_scroll, 0);

        let wheel_down = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 50,
            row: 15,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        handle_mouse(&mut app, wheel_down);
        assert_eq!(app.activity_detail_scroll, WHEEL_SCROLL_ROWS.min(max));
        assert_eq!(
            app.selected_activity, 0,
            "wheel scrolls the popup, not the list"
        );
    }

    fn mcp_form(source: &str, auth: &str) -> ImportForm {
        ImportForm {
            source: source.into(),
            auth: auth.into(),
            ..ImportForm::default()
        }
    }

    #[test]
    fn mcp_form_parses_urls_commands_and_auth() {
        let plan = plan_mcp_import(&mcp_form(
            "https://mcp.context7.com/mcp",
            "Authorization: Bearer ${CTX_TOKEN}; X-Team: blue",
        ))
        .unwrap();
        assert_eq!(plan.name, "context7");
        assert_eq!(plan.url.as_deref(), Some("https://mcp.context7.com/mcp"));
        assert_eq!(plan.command, None);
        assert_eq!(plan.headers["Authorization"], "Bearer ${CTX_TOKEN}");
        assert_eq!(plan.headers["X-Team"], "blue");
        assert!(plan.literal_values, "X-Team: blue has no ${{VAR}}");

        let plan = plan_mcp_import(&mcp_form(
            "  npx -y   @modelcontextprotocol/server-filesystem /tmp ",
            "ROOT=${FS_ROOT};",
        ))
        .unwrap();
        assert_eq!(plan.name, "filesystem");
        assert_eq!(plan.command.as_deref(), Some("npx"));
        assert_eq!(
            plan.args,
            ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
        );
        assert_eq!(plan.env["ROOT"], "${FS_ROOT}");
        assert!(!plan.literal_values);

        let typed = ImportForm {
            name: "fs".into(),
            ..mcp_form("npx -y pkg", "")
        };
        assert_eq!(plan_mcp_import(&typed).unwrap().name, "fs");

        let err = |form: ImportForm| plan_mcp_import(&form).unwrap_err();
        assert!(err(mcp_form("", "")).contains("Enter the MCP server"));
        assert!(err(mcp_form("npx -y \"my pkg\"", "")).contains("Quotes are not supported"));
        assert!(err(mcp_form("npx pkg", "Authorization: x")).contains("KEY=value"));
        assert!(err(mcp_form("https://x.example/mcp", "TOKEN=${T}")).contains("Header: value"));
        let bad_name = ImportForm {
            name: "my server".into(),
            ..mcp_form("npx pkg", "")
        };
        assert!(err(bad_name).contains("letters, digits"));
    }

    #[test]
    fn import_tab_switches_modes_and_keeps_each_form() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = AppState::unloaded(test_paths(tmp.path()), None);
        app.active_tab = 2;

        handle_key(&mut app, KeyCode::Char('e'));
        type_text(&mut app, "spec.yaml");
        handle_key(&mut app, KeyCode::Esc);

        handle_key(&mut app, KeyCode::Char('m'));
        assert_eq!(app.import_mode, ImportMode::Mcp);
        assert!(app.import_mcp.source.is_empty(), "MCP form starts empty");
        handle_key(&mut app, KeyCode::Enter);
        type_text(&mut app, "https://mcp.context7.com/mcp");
        handle_key(&mut app, KeyCode::Tab);
        handle_key(&mut app, KeyCode::Tab);
        assert_eq!(app.import_field, ImportField::Hint);
        type_text(&mut app, "library docs");
        handle_key(&mut app, KeyCode::Tab);
        assert_eq!(app.import_field, ImportField::Auth);
        type_text(&mut app, "X-Key: ${K}");
        handle_key(&mut app, KeyCode::BackTab);
        assert_eq!(app.import_field, ImportField::Hint);
        handle_key(&mut app, KeyCode::Esc);
        assert_eq!(app.import_mcp.auth, "X-Key: ${K}");
        assert_eq!(app.import_mcp.hint_arg(), Some("library docs"));
        assert!(screen_text(&app).contains("Hint (used by r)"));

        let screen = screen_text(&app);
        assert!(screen.contains("MCP server: https:// URL"), "{screen}");
        assert!(screen.contains("empty = 'context7'"), "{screen}");
        assert!(screen.contains("Headers (optional)"), "{screen}");
        let default_output = format!("imports{}context7.utcp", std::path::MAIN_SEPARATOR);
        assert!(screen.contains(&default_output), "{screen}");

        handle_key(&mut app, KeyCode::Char('m'));
        assert_eq!(app.import_mode, ImportMode::Spec);
        assert_eq!(app.import_spec.source, "spec.yaml", "spec form kept");
        assert_eq!(app.import_field, ImportField::Source);

        handle_key(&mut app, KeyCode::Char('x'));
        assert!(app.import_spec.source.is_empty());
        assert_eq!(app.import_mcp.source, "https://mcp.context7.com/mcp");

        // c with nothing to import explains instead of queueing.
        handle_key(&mut app, KeyCode::Char('c'));
        assert!(app.pending_import.is_none());
        assert!(app.status_message.contains("Enter a Source"));
        handle_key(&mut app, KeyCode::Char('m'));
        handle_key(&mut app, KeyCode::Char('r'));
        assert_eq!(app.pending_import, Some(true));
        assert!(app.status_message.contains("up to 60 s"));
    }

    #[test]
    fn help_popup_explains_the_current_tab_and_takes_every_key() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = AppState::unloaded(test_paths(tmp.path()), None);
        app.active_tab = 2;
        handle_key(&mut app, KeyCode::Char('?'));
        assert!(app.help_open);
        let screen = screen_text(&app);
        assert!(screen.contains("This tab: Tab 3: Import"), "{screen}");
        assert!(screen.contains("Help (line 1/"), "{screen}");

        let text: String = help_lines(2, &app.paths)
            .iter()
            .map(|l| l.to_string() + "\n")
            .collect();
        assert!(text.contains("MCP (Model Context Protocol)"));
        assert!(text.contains("utcp import-mcp context7 --url https://mcp.context7.com/mcp"));
        assert!(text.contains("Everywhere"));
        assert!(text.contains("Tab 6: Daemon Monitor"));
        assert!(help_lines(0, &app.paths)[0]
            .to_string()
            .contains("Tab 1: Manuals"));

        // Keys scroll the popup instead of acting on the tab; q closes it, not the app.
        handle_key(&mut app, KeyCode::Char('j'));
        assert_eq!(app.help_scroll, 1);
        assert_eq!(app.import_field, ImportField::Source);
        handle_key(&mut app, KeyCode::Char('m'));
        assert_eq!(app.import_mode, ImportMode::Spec);
        handle_key(&mut app, KeyCode::Char('q'));
        assert!(!app.help_open);
        assert!(!app.should_quit);

        // While a field is being edited, ? is just text.
        handle_key(&mut app, KeyCode::Char('e'));
        handle_key(&mut app, KeyCode::Char('?'));
        assert!(!app.help_open);
        assert_eq!(app.import_spec.source, "?");
    }

    /// A local HTTP MCP server that answers only with `Authorization: Bearer tok-tui`.
    fn spawn_auth_mcp_server() -> String {
        use axum::{http::HeaderMap, http::StatusCode, routing::post, Json, Router};
        use serde_json::{json, Value};
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let handler = |headers: HeaderMap, Json(req): Json<Value>| async move {
                    let ok = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        == Some("Bearer tok-tui");
                    if !ok {
                        return (StatusCode::UNAUTHORIZED, Json(json!({})));
                    }
                    let result = match req["method"].as_str() {
                        Some("tools/list") => json!({"tools": [
                            {"name": "lookup", "description": "Look up", "inputSchema": {"type": "object"}}
                        ]}),
                        _ => json!({"protocolVersion": "2024-11-05", "capabilities": {}}),
                    };
                    (
                        StatusCode::OK,
                        Json(json!({"jsonrpc": "2.0", "id": req["id"], "result": result})),
                    )
                };
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                tx.send(listener.local_addr().unwrap()).unwrap();
                axum::serve(listener, Router::new().route("/mcp", post(handler)))
                    .await
                    .unwrap();
            });
        });
        format!("http://{}/mcp", rx.recv().unwrap())
    }

    #[test]
    fn c_saves_an_mcp_import_to_the_imports_folder_with_placeholders_kept() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = test_paths(tmp.path());
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(&paths.env_file, "TUI_MCP_TOKEN=tok-tui\n").unwrap();
        let mut app = AppState::unloaded(paths, None);
        app.active_tab = 2;
        app.import_mode = ImportMode::Mcp;
        app.import_mcp = ImportForm {
            source: spawn_auth_mcp_server(),
            name: "tui_probe".into(),
            hint: String::new(),
            auth: "Authorization: Bearer ${TUI_MCP_TOKEN}".into(),
            output: String::new(),
        };

        handle_key(&mut app, KeyCode::Char('c'));
        assert_eq!(app.pending_import, Some(false), "queued until after a draw");
        app.run_pending_import();
        assert!(app.pending_import.is_none());

        let saved = tmp.path().join("cache/imports/tui_probe.utcp.json");
        assert!(
            app.status_message
                .starts_with("Saved 'tui_probe' (1 tools)"),
            "{}",
            app.status_message
        );
        assert!(
            app.status_message.contains("Needs: TUI_MCP_TOKEN"),
            "{}",
            app.status_message
        );
        let text = std::fs::read_to_string(&saved).unwrap();
        assert!(text.contains("${TUI_MCP_TOKEN}"));
        assert!(
            !text.contains("tok-tui"),
            "secret value written to the manual"
        );
        assert!(
            !app.paths.config_file.exists(),
            "c must not register anything"
        );

        // Without the token, the failure names the unset placeholder.
        std::fs::write(&app.paths.env_file, "").unwrap();
        app.execute_import(false);
        assert!(
            app.status_message.starts_with("Import failed"),
            "{}",
            app.status_message
        );
        assert!(
            app.status_message.contains("TUI_MCP_TOKEN"),
            "{}",
            app.status_message
        );
    }

    #[test]
    fn try_args_template_prefills_required_properties() {
        let inputs = serde_json::json!({
            "type": "object",
            "properties": {
                "count": {"type": "integer"},
                "message": {"type": "string"},
                "flag": {"type": ["boolean", "null"]},
                "optional": {"type": "string"}
            },
            "required": ["count", "message", "flag"]
        });
        let (text, caret) = try_args_template(&inputs);
        assert_eq!(text, r#"{"count": 0, "message": "", "flag": false}"#);
        assert_eq!(
            &text[..caret],
            r#"{"count": 0, "message": ""#,
            "caret inside the first \"\""
        );
        assert!(parse_try_args(&text).is_ok(), "the template is valid JSON");

        let (text, caret) = try_args_template(&serde_json::json!({"type": "object"}));
        assert_eq!(
            (text.as_str(), caret),
            ("{}", 1),
            "caret between the braces"
        );
    }

    #[test]
    fn parse_try_args_accepts_objects_only() {
        assert!(parse_try_args("").unwrap().is_empty());
        assert_eq!(parse_try_args(r#"{"a": 1}"#).unwrap()["a"], 1);
        assert!(parse_try_args("[1]")
            .unwrap_err()
            .contains("must be a JSON object"));
        assert!(parse_try_args("{\"a\"}")
            .unwrap_err()
            .contains("not valid JSON"));
    }

    #[test]
    fn t_opens_try_popup_that_edits_args_and_rejects_bad_json() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = search_tab_app(tmp.path());
        handle_key(&mut app, KeyCode::Char('t'));
        let popup = app.try_popup.as_ref().expect("t opens the Try popup");
        assert_eq!(popup.tool, "demo.first");
        assert_eq!(popup.args, "{}");
        assert!(!popup.editing, "nothing required, so d/Enter work at once");
        let screen = screen_text(&app);
        assert!(screen.contains("Try demo.first"), "{screen}");
        assert!(screen.contains("d: dry run"), "{screen}");
        assert!(!screen.contains('«'), "markers are hidden");

        // e types at the caret (between the braces); arrows, Backspace and Delete edit in place.
        handle_key(&mut app, KeyCode::Char('e'));
        assert!(screen_text(&app).contains("TYPING"));
        for c in "\"ab\"".chars() {
            handle_key(&mut app, KeyCode::Char(c));
        }
        handle_key(&mut app, KeyCode::Left);
        handle_key(&mut app, KeyCode::Backspace);
        handle_key(&mut app, KeyCode::Char('x'));
        handle_key(&mut app, KeyCode::Home);
        handle_key(&mut app, KeyCode::Delete);
        handle_key(&mut app, KeyCode::Char('{'));
        handle_key(&mut app, KeyCode::Char('q'));
        assert_eq!(app.try_popup.as_ref().unwrap().args, "{q\"ax\"}");
        assert!(app.try_popup.is_some(), "q while typing is just a letter");

        handle_key(&mut app, KeyCode::Esc);
        assert!(!app.try_popup.as_ref().unwrap().editing);
        handle_key(&mut app, KeyCode::Enter);
        let popup = app.try_popup.as_ref().unwrap();
        assert!(
            matches!(&popup.output, TryOutput::Error(e) if e.contains("not valid JSON")),
            "{:?}",
            popup.output
        );
        assert!(app.pending_try.is_none(), "bad JSON never runs");
        assert!(screen_text(&app).contains("not valid JSON"));

        handle_key(&mut app, KeyCode::Esc);
        assert!(app.try_popup.is_none(), "Esc closes the popup");
        assert_eq!(app.active_tab, 1, "and stays on Tab 2");
    }

    /// Tab 2 with a cached `demo` manual: `get_item` (HTTP, token from `.env`) and `hello` (runs
    /// `echo`, so a real call works offline). Logging is on so runs land in the activity log.
    fn try_manual_app(root: &Path) -> AppState {
        let paths = test_paths(root);
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(&paths.env_file, "DEMO_TOKEN=supersecret-try-value\n").unwrap();
        set_logging_enabled(&paths, true).unwrap();
        let manual = serde_json::json!({
            "manual_version": "1.0.0",
            "utcp_version": "1.0.0",
            "info": {"title": "demo", "version": "1.0.0"},
            "_meta": {
                "manual_name": "demo", "source_url": "test", "sha256": "", "fetched_at": "",
                "tool_count": 2, "protocols": ["http", "cli"], "required_env_vars": ["DEMO_TOKEN"]
            },
            "tools": [
                {
                    "name": "get_item", "description": "", "outputs": {},
                    "inputs": {
                        "type": "object",
                        "properties": {"id": {"type": "string"}},
                        "required": ["id"]
                    },
                    "provider": {
                        "name": "demo", "provider_type": "http", "type": "http",
                        "call_template_type": "http", "http_method": "GET",
                        "url": "https://example.invalid/items/{id}",
                        "headers": {"Authorization": "Bearer ${DEMO_TOKEN}"}
                    }
                },
                {
                    "name": "hello", "description": "", "outputs": {},
                    "inputs": {"type": "object", "properties": {}},
                    "provider": {
                        "name": "demo", "provider_type": "cli", "type": "cli",
                        "call_template_type": "cli", "command_name": "node",
                        "args_template": ["-e", "console.log('hello-from-try')"],
                        "direct_exec": true
                    }
                }
            ]
        });
        let file = paths.cached_manual_path("demo");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, manual.to_string()).unwrap();
        let mut registry = UtcpRegistryConfig::default();
        registry.manuals.insert(
            "demo".into(),
            utcp_core::catalog::RegisteredManualEntry {
                name: "demo".into(),
                source_url: "test".into(),
                cached: true,
                cached_file: Some(file.display().to_string()),
                base_url: None,
                headers: Default::default(),
                hint: None,
            },
        );
        registry.save(&paths).unwrap();

        let mut app = AppState::unloaded(paths, None);
        app.active_tab = 1;
        app
    }

    fn select_schema(app: &mut AppState, name: &str, inputs: serde_json::Value) {
        app.selected_schema = Some(ToolSchemaDetail {
            name: name.into(),
            manual: "demo".into(),
            protocol: "http".into(),
            description: String::new(),
            tags: Vec::new(),
            inputs,
            outputs: serde_json::json!({}),
        });
    }

    #[test]
    fn try_dry_run_hides_secrets_and_real_call_runs_and_both_are_logged() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = try_manual_app(tmp.path());
        select_schema(
            &mut app,
            "demo.get_item",
            serde_json::json!({
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"]
            }),
        );

        handle_key(&mut app, KeyCode::Char('t'));
        assert!(
            app.try_popup.as_ref().unwrap().editing,
            "a required argument starts typing"
        );
        for c in "42".chars() {
            handle_key(&mut app, KeyCode::Char(c));
        }
        assert_eq!(app.try_popup.as_ref().unwrap().args, r#"{"id": "42"}"#);
        handle_key(&mut app, KeyCode::Esc);

        handle_key(&mut app, KeyCode::Char('d'));
        assert_eq!(app.pending_try, Some(true), "queued until after a draw");
        assert!(screen_text(&app).contains("Preparing the dry run"));
        app.run_pending_try();
        assert!(app.pending_try.is_none());
        let TryOutput::DryRun(text) = &app.try_popup.as_ref().unwrap().output else {
            panic!("{:?}", app.try_popup.as_ref().unwrap().output);
        };
        assert!(text.contains("https://example.invalid/items/42"), "{text}");
        assert!(!text.contains("supersecret-try-value"), "{text}");
        assert!(app.status_message.contains("nothing was sent"));

        // A real call of an offline CLI tool shows its output in the same popup.
        handle_key(&mut app, KeyCode::Esc);
        select_schema(
            &mut app,
            "demo.hello",
            serde_json::json!({"type": "object"}),
        );
        handle_key(&mut app, KeyCode::Char('t'));
        handle_key(&mut app, KeyCode::Enter);
        assert_eq!(app.pending_try, Some(false));
        assert!(screen_text(&app).contains("Calling the tool"));
        app.run_pending_try();
        let popup = app.try_popup.as_ref().unwrap();
        assert!(
            matches!(&popup.output, TryOutput::Result(t) if t.contains("hello-from-try")),
            "{:?}",
            popup.output
        );
        assert!(app.status_message.contains("OK"), "{}", app.status_message);

        let log = read_recent_activity(&app.paths, 10);
        let mut runs: Vec<(&str, &str, bool)> = log
            .iter()
            .map(|e| (e.command.as_str(), e.caller.as_str(), e.ok))
            .collect();
        runs.sort();
        assert_eq!(runs, [("call", "tui", true), ("dry-run", "tui", true)]);
    }

    #[test]
    fn try_output_scrolls_with_keys_and_wheel() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = search_tab_app(tmp.path());
        handle_key(&mut app, KeyCode::Char('t'));
        let long: Vec<String> = (1..=200).map(|i| format!("line {i:03}")).collect();
        app.try_popup.as_mut().unwrap().output = TryOutput::Result(long.join("\n"));
        draw_once(&app);
        let max = app.try_max_scroll.get();
        assert!(max > 0);

        let scroll = |app: &AppState| app.try_popup.as_ref().unwrap().scroll;
        handle_key(&mut app, KeyCode::Char('j'));
        assert_eq!(scroll(&app), 1);
        handle_key(&mut app, KeyCode::End);
        assert_eq!(scroll(&app), max);
        handle_key(&mut app, KeyCode::PageDown);
        assert_eq!(scroll(&app), max, "clamped at the bottom");
        handle_key(&mut app, KeyCode::Home);
        assert_eq!(scroll(&app), 0);
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 0,
                row: 0,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
        );
        assert_eq!(scroll(&app), WHEEL_SCROLL_ROWS.min(max));
    }
}
