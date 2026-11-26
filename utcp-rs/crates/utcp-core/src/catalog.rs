use crate::index::{
    IndexedManual, IndexedTool, SearchIndex, ToolSchemaDetail, ToolSummary, SEARCH_INDEX_MAGIC,
};
use crate::loader::{create_provider_from_value, tool_to_provider, with_call_template_type};
use crate::openapi::{parse_spec_bytes, OpenApiConverter};
use crate::paths::{write_atomic, UtcpPaths, UtcpSettings};
use crate::providers::http::HttpProvider;
use crate::security::{build_secure_http_client, validate_url_security};
use crate::spec::ManualInfo;
use crate::transports::registry::CommunicationProtocolRegistry;
use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use futures::stream::{self, StreamExt};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

static VAR_PLACEHOLDER_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\$\{([A-Za-z0-9_]+)\}").expect("valid placeholder regex"));

/// Shared machine-wide user registry persisted in `.utcp_config.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UtcpRegistryConfig {
    #[serde(default = "default_registry_version")]
    pub version: String,
    #[serde(default)]
    pub manuals: BTreeMap<String, RegisteredManualEntry>,
    /// Manuals removed with `deregister` (without `--purge-cache`). Their settings and saved copy are
    /// kept so `register <name>` (no source) can restore them offline. Not searchable or callable.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub deregistered: BTreeMap<String, RegisteredManualEntry>,
    /// Manuals added with `stage`, waiting for `promote` (or `discard`). Their copy is in `manuals/staged/`. Not in
    /// search, `list` or the skills; callable only with `--staged`. At most one staged copy per name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub staged: BTreeMap<String, RegisteredManualEntry>,
    /// The registration each `promote` replaced, with its copy in `manuals/previous/`, for `rollback`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub previous: BTreeMap<String, RegisteredManualEntry>,
}

fn default_registry_version() -> String {
    "1.0".to_string()
}

impl Default for UtcpRegistryConfig {
    fn default() -> Self {
        Self {
            version: default_registry_version(),
            manuals: BTreeMap::new(),
            deregistered: BTreeMap::new(),
            staged: BTreeMap::new(),
            previous: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredManualEntry {
    pub name: String,
    pub source_url: String,
    pub cached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// A few words shown next to the manual's name in the installed skill's description, so agents
    /// know what an unclear name such as `context7` is for (see [`normalize_manual_hint`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// Longest hint [`normalize_manual_hint`] accepts, in characters. The hint is loaded into every
/// agent's context with the skill description, so it must stay a few words.
pub const MAX_MANUAL_HINT_CHARS: usize = 60;

/// Clean up a manual hint: whitespace runs (including newlines) become one space and the ends are
/// trimmed. Empty text means "no hint" (`None`). Other control characters and hints longer than
/// [`MAX_MANUAL_HINT_CHARS`] are rejected.
pub fn normalize_manual_hint(text: &str) -> Result<Option<String>> {
    if let Some(c) = text.chars().find(|c| c.is_control() && !c.is_whitespace()) {
        return Err(anyhow!(
            "The hint contains a control character ({:?}); use plain text",
            c
        ));
    }
    let hint = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if hint.is_empty() {
        return Ok(None);
    }
    let len = hint.chars().count();
    if len > MAX_MANUAL_HINT_CHARS {
        return Err(anyhow!(
            "The hint is {} characters; keep it to {} or fewer (a few words, e.g. \"library and framework docs\")",
            len,
            MAX_MANUAL_HINT_CHARS
        ));
    }
    Ok(Some(hint))
}

/// Result of [`set_manual_hint`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualHintUpdate {
    /// The hint as stored (`None` when cleared).
    pub hint: Option<String>,
    /// Whether the manual is active. Only active manuals appear in the skill description, so callers
    /// refresh installed skills only when this is `true`.
    pub active: bool,
}

/// Set or clear (`None` or empty text) the hint of a registered or deregistered manual, save the
/// registry and (for an active manual) rebuild the search index, since hint words are searchable.
/// Does not touch installed skills or the daemon: call
/// [`crate::skills::refresh_installed_global_skills`] afterwards when `active` is `true`, and tell a
/// running daemon to reload.
pub fn set_manual_hint(
    paths: &UtcpPaths,
    name: &str,
    text: Option<&str>,
) -> Result<ManualHintUpdate> {
    let hint = match text {
        Some(t) => normalize_manual_hint(t)?,
        None => None,
    };
    let _lock = paths.acquire_write_lock()?;
    let mut registry = UtcpRegistryConfig::load(paths)?;
    let (entry, active) = if let Some(e) = registry.manuals.get_mut(name) {
        (e, true)
    } else if let Some(e) = registry.deregistered.get_mut(name) {
        (e, false)
    } else {
        let known: Vec<&str> = registry
            .manuals
            .keys()
            .chain(registry.deregistered.keys())
            .map(String::as_str)
            .collect();
        return Err(anyhow!(
            "No manual named '{}' (known: {})",
            name,
            if known.is_empty() {
                "none".to_string()
            } else {
                known.join(", ")
            }
        ));
    };
    entry.hint = hint.clone();
    registry.save(paths)?;
    if active {
        rebuild_search_index_locked(paths, &registry)?;
    }
    Ok(ManualHintUpdate { hint, active })
}

/// Hints of all active manuals, keyed by manual name (manuals without a hint are left out).
pub fn manual_hints(paths: &UtcpPaths) -> BTreeMap<String, String> {
    UtcpRegistryConfig::load(paths)
        .map(|cfg| {
            cfg.manuals
                .into_iter()
                .filter_map(|(name, e)| e.hint.map(|h| (name, h)))
                .collect()
        })
        .unwrap_or_default()
}

/// Cached UTCP manual stored in `manuals/<name>.utcp.json` (compatible with `ManualV1`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedUtcpManual {
    pub manual_version: String,
    pub utcp_version: String,
    pub info: ManualInfo,
    #[serde(rename = "_meta")]
    pub meta: CachedManualMeta,
    pub tools: Vec<CachedToolEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedManualMeta {
    pub manual_name: String,
    pub source_url: String,
    pub sha256: String,
    pub fetched_at: String,
    pub tool_count: usize,
    pub protocols: Vec<String>,
    pub required_env_vars: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedToolEntry {
    pub name: String,
    pub description: String,
    pub inputs: Value,
    pub outputs: Value,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Normalized provider definition ready for direct execution (`http`, `websocket`, `cli`, etc.)
    pub provider: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_template: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportSummary {
    pub manual_name: String,
    pub output_path: String,
    pub source_url: String,
    pub sha256: String,
    pub tool_count: usize,
    pub protocols: Vec<String>,
    pub required_env_vars: Vec<String>,
}

/// One `utcp batch` item. Unknown keys are refused, so a misspelled `fields` or `where` can't be
/// ignored quietly; `id` defaults to the item's position (from 0).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchRequestItem {
    #[serde(default)]
    pub id: String,
    pub tool: String,
    #[serde(default, alias = "arguments")]
    pub args: HashMap<String, Value>,
    /// `--fields` paths for this item's answer: a list, or one string split at commas as on the
    /// command line (`"fields": "value.id,value.name"`).
    #[serde(
        default,
        deserialize_with = "fields_list",
        skip_serializing_if = "Option::is_none"
    )]
    pub fields: Option<Vec<String>>,
    /// `--where` conditions for this item's answer: a list (`"where": ["records.result=failed"]`)
    /// or one condition as a string.
    #[serde(
        default,
        rename = "where",
        deserialize_with = "where_list",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub filters: Vec<String>,
    /// Save this item's answer to this file (overwritten) instead of printing it, as `call --out`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out: Option<String>,
}

/// A batch item's `"fields"`: a list of paths, or one string of comma-separated paths.
fn fields_list<'de, D>(d: D) -> std::result::Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let strings = |v: &Value| match v {
        Value::String(s) => Some(
            s.split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect(),
        ),
        Value::Array(a) => a.iter().map(|x| x.as_str().map(str::to_string)).collect(),
        _ => None,
    };
    match Value::deserialize(d)? {
        Value::Null => Ok(None),
        v => strings(&v).map(Some).ok_or_else(|| {
            serde::de::Error::custom(
                "\"fields\" takes a list of paths or one string of them, as \"value.id,value.name\"",
            )
        }),
    }
}

/// A batch item's `"where"`: a list of conditions, or one condition as a string.
fn where_list<'de, D>(d: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Value::deserialize(d)? {
        Value::Null => Ok(Vec::new()),
        Value::String(s) => Ok(vec![s]),
        Value::Array(a) => a
            .iter()
            .map(|x| x.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                serde::de::Error::custom(
                    "\"where\" takes conditions as strings, as [\"status=failed\"]",
                )
            }),
        _ => Err(serde::de::Error::custom(
            "\"where\" takes a condition or a list of them, as [\"status=failed\"]",
        )),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchResultItem {
    pub id: String,
    pub tool: String,
    pub ok: bool,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// The arguments for the next page when `result` is one page of a longer list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_page: Option<Value>,
    /// What the caller should know about the call or its shaping: undeclared keys that were sent,
    /// a `where` condition that kept none of a list's items (with the values that field has on this
    /// page), or a `fields` path that matched nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub latency_ms: f64,
}

/// Milliseconds to two decimals, so a batch result prints `194.95`, not `194.94508199999999`.
fn round_ms(ms: f64) -> f64 {
    (ms * 100.0).round() / 100.0
}

impl BatchResultItem {
    /// A successful item; a page's next-page arguments and any `where` note go beside its `result`.
    pub fn succeeded(id: String, tool: String, value: Value, latency_ms: f64) -> Self {
        let shaped = crate::transfer::ShapedAnswer::from_value(value);
        Self {
            id,
            tool,
            ok: true,
            status: "OK".into(),
            result: Some(shaped.result),
            next_page: shaped.next_page,
            note: shaped.note,
            error: None,
            latency_ms: round_ms(latency_ms),
        }
    }

    pub fn failed(id: String, tool: String, error: String, latency_ms: f64) -> Self {
        Self {
            id,
            tool,
            ok: false,
            status: "ERR".into(),
            result: None,
            next_page: None,
            note: None,
            error: Some(error),
            latency_ms: round_ms(latency_ms),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub config_file: String,
    pub cache_dir: String,
    pub state_dir: String,
    pub socket_endpoint: String,
    pub daemon_running: bool,
    pub daemon_pid: Option<u32>,
    /// The running daemon's utcp version (`""` from daemons before 0.2.4); set by the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_version: Option<String>,
    pub logging_enabled: bool,
    pub cache_on_register: bool,
    pub index_present: bool,
    pub manual_count: usize,
    pub tool_count: usize,
    /// Manuals waiting in the staging area (`utcp list --staged`, `utcp diff`, `utcp promote`).
    #[serde(default)]
    pub staged_count: usize,
    /// The utcp `.env` file, and whether it exists.
    pub env_file: String,
    pub env_file_exists: bool,
    pub credentials: Vec<CredentialStatus>,
    /// The last run of `[secrets] command`; `None` when none is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets_command: Option<crate::secrets::SecretsCommandStatus>,
    /// Why `settings.toml` could not be used (utcp then runs with the default settings).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings_error: Option<String>,
    #[serde(default)]
    pub settings_file: String,
    /// Settings in `settings.toml` that utcp does not know, so they have no effect.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unknown_settings: Vec<crate::settings_file::UnknownSetting>,
    /// `max_upload_mb` / `max_download_mb`: `None` is no limit, `0` turned off.
    #[serde(default)]
    pub max_upload_mb: Option<u64>,
    #[serde(default)]
    pub max_download_mb: Option<u64>,
    /// Where file answers go without `--out`, and why there.
    #[serde(default)]
    pub download_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_dir_source: Option<crate::transfer::DownloadFolderSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialStatus {
    pub manual: String,
    pub env_var: String,
    pub resolved_via: Option<String>,
    pub is_set: bool,
    /// Where the value came from: [`SOURCE_SECRETS_COMMAND`], [`SOURCE_CALLER_SHELL`],
    /// [`SOURCE_DAEMON_ENV`] or [`SOURCE_ENV`]. `None` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// `manual` is a staged manual (`utcp stage`), not an active one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub staged: bool,
}

/// [`CredentialStatus::source`]: printed by `[secrets] command`.
pub const SOURCE_SECRETS_COMMAND: &str = "secrets command";
/// [`CredentialStatus::source`]: pushed to the daemon by `reload-env` or taken from a caller's shell.
pub const SOURCE_CALLER_SHELL: &str = "caller shell";
/// [`CredentialStatus::source`]: the daemon's own environment, `environment.d` or the utcp `.env`.
pub const SOURCE_DAEMON_ENV: &str = "daemon environment";
/// [`CredentialStatus::source`]: this process's environment, `environment.d` or the utcp `.env`.
pub const SOURCE_ENV: &str = "environment";

impl UtcpRegistryConfig {
    pub fn load(paths: &UtcpPaths) -> Result<Self> {
        if !paths.config_file.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(&paths.config_file).with_context(|| {
            format!("Failed to read config file {}", paths.config_file.display())
        })?;
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        let parsed: Self = serde_json::from_str(&raw).with_context(|| {
            format!(
                "Invalid JSON in registry config {}",
                paths.config_file.display()
            )
        })?;
        Ok(parsed)
    }

    pub fn save(&self, paths: &UtcpPaths) -> Result<()> {
        paths.ensure_dirs()?;
        let bytes = serde_json::to_vec_pretty(self)?;
        write_atomic(&paths.config_file, &bytes)
    }

    /// Make `entry` the active registration for `name`, dropping any kept deregistered entry.
    /// An entry without a hint keeps the hint of the registration it replaces.
    pub(crate) fn activate(&mut self, name: &str, mut entry: RegisteredManualEntry) {
        let previous = self.deregistered.remove(name);
        if entry.hint.is_none() {
            entry.hint = self
                .manuals
                .get(name)
                .or(previous.as_ref())
                .and_then(|e| e.hint.clone());
        }
        self.manuals.insert(name.to_string(), entry);
    }
}

/// Result of [`deregister_manual`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeregisterOutcome {
    /// Removed from the active registry; entry and saved copy kept for `register <name>`.
    Deregistered,
    /// Registry entry (active or deregistered) and saved copy deleted.
    Purged,
    /// Nothing by that name was registered, deregistered, or saved.
    NotFound,
}

/// A manual that exists on disk but is not in the active registry.
#[derive(Debug, Clone)]
pub struct UnregisteredManual {
    /// Metadata read from the saved copy (`tool_count` is 0 when the copy is missing or unreadable).
    pub manual: IndexedManual,
    /// `true` when the manual was deregistered and its registry settings were kept; `false` for a
    /// saved copy in `manuals/` with no registry entry at all.
    pub settings_kept: bool,
    /// Whether the saved copy exists and parses, i.e. whether offline re-registration can work.
    pub restorable: bool,
}

/// The variables a `utcp` process without the daemon resolves `${VAR}` with: [`load_file_env_map`],
/// overlaid with the secrets command's values for every name a registered manual may read (they win).
pub fn load_env_map(paths: &UtcpPaths) -> HashMap<String, String> {
    load_env_map_for(paths, None)
}

/// [`load_env_map`], asking the secrets command for `extra` names too (a manual being registered,
/// which is not in the index yet).
pub fn load_env_map_for(
    paths: &UtcpPaths,
    extra: Option<&BTreeSet<String>>,
) -> HashMap<String, String> {
    let mut map = load_file_env_map(paths);
    if let Some(run) = direct_secrets(paths, extra) {
        map.extend(run.values);
    }
    map
}

/// Runs the configured secrets command (through the per-process cache) for every name registered
/// manuals may read, plus `extra`. `None` when no command is configured or no name is wanted.
pub fn direct_secrets(
    paths: &UtcpPaths,
    extra: Option<&BTreeSet<String>>,
) -> Option<crate::secrets::SecretsRun> {
    let secrets = UtcpSettings::load(paths).secrets?;
    let mut wanted = load_or_build_index_sync(paths)
        .map(|index| index.env_var_names())
        .unwrap_or_default();
    if let Some(extra) = extra {
        wanted.extend(extra.iter().cloned());
    }
    run_direct_secrets(paths, &secrets, &wanted)
}

fn run_direct_secrets(
    paths: &UtcpPaths,
    secrets: &crate::paths::SecretsSettings,
    wanted: &BTreeSet<String>,
) -> Option<crate::secrets::SecretsRun> {
    (!wanted.is_empty()).then(|| crate::secrets::cached_run(paths, secrets, wanted))
}

/// Every name the secrets command could be asked for to fill the `${VAR}` placeholders in `value`
/// for `manual_name`.
fn placeholder_candidates(manual_name: &str, value: &Value) -> BTreeSet<String> {
    let mut vars = BTreeSet::new();
    collect_placeholders(value, &mut vars);
    vars.iter()
        .flat_map(|var| secret_var_candidates(manual_name, var))
        .collect()
}

/// The variables from files and the environment only (no secrets command): the process
/// environment, `~/.config/environment.d/*.conf` (on Unix) and the utcp `.env` (`paths.env_file`),
/// earlier sources winning.
pub fn load_file_env_map(paths: &UtcpPaths) -> HashMap<String, String> {
    let mut map: HashMap<String, String> = std::env::vars().collect();

    #[cfg(unix)]
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let env_d = home.join(".config/environment.d");
        if let Ok(entries) = fs::read_dir(env_d) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().and_then(|s| s.to_str()) == Some("conf") {
                    if let Ok(iter) = dotenvy::from_path_iter(&p) {
                        for item in iter.flatten() {
                            map.entry(item.0).or_insert(item.1);
                        }
                    }
                }
            }
        }
    }

    if paths.env_file.exists() {
        if let Ok(iter) = dotenvy::from_path_iter(&paths.env_file) {
            for item in iter.flatten() {
                map.entry(item.0).or_insert(item.1);
            }
        }
    }
    map
}

/// Load full startup environment for `utcp daemon`, additionally importing `systemctl --user show-environment`
/// on Linux when `systemd --user` is active.
pub fn load_daemon_startup_env_map(paths: &UtcpPaths) -> HashMap<String, String> {
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
    let mut map = load_file_env_map(paths);

    #[cfg(target_os = "linux")]
    if let Ok(out) = std::process::Command::new("systemctl")
        .args(["--user", "show-environment"])
        .output()
    {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    let key = k.trim();
                    if !key.is_empty() {
                        map.entry(key.to_string())
                            .or_insert_with(|| v.trim_matches('"').to_string());
                    }
                }
            }
        }
    }

    map
}

/// Check `systemd --user is-active utcp-daemon.service` on Linux for the Daemon Monitor view.
pub fn query_systemd_user_unit_state() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let out = std::process::Command::new("systemctl")
            .args(["--user", "is-active", "utcp-daemon.service"])
            .output()
            .ok()?;
        let state = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if state.is_empty() {
            None
        } else {
            Some(state)
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Every env var name that `resolve_secret_var(manual_name, var_name, ..)` may read, in lookup order
/// (may contain duplicates). Shared with `manual_env_var_names`, so the daemon's env allow-list
/// always matches what lookups can actually reach.
pub fn secret_var_candidates(manual_name: &str, var_name: &str) -> Vec<String> {
    let doubled_prefix = manual_name.replace('_', "__");
    let upper_prefix = manual_name.to_ascii_uppercase();
    let mut out = vec![var_name.to_string()];
    // If var_name is `MY_API_API_KEY`, extract suffix `API_KEY` and check `my__api_API_KEY`
    if let Some(suffix) = var_name.strip_prefix(&format!("{}_", upper_prefix)) {
        out.push(format!("{}_{}", doubled_prefix, suffix));
    }
    // Uppercase equivalent (e.g., `github_TOKEN` -> `GITHUB_TOKEN`, `my__api_API_KEY` -> `MY_API_API_KEY`)
    let collapsed_upper = var_name.replace("__", "_").to_ascii_uppercase();
    if collapsed_upper != var_name {
        out.push(collapsed_upper);
    }
    out.push(format!("{}_{}", doubled_prefix, var_name));
    out.push(format!("{}_{}", upper_prefix, var_name));
    out
}

/// Resolve a variable placeholder `${VAR}` supporting UTCP's doubled-underscore rule:
/// For manual `my_api`:
/// 1. Exact `VAR` match in `.env` / environment
/// 2. If `VAR` starts with `MY_API_`, also check `my__api_<SUFFIX>`
/// 3. Also check `<manual_escaped>_<VAR>` (`my__api_API_KEY`) and `<MANUAL_UPPER>_<VAR>`
pub fn resolve_secret_var<'a>(
    manual_name: &str,
    var_name: &str,
    env_map: &'a HashMap<String, String>,
) -> Option<(&'a str, String)> {
    secret_var_candidates(manual_name, var_name)
        .into_iter()
        .find_map(|candidate| match env_map.get(&candidate) {
            Some(val) if !val.is_empty() => Some((val.as_str(), candidate)),
            _ => None,
        })
}

/// The only env var names a set of loaded manuals can ever read: every name `resolve_secret_var`
/// tries for each `${VAR}` placeholder in a tool's provider/call template and for each declared
/// `required_env_vars` entry. `manuals` yields `(registered manual name, manual)`.
pub fn manual_env_var_names<'a>(
    manuals: impl IntoIterator<Item = (&'a str, &'a CachedUtcpManual)>,
) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for (manual_name, manual) in manuals {
        let mut vars: BTreeSet<String> = manual.meta.required_env_vars.iter().cloned().collect();
        for tool in &manual.tools {
            collect_placeholders(&tool.provider, &mut vars);
            if let Some(template) = &tool.tool_call_template {
                collect_placeholders(template, &mut vars);
            }
        }
        for var in &vars {
            names.extend(secret_var_candidates(manual_name, var));
        }
    }
    names
}

/// Drops every entry of `env_map` whose name is not in `allowed`.
pub fn retain_allowed_env(env_map: &mut HashMap<String, String>, allowed: &BTreeSet<String>) {
    env_map.retain(|k, _| allowed.contains(k));
}

/// Substitute `${VAR}` placeholders inside a JSON value using `env_map`. With `redact`, a set
/// variable shows as `***REDACTED:VAR***` and an unset one as `***UNSET:VAR***`; without it, an unset
/// placeholder stays as it is (see [`unset_placeholders_error`]).
pub fn interpolate_provider_vars(
    value: &mut Value,
    manual_name: &str,
    env_map: &HashMap<String, String>,
    redact: bool,
) {
    match value {
        Value::String(s) => {
            if s.contains("${") {
                let replaced = VAR_PLACEHOLDER_RE.replace_all(s, |caps: &regex::Captures| {
                    let var = &caps[1];
                    let resolved = resolve_secret_var(manual_name, var, env_map);
                    if redact {
                        let state = if resolved.is_some() {
                            "REDACTED"
                        } else {
                            "UNSET"
                        };
                        return format!("***{}:{}***", state, var);
                    }
                    resolved
                        .map(|(v, _)| v.to_string())
                        .unwrap_or_else(|| caps[0].to_string())
                });
                *s = replaced.into_owned();
            }
        }
        Value::Array(arr) => {
            for v in arr {
                interpolate_provider_vars(v, manual_name, env_map, redact);
            }
        }
        Value::Object(obj) => {
            for v in obj.values_mut() {
                interpolate_provider_vars(v, manual_name, env_map, redact);
            }
        }
        _ => {}
    }
}

fn collect_placeholders(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::String(s) => {
            for caps in VAR_PLACEHOLDER_RE.captures_iter(s) {
                out.insert(caps[1].to_string());
            }
        }
        Value::Array(arr) => {
            for v in arr {
                collect_placeholders(v, out);
            }
        }
        Value::Object(obj) => {
            for v in obj.values() {
                collect_placeholders(v, out);
            }
        }
        _ => {}
    }
}

fn sanitize_tool_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "tool".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Resolves `${VAR}` placeholders in spec-fetch headers (shell env, `~/.config/environment.d`, the
/// utcp `.env` and the secrets command). Returns the resolved headers and the placeholder names that
/// stayed unset.
fn resolve_fetch_headers(
    paths: &UtcpPaths,
    manual_name: &str,
    headers: &BTreeMap<String, String>,
) -> (BTreeMap<String, String>, BTreeSet<String>) {
    if !headers.values().any(|v| v.contains("${")) {
        return (headers.clone(), BTreeSet::new());
    }
    let mut value = serde_json::to_value(headers).unwrap_or_default();
    let env_map = load_env_map_for(paths, Some(&placeholder_candidates(manual_name, &value)));
    interpolate_provider_vars(&mut value, manual_name, &env_map, false);
    let mut unresolved = BTreeSet::new();
    collect_placeholders(&value, &mut unresolved);
    let resolved = serde_json::from_value(value).unwrap_or_else(|_| headers.clone());
    (resolved, unresolved)
}

/// Suffix for a failed fetch or discovery that names unset `${VAR}` placeholders (empty if none).
fn unset_placeholders_note(paths: &UtcpPaths, unresolved: &BTreeSet<String>) -> String {
    if unresolved.is_empty() {
        return String::new();
    }
    let names: Vec<&str> = unresolved.iter().map(String::as_str).collect();
    format!(
        " (these placeholders were not set: {}; add them to {} or export them, then retry)",
        names.join(", "),
        paths.env_file.display()
    )
}

/// A call refused before sending because `provider` (already interpolated) still holds `${VAR}`
/// placeholders: sending it would put the literal text where the secret goes, and the API's 401
/// or 404 would not say why. `None` when every placeholder was resolved.
fn unset_placeholders_error(manual_name: &str, provider: &Value) -> Option<anyhow::Error> {
    let mut unresolved = BTreeSet::new();
    collect_placeholders(provider, &mut unresolved);
    if unresolved.is_empty() {
        return None;
    }
    let names: Vec<&str> = unresolved.iter().map(String::as_str).collect();
    let it = if names.len() == 1 { "it" } else { "them" };
    Some(anyhow!(
        "these placeholders were not set: {} (manual {} needs {it}). Nothing was sent. Set {it} \
         where utcp reads secrets (its .env file or secrets command, or the environment), then \
         retry; utcp doctor shows where it looks",
        names.join(", "),
        manual_name,
    ))
}

/// Fetch raw bytes from either an HTTP(S) URL or a local file path. `${VAR}` placeholders in
/// `headers` are resolved for the request only; callers keep (and save) the placeholders.
pub async fn fetch_source_bytes(
    paths: &UtcpPaths,
    manual_name: &str,
    source: &str,
    headers: &BTreeMap<String, String>,
) -> Result<(Vec<u8>, String)> {
    if source.starts_with("http://") || source.starts_with("https://") {
        validate_url_security(source, false)?;
        let (headers, unresolved) = resolve_fetch_headers(paths, manual_name, headers);
        let note = unset_placeholders_note(paths, &unresolved);
        let client = build_secure_http_client();
        let mut req = client.get(source);
        for (k, v) in &headers {
            req = req.header(k, v);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("Failed to fetch {}{}", source, note))?;
        let resp = crate::transports::rate_limit::reject_rate_limited(resp).await?;
        if !resp.status().is_success() {
            return Err(anyhow!(
                "Failed to fetch {}: HTTP {}{}",
                source,
                resp.status(),
                note
            ));
        }
        let final_url = resp.url().to_string();
        let bytes = resp.bytes().await?.to_vec();
        Ok((bytes, final_url))
    } else {
        let path = Path::new(source);
        let canon = fs::canonicalize(path)
            .with_context(|| format!("Failed to locate local spec file '{}'", source))?;
        let bytes = fs::read(&canon)
            .with_context(|| format!("Failed to read spec file '{}'", canon.display()))?;
        Ok((bytes, canon.to_string_lossy().into_owned()))
    }
}

/// Convert raw OpenAPI (2.0/3.x JSON/YAML) or native UTCP manual bytes into a normalized `CachedUtcpManual`.
pub fn convert_bytes_to_cached_manual(
    bytes: &[u8],
    manual_name: &str,
    source_url: &str,
    base_url_override: Option<&str>,
    extra_headers: &BTreeMap<String, String>,
) -> Result<CachedUtcpManual> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let sha256 = format!("{:x}", hasher.finalize());
    let fetched_at = Utc::now().to_rfc3339();

    let mut parsed = parse_spec_bytes(bytes)?;

    // Check if already a UTCP Manual (`tools` array at root)
    if parsed
        .as_object()
        .and_then(|o| o.get("tools"))
        .and_then(|t| t.as_array())
        .is_some()
    {
        return normalize_utcp_manual_json(
            parsed,
            manual_name,
            source_url,
            &sha256,
            &fetched_at,
            base_url_override,
            extra_headers,
        );
    }

    // Otherwise treat as OpenAPI 2.0 / 3.x
    if let Some(base_override) = base_url_override {
        if let Some(obj) = parsed.as_object_mut() {
            obj.insert("servers".to_string(), json!([{ "url": base_override }]));
        }
    }

    let spec_url_opt = if source_url.starts_with("http://") || source_url.starts_with("https://") {
        Some(source_url.to_string())
    } else {
        None
    };

    let title = parsed
        .get("info")
        .and_then(|i| i.get("title"))
        .and_then(|v| v.as_str())
        .unwrap_or(manual_name)
        .to_string();
    let version = parsed
        .get("info")
        .and_then(|i| i.get("version"))
        .and_then(|v| v.as_str())
        .unwrap_or("1.0.0")
        .to_string();
    let description = parsed
        .get("info")
        .and_then(|i| i.get("description"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let converter = OpenApiConverter::new(parsed, spec_url_opt, Some(manual_name.to_string()));
    let converted = converter.convert();

    let mut tools = Vec::with_capacity(converted.tools.len());
    let mut protocols = BTreeSet::new();
    let mut env_vars = BTreeSet::new();

    for tool in converted.tools {
        let clean_name = sanitize_tool_name(&tool.name);
        let mut provider_val = tool.provider.unwrap_or_else(|| {
            json!({
                "name": manual_name,
                "provider_type": "http",
                "http_method": "GET",
                "url": base_url_override.unwrap_or("http://localhost")
            })
        });

        if !extra_headers.is_empty() {
            if let Some(pobj) = provider_val.as_object_mut() {
                let hdrs = pobj
                    .entry("headers")
                    .or_insert_with(|| Value::Object(serde_json::Map::new()));
                if let Some(hmap) = hdrs.as_object_mut() {
                    for (k, v) in extra_headers {
                        hmap.insert(k.clone(), Value::String(v.clone()));
                    }
                }
            }
        }

        collect_placeholders(&provider_val, &mut env_vars);
        let proto = extract_protocol_name(&provider_val);
        protocols.insert(proto);

        tools.push(CachedToolEntry {
            name: clean_name,
            description: tool.description,
            inputs: serde_json::to_value(&tool.inputs)?,
            outputs: serde_json::to_value(&tool.outputs)?,
            tags: tool.tags,
            provider: provider_val,
            tool_call_template: None,
        });
    }

    let doubled_vars = format_preferred_env_vars(manual_name, &env_vars);

    Ok(CachedUtcpManual {
        manual_version: "1.0.0".to_string(),
        utcp_version: "1.0.0".to_string(),
        info: ManualInfo {
            title,
            version,
            description,
        },
        meta: CachedManualMeta {
            manual_name: manual_name.to_string(),
            source_url: source_url.to_string(),
            sha256,
            fetched_at,
            tool_count: tools.len(),
            protocols: protocols.into_iter().collect(),
            required_env_vars: doubled_vars,
        },
        tools,
    })
}

fn format_preferred_env_vars(manual_name: &str, raw_vars: &BTreeSet<String>) -> Vec<String> {
    let upper_prefix = format!("{}_", manual_name.to_ascii_uppercase());
    let doubled_prefix = manual_name.replace('_', "__");
    raw_vars
        .iter()
        .map(|v| {
            if let Some(suffix) = v.strip_prefix(&upper_prefix) {
                format!("{}_{}", doubled_prefix, suffix)
            } else {
                v.clone()
            }
        })
        .collect()
}

fn normalize_utcp_manual_json(
    raw: Value,
    manual_name: &str,
    source_url: &str,
    sha256: &str,
    fetched_at: &str,
    base_url_override: Option<&str>,
    extra_headers: &BTreeMap<String, String>,
) -> Result<CachedUtcpManual> {
    let obj = raw
        .as_object()
        .ok_or_else(|| anyhow!("UTCP manual root must be a JSON object"))?;
    let tools_arr = obj
        .get("tools")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("UTCP manual missing 'tools' array"))?;

    let title = obj
        .get("info")
        .and_then(|i| i.get("title"))
        .and_then(|v| v.as_str())
        .unwrap_or(manual_name)
        .to_string();
    let version = obj
        .get("info")
        .and_then(|i| i.get("version"))
        .and_then(|v| v.as_str())
        .unwrap_or("1.0.0")
        .to_string();
    let description = obj
        .get("info")
        .and_then(|i| i.get("description"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let mut tools = Vec::with_capacity(tools_arr.len());
    let mut protocols = BTreeSet::new();
    let mut env_vars = BTreeSet::new();

    for tool_val in tools_arr {
        let t_obj = tool_val
            .as_object()
            .ok_or_else(|| anyhow!("Each tool entry must be a JSON object"))?;
        let raw_name = t_obj
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("Tool missing 'name'"))?;
        let short_name = raw_name
            .strip_prefix(&format!("{}.", manual_name))
            .unwrap_or(raw_name);
        let clean_name = sanitize_tool_name(short_name);

        let desc = t_obj
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let inputs = t_obj
            .get("inputs")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object"}));
        let outputs = t_obj
            .get("outputs")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object"}));
        let tags = t_obj
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        let mut provider_val = tool_to_provider(tool_val)?.unwrap_or_else(|| {
            json!({
                "name": manual_name,
                "provider_type": "http",
                "http_method": "GET",
                "url": base_url_override.unwrap_or("http://localhost")
            })
        });

        if let Some(pobj) = provider_val.as_object_mut() {
            pobj.insert("name".to_string(), Value::String(manual_name.to_string()));
            if let Some(base_override) = base_url_override {
                if let Some(url_str) = pobj.get("url").and_then(|v| v.as_str()) {
                    if url_str.starts_with('/') {
                        pobj.insert(
                            "url".to_string(),
                            Value::String(format!(
                                "{}/{}",
                                base_override.trim_end_matches('/'),
                                url_str.trim_start_matches('/')
                            )),
                        );
                    }
                }
            }
            if !extra_headers.is_empty() {
                let hdrs = pobj
                    .entry("headers")
                    .or_insert_with(|| Value::Object(serde_json::Map::new()));
                if let Some(hmap) = hdrs.as_object_mut() {
                    for (k, v) in extra_headers {
                        hmap.insert(k.clone(), Value::String(v.clone()));
                    }
                }
            }
        }

        collect_placeholders(&provider_val, &mut env_vars);
        let proto = extract_protocol_name(&provider_val);
        protocols.insert(proto);

        tools.push(CachedToolEntry {
            name: clean_name,
            description: desc,
            inputs,
            outputs,
            tags,
            provider: provider_val,
            tool_call_template: t_obj.get("tool_call_template").map(with_call_template_type),
        });
    }

    let explicit_vars: Vec<String> = env_vars.into_iter().collect();

    Ok(CachedUtcpManual {
        manual_version: "1.0.0".to_string(),
        utcp_version: "1.0.0".to_string(),
        info: ManualInfo {
            title,
            version,
            description,
        },
        meta: CachedManualMeta {
            manual_name: manual_name.to_string(),
            source_url: source_url.to_string(),
            sha256: sha256.to_string(),
            fetched_at: fetched_at.to_string(),
            tool_count: tools.len(),
            protocols: protocols.into_iter().collect(),
            required_env_vars: explicit_vars,
        },
        tools,
    })
}

pub fn extract_protocol_name(provider_val: &Value) -> String {
    provider_val
        .get("provider_type")
        .or_else(|| provider_val.get("call_template_type"))
        .or_else(|| provider_val.get("type"))
        .and_then(|v| v.as_str())
        .unwrap_or("http")
        .to_string()
}

/// Mode B ("Cache-Before-Register"): Convert any OpenAPI or UTCP source to a standalone `.utcp.json` file.
pub async fn import_manual_to_file(
    paths: &UtcpPaths,
    source: &str,
    manual_name: Option<&str>,
    output_path: Option<&Path>,
    base_url: Option<&str>,
    headers: &BTreeMap<String, String>,
) -> Result<ImportSummary> {
    let inferred_name = manual_name
        .map(str::to_string)
        .unwrap_or_else(|| infer_name_from_source(source));
    let (bytes, resolved_source) =
        fetch_source_bytes(paths, &inferred_name, source, headers).await?;
    let manual = convert_bytes_to_cached_manual(
        &bytes,
        &inferred_name,
        &resolved_source,
        base_url,
        headers,
    )?;

    let out_path = output_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(format!("{}.utcp.json", inferred_name)));
    let serialized = serde_json::to_vec_pretty(&manual)?;
    write_atomic(&out_path, &serialized)?;

    Ok(ImportSummary {
        manual_name: inferred_name,
        output_path: out_path.to_string_lossy().into_owned(),
        source_url: resolved_source,
        sha256: manual.meta.sha256,
        tool_count: manual.meta.tool_count,
        protocols: manual.meta.protocols,
        required_env_vars: manual.meta.required_env_vars,
    })
}

/// Default manual name for an OpenAPI/UTCP source: the first host label of a URL, or the file stem.
pub fn infer_name_from_source(source: &str) -> String {
    if let Ok(url) = url::Url::parse(source) {
        if let Some(host) = url.host_str() {
            return sanitize_tool_name(host.split('.').next().unwrap_or("manual"));
        }
    }
    Path::new(source)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.trim_end_matches(".utcp"))
        .map(sanitize_tool_name)
        .unwrap_or_else(|| "manual".to_string())
}

/// Default manual name for an MCP server.
///
/// URL: the first host label that is not a generic one (`mcp.context7.com` -> `context7`).
/// Command: the package or script a launcher runs (`npx -y @upstash/context7-mcp` -> `context7`),
/// else the command's own file name. Common `mcp-server-` / `-mcp` affixes are trimmed.
pub fn infer_mcp_manual_name(command: Option<&str>, args: &[String], url: Option<&str>) -> String {
    const GENERIC_HOST_LABELS: &[&str] = &["mcp", "api", "www", "server", "app"];
    const LAUNCHERS: &[&str] = &[
        "npx", "bunx", "pnpx", "uvx", "pipx", "uv", "pnpm", "bun", "node", "deno", "python",
        "python3", "docker", "podman",
    ];
    const SUBCOMMANDS: &[&str] = &["run", "dlx", "exec", "x", "tool"];

    let raw = if let Some(host) = url
        .and_then(|u| url::Url::parse(u).ok())
        .and_then(|u| u.host_str().map(str::to_string))
    {
        host.split('.')
            .find(|label| !GENERIC_HOST_LABELS.contains(label))
            .unwrap_or("mcp")
            .to_string()
    } else if let Some(cmd) = command {
        let base = Path::new(cmd)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(cmd);
        let target = if LAUNCHERS.contains(&base) {
            args.iter()
                .map(String::as_str)
                .find(|a| !a.starts_with('-') && !SUBCOMMANDS.contains(a))
                .unwrap_or(base)
        } else {
            base
        };
        let last = target.rsplit('/').next().unwrap_or(target);
        let unversioned = match last.get(1..).and_then(|rest| rest.find('@')) {
            Some(at) => &last[..at + 1],
            None => last,
        };
        let unversioned = unversioned.split(':').next().unwrap_or(unversioned);
        Path::new(unversioned)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(unversioned)
            .to_string()
    } else {
        "mcp".to_string()
    };

    let mut name = raw.to_ascii_lowercase().replace('-', "_");
    for prefix in ["mcp_server_", "server_", "mcp_"] {
        if let Some(rest) = name.strip_prefix(prefix) {
            if !rest.is_empty() {
                name = rest.to_string();
                break;
            }
        }
    }
    for suffix in ["_mcp_server", "_mcp", "_server"] {
        if let Some(rest) = name.strip_suffix(suffix) {
            if !rest.is_empty() {
                name = rest.to_string();
                break;
            }
        }
    }
    let name = sanitize_tool_name(&name);
    if name == "tool" {
        "mcp".to_string()
    } else {
        name
    }
}

/// How long `import-mcp` waits for a server to answer `initialize` + `tools/list`.
/// Generous because `npx`/`uvx` may download the server package on first run.
pub const MCP_DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

#[cfg(feature = "mcp")]
async fn discover_mcp_tools(
    provider: &crate::providers::mcp::McpProvider,
    timeout: std::time::Duration,
) -> Result<Vec<crate::tools::Tool>> {
    use crate::transports::ClientTransport;
    let transport = crate::transports::mcp::McpTransport::new();
    let outcome = tokio::time::timeout(timeout, transport.register_tool_provider(provider)).await;
    let _ = transport.deregister_tool_provider(provider).await;
    match outcome {
        Ok(result) => result,
        Err(_) => Err(anyhow!(
            "MCP server did not answer initialize + tools/list within {}s (is it an MCP server? \
             does it need a token or a first-run download?)",
            timeout.as_secs_f32()
        )),
    }
}

#[cfg(not(feature = "mcp"))]
async fn discover_mcp_tools(
    _provider: &crate::providers::mcp::McpProvider,
    _timeout: std::time::Duration,
) -> Result<Vec<crate::tools::Tool>> {
    Err(anyhow!(
        "this utcp was built without MCP support (cargo feature `mcp`)"
    ))
}

/// One local (stdio) MCP server a manual starts.
#[cfg(feature = "mcp")]
pub struct ManualStdioServer {
    /// The program as written in the manual (`${VAR}` not resolved). Never its arguments or
    /// environment, which may hold secrets.
    pub command: String,
    /// Ready to launch: `${VAR}` placeholders resolved from `env_map`.
    pub provider: crate::providers::mcp::McpProvider,
}

/// The local (stdio) MCP servers `manual` starts, one per server. Empty for manuals without one
/// (HTTP MCP, OpenAPI, ...).
#[cfg(feature = "mcp")]
pub fn manual_stdio_mcp_servers(
    manual: &CachedUtcpManual,
    manual_name: &str,
    env_map: &HashMap<String, String>,
) -> Vec<ManualStdioServer> {
    use crate::providers::mcp::McpProvider;
    let as_stdio = |value: Value| -> Option<McpProvider> {
        let provider = create_provider_from_value(value, 0).ok()?;
        let mcp = provider.as_any().downcast_ref::<McpProvider>()?;
        mcp.is_stdio().then(|| mcp.clone())
    };
    let mut seen = std::collections::HashSet::new();
    let mut servers = Vec::new();
    for entry in &manual.tools {
        let Some(raw) = as_stdio(entry.provider.clone()) else {
            continue;
        };
        // The transport keeps one server per provider name.
        if !seen.insert(raw.base.name.clone()) {
            continue;
        }
        let mut value = entry.provider.clone();
        if value.to_string().contains("${") {
            interpolate_provider_vars(&mut value, manual_name, env_map, false);
        }
        if let Some(provider) = as_stdio(value) {
            servers.push(ManualStdioServer {
                command: raw.command.unwrap_or_default(),
                provider,
            });
        }
    }
    servers
}

/// Discover tools from an MCP server (`--command` stdio or `--url` HTTP) via standard MCP
/// `initialize` + `tools/list`, save a standalone `.utcp.json` manual, and optionally register it.
#[allow(clippy::too_many_arguments)]
pub async fn import_mcp_manual(
    paths: &UtcpPaths,
    manual_name: &str,
    command: Option<&str>,
    args: Vec<String>,
    env_vars: BTreeMap<String, String>,
    url: Option<&str>,
    headers: BTreeMap<String, String>,
    output_path: Option<&Path>,
    also_register: bool,
    hint: Option<&str>,
) -> Result<ImportSummary> {
    let hint = match hint {
        Some(h) => normalize_manual_hint(h)?,
        None => None,
    };
    use crate::providers::base::{BaseProvider, ProviderType};
    use crate::providers::mcp::McpProvider;

    if command.is_none() && url.is_none() {
        return Err(anyhow!(
            "`utcp import-mcp` requires either `--command <cmd>` (stdio) or `--url <http_url>`"
        ));
    }
    if let Some(cmd) = command {
        if cmd.trim().contains(char::is_whitespace) && !Path::new(cmd).exists() {
            let mut parts = cmd.split_whitespace();
            let program = parts.next().unwrap_or_default();
            let hint: Vec<String> = parts.map(|p| format!("--arg {}", p)).collect();
            return Err(anyhow!(
                "--command takes only the program; pass each argument with --arg \
                 (try: --command {} {})",
                program,
                hint.join(" ")
            ));
        }
    }

    let source_label = if let Some(u) = url {
        format!("mcp+{}", u)
    } else {
        let cmd_str = command.unwrap_or_default();
        if args.is_empty() {
            format!("mcp+stdio://{}", cmd_str)
        } else {
            format!("mcp+stdio://{} {}", cmd_str, args.join(" "))
        }
    };

    let mcp_prov = McpProvider {
        base: BaseProvider {
            name: manual_name.to_string(),
            provider_type: ProviderType::Mcp,
            auth: None,
            allowed_communication_protocols: Some(vec!["mcp".to_string()]),
        },
        url: url.map(str::to_string),
        headers: if headers.is_empty() {
            None
        } else {
            Some(
                headers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            )
        },
        command: command.map(str::to_string),
        args: if args.is_empty() { None } else { Some(args) },
        env_vars: if env_vars.is_empty() {
            None
        } else {
            Some(
                env_vars
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            )
        },
    };

    let manual = discover_mcp_manual(paths, manual_name, &mcp_prov, &source_label).await?;
    let sha256 = manual.meta.sha256.clone();
    let doubled_vars = manual.meta.required_env_vars.clone();

    let serialized = serde_json::to_vec_pretty(&manual)?;
    let final_out_path = if also_register {
        let _lock = paths.acquire_write_lock()?;
        let cached_path = paths.cached_manual_path(manual_name);
        write_atomic(&cached_path, &serialized)?;
        if let Some(extra_out) = output_path {
            write_atomic(extra_out, &serialized)?;
        }
        let mut registry = UtcpRegistryConfig::load(paths)?;
        registry.activate(
            manual_name,
            RegisteredManualEntry {
                name: manual_name.to_string(),
                source_url: cached_path.to_string_lossy().into_owned(),
                cached: true,
                cached_file: Some(cached_path.to_string_lossy().into_owned()),
                base_url: None,
                headers: BTreeMap::new(),
                hint,
            },
        );
        registry.save(paths)?;
        rebuild_search_index_locked(paths, &registry)?;
        drop(_lock);
        crate::skills::refresh_installed_global_skills(paths);
        cached_path
    } else {
        let out = output_path
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(format!("{}.utcp.json", manual_name)));
        write_atomic(&out, &serialized)?;
        out
    };

    Ok(ImportSummary {
        manual_name: manual_name.to_string(),
        output_path: final_out_path.to_string_lossy().into_owned(),
        source_url: source_label,
        sha256,
        tool_count: manual.meta.tool_count,
        protocols: vec!["mcp".to_string()],
        required_env_vars: doubled_vars,
    })
}

/// Ask the MCP server `mcp_prov` for its tools (`initialize` + `tools/list`) and build the manual.
/// The provider keeps its `${VAR}` placeholders in the manual; discovery uses resolved values.
async fn discover_mcp_manual(
    paths: &UtcpPaths,
    manual_name: &str,
    mcp_prov: &crate::providers::mcp::McpProvider,
    source_label: &str,
) -> Result<CachedUtcpManual> {
    use crate::providers::mcp::McpProvider;
    // Discovery runs against a copy with `${VAR}` placeholders resolved (from the shell,
    // `~/.config/environment.d`, the utcp `.env` and the secrets command); the saved manual keeps
    // the placeholders.
    let mut resolved_json = serde_json::to_value(mcp_prov)?;
    let env_map = load_env_map_for(
        paths,
        Some(&placeholder_candidates(manual_name, &resolved_json)),
    );
    interpolate_provider_vars(&mut resolved_json, manual_name, &env_map, false);
    let mut unresolved = BTreeSet::new();
    collect_placeholders(&resolved_json, &mut unresolved);
    let discovery_prov: McpProvider = serde_json::from_value(resolved_json)?;

    let discovered = match discover_mcp_tools(&discovery_prov, MCP_DISCOVERY_TIMEOUT).await {
        Ok(tools) => tools,
        Err(e) if !unresolved.is_empty() => {
            return Err(anyhow!(
                "{:#}{}",
                e,
                unset_placeholders_note(paths, &unresolved)
            ));
        }
        Err(e) => return Err(e),
    };

    let provider_json = serde_json::to_value(mcp_prov)?;
    let call_template = with_call_template_type(&provider_json);
    let mut env_placeholders = BTreeSet::new();
    collect_placeholders(&provider_json, &mut env_placeholders);

    let mut tools = Vec::with_capacity(discovered.len());
    for t in discovered {
        let short = t
            .name
            .strip_prefix(&format!("{}.", manual_name))
            .unwrap_or(&t.name);
        let clean_name = sanitize_tool_name(short);
        tools.push(CachedToolEntry {
            name: clean_name,
            description: t.description,
            inputs: serde_json::to_value(&t.inputs)?,
            outputs: serde_json::to_value(&t.outputs)?,
            tags: if t.tags.is_empty() {
                vec!["mcp".to_string()]
            } else {
                t.tags
            },
            provider: provider_json.clone(),
            tool_call_template: Some(call_template.clone()),
        });
    }

    let raw_bytes_for_hash = serde_json::to_vec(&tools)?;
    let mut hasher = Sha256::new();
    hasher.update(&raw_bytes_for_hash);
    let sha256 = format!("{:x}", hasher.finalize());
    let fetched_at = Utc::now().to_rfc3339();
    let doubled_vars = format_preferred_env_vars(manual_name, &env_placeholders);

    Ok(CachedUtcpManual {
        manual_version: "1.0.0".to_string(),
        utcp_version: "1.0.0".to_string(),
        info: ManualInfo {
            title: format!("{} (MCP Bridge Manual)", manual_name),
            version: "1.0.0".to_string(),
            description: Some(format!("Discovered via MCP from {}", source_label)),
        },
        meta: CachedManualMeta {
            manual_name: manual_name.to_string(),
            source_url: source_label.to_string(),
            sha256,
            fetched_at,
            tool_count: tools.len(),
            protocols: vec!["mcp".to_string()],
            required_env_vars: doubled_vars,
        },
        tools,
    })
}

/// The MCP server an `import-mcp` manual was discovered from, recovered from its saved copy
/// (`_meta.source_url` starts with `mcp+`; every tool carries the server's provider, with its
/// `${VAR}` placeholders). `None` for other manuals.
fn saved_mcp_import(
    manual: &CachedUtcpManual,
    manual_name: &str,
) -> Option<Result<crate::providers::mcp::McpProvider>> {
    use crate::providers::mcp::McpProvider;
    if !manual.meta.source_url.starts_with("mcp+") {
        return None;
    }
    let found = manual.tools.iter().find_map(|t| {
        let provider = create_provider_from_value(t.provider.clone(), 0).ok()?;
        provider.as_any().downcast_ref::<McpProvider>().cloned()
    });
    Some(match found {
        Some(mut prov) => {
            prov.base.name = manual_name.to_string();
            Ok(prov)
        }
        None => Err(anyhow!(
            "'{}' has no saved MCP server to ask again; run `utcp import-mcp {} ... --register`",
            manual_name,
            manual_name
        )),
    })
}

/// Mode A ("Cache-on-Register"): Register a manual in `.utcp_config.json`, optionally cache `manuals/<name>.utcp.json`,
/// and atomically rebuild `search_index.bin`.
pub async fn register_manual(
    paths: &UtcpPaths,
    name: &str,
    source: &str,
    cache_override: Option<bool>,
    base_url: Option<&str>,
    headers: BTreeMap<String, String>,
    hint: Option<&str>,
) -> Result<ImportSummary> {
    let hint = match hint {
        Some(h) => normalize_manual_hint(h)?,
        None => None,
    };
    let _lock = paths.acquire_write_lock()?;
    let settings = UtcpSettings::load(paths);
    let should_cache = cache_override.unwrap_or(settings.cache_on_register);

    let (bytes, resolved_source) = fetch_source_bytes(paths, name, source, &headers).await?;
    let manual =
        convert_bytes_to_cached_manual(&bytes, name, &resolved_source, base_url, &headers)?;

    let cached_path = paths.cached_manual_path(name);
    // Always write cached manual file so both cached and live modes have immediate indexed metadata;
    // when `should_cache` is false, `entry.cached = false` signals live refresh eligibility.
    let serialized = serde_json::to_vec_pretty(&manual)?;
    write_atomic(&cached_path, &serialized)?;

    let mut registry = UtcpRegistryConfig::load(paths)?;
    registry.activate(
        name,
        RegisteredManualEntry {
            name: name.to_string(),
            source_url: resolved_source.clone(),
            cached: should_cache,
            cached_file: Some(cached_path.to_string_lossy().into_owned()),
            base_url: base_url.map(str::to_string),
            headers,
            hint,
        },
    );
    registry.save(paths)?;
    rebuild_search_index_locked(paths, &registry)?;
    drop(_lock);
    crate::skills::refresh_installed_global_skills(paths);

    Ok(ImportSummary {
        manual_name: name.to_string(),
        output_path: cached_path.to_string_lossy().into_owned(),
        source_url: resolved_source,
        sha256: manual.meta.sha256,
        tool_count: manual.meta.tool_count,
        protocols: manual.meta.protocols,
        required_env_vars: manual.meta.required_env_vars,
    })
}

/// Remove a manual from the active registry and rebuild the index.
///
/// Without `purge_cache` the entry moves to `deregistered` and its saved copy stays, so
/// [`reregister_manual`] can restore it offline. With `purge_cache` the entry (active or already
/// deregistered) and the saved copy are deleted; this also cleans up a saved copy with no entry.
pub fn deregister_manual(
    paths: &UtcpPaths,
    name: &str,
    purge_cache: bool,
) -> Result<DeregisterOutcome> {
    let _lock = paths.acquire_write_lock()?;
    let mut registry = UtcpRegistryConfig::load(paths)?;
    let active = registry.manuals.remove(name);

    if !purge_cache {
        let Some(entry) = active else {
            return Ok(DeregisterOutcome::NotFound);
        };
        registry.deregistered.insert(name.to_string(), entry);
        registry.save(paths)?;
        rebuild_search_index_locked(paths, &registry)?;
        drop(_lock);
        crate::skills::refresh_installed_global_skills(paths);
        return Ok(DeregisterOutcome::Deregistered);
    }

    let kept = registry.deregistered.remove(name);
    let staged = registry.staged.remove(name);
    let previous = registry.previous.remove(name);
    let mut files: Vec<PathBuf> = [active.as_ref(), kept.as_ref()]
        .into_iter()
        .flatten()
        .filter_map(|e| e.cached_file.as_ref().map(PathBuf::from))
        .collect();
    files.push(paths.cached_manual_path(name));
    files.push(paths.staged_manual_path(name));
    files.push(paths.previous_manual_path(name));
    let mut removed_file = false;
    for f in files {
        if fs::remove_file(&f).is_ok() {
            removed_file = true;
        }
    }

    if active.is_none() && kept.is_none() && staged.is_none() && previous.is_none() {
        return Ok(if removed_file {
            DeregisterOutcome::Purged
        } else {
            DeregisterOutcome::NotFound
        });
    }
    registry.save(paths)?;
    rebuild_search_index_locked(paths, &registry)?;
    drop(_lock);
    if active.is_some() {
        crate::skills::refresh_installed_global_skills(paths);
    }
    Ok(DeregisterOutcome::Purged)
}

/// Re-register a deregistered manual (or a saved copy in `manuals/` with no registry entry) from
/// its saved copy, without any network access. A kept entry keeps its source, base URL, and headers.
pub fn reregister_manual(paths: &UtcpPaths, name: &str) -> Result<ImportSummary> {
    let _lock = paths.acquire_write_lock()?;
    let mut registry = UtcpRegistryConfig::load(paths)?;
    if registry.manuals.contains_key(name) {
        return Err(anyhow!("Manual '{}' is already registered", name));
    }

    let entry = match registry.deregistered.get(name) {
        Some(e) => e.clone(),
        None => {
            let cf = paths.cached_manual_path(name);
            if !cf.exists() {
                drop(_lock);
                let available: Vec<String> = list_unregistered_manuals(paths)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|u| u.manual.name)
                    .collect();
                return Err(anyhow!(
                    "No deregistered manual named '{}' (available: {}). To add a new manual, pass a source: register <name> <url_or_file_path>",
                    name,
                    if available.is_empty() { "none".to_string() } else { available.join(", ") }
                ));
            }
            let cf = cf.to_string_lossy().into_owned();
            RegisteredManualEntry {
                name: name.to_string(),
                source_url: cf.clone(),
                cached: true,
                cached_file: Some(cf),
                base_url: None,
                headers: BTreeMap::new(),
                hint: None,
            }
        }
    };

    let cf_path = entry
        .cached_file
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| paths.cached_manual_path(name));
    let Some((_, manual)) = read_saved_manual(name, &entry, &cf_path) else {
        return Err(anyhow!(
            "The saved copy of '{}' ({}) is missing or unreadable; re-add it from its source: register {} {}",
            name,
            cf_path.display(),
            name,
            entry.source_url
        ));
    };

    let source_url = entry.source_url.clone();
    registry.activate(name, entry);
    registry.save(paths)?;
    rebuild_search_index_locked(paths, &registry)?;
    drop(_lock);
    crate::skills::refresh_installed_global_skills(paths);

    Ok(ImportSummary {
        manual_name: name.to_string(),
        output_path: cf_path.to_string_lossy().into_owned(),
        source_url,
        sha256: manual.meta.sha256,
        tool_count: manual.tools.len(),
        protocols: manual.meta.protocols,
        required_env_vars: manual.meta.required_env_vars,
    })
}

/// Manuals on disk that are not in the active registry: kept deregistered entries plus saved copies
/// in `manuals/` that no entry refers to. Sorted by name.
pub fn list_unregistered_manuals(paths: &UtcpPaths) -> Result<Vec<UnregisteredManual>> {
    let registry = UtcpRegistryConfig::load(paths)?;
    let entry_file = |name: &str, e: &RegisteredManualEntry| {
        e.cached_file
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| paths.cached_manual_path(name))
    };
    let mut known_files: BTreeSet<PathBuf> = BTreeSet::new();
    for (name, e) in registry.manuals.iter().chain(registry.deregistered.iter()) {
        known_files.insert(entry_file(name, e));
        known_files.insert(paths.cached_manual_path(name));
    }

    let mut out = Vec::new();
    for (name, entry) in &registry.deregistered {
        if registry.manuals.contains_key(name) {
            continue;
        }
        let cf = entry_file(name, entry);
        let (manual, restorable) = match read_saved_manual(name, entry, &cf) {
            Some((m, _)) => (m, true),
            None => (placeholder_indexed_manual(name, entry, &cf), false),
        };
        out.push(UnregisteredManual {
            manual,
            settings_kept: true,
            restorable,
        });
    }

    if let Ok(dir) = fs::read_dir(&paths.manuals_dir) {
        for item in dir.flatten() {
            let path = item.path();
            let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(name) = file_name.strip_suffix(".utcp.json") else {
                continue;
            };
            if name.is_empty()
                || known_files.contains(&path)
                || registry.manuals.contains_key(name)
                || registry.deregistered.contains_key(name)
            {
                continue;
            }
            let pseudo = RegisteredManualEntry {
                name: name.to_string(),
                source_url: path.to_string_lossy().into_owned(),
                cached: true,
                cached_file: Some(path.to_string_lossy().into_owned()),
                base_url: None,
                headers: BTreeMap::new(),
                hint: None,
            };
            if let Some((manual, _)) = read_saved_manual(name, &pseudo, &path) {
                out.push(UnregisteredManual {
                    manual,
                    settings_kept: false,
                    restorable: true,
                });
            }
        }
    }

    out.sort_by(|a, b| a.manual.name.cmp(&b.manual.name));
    Ok(out)
}

/// Read and parse a saved manual copy into its index summary.
pub(crate) fn read_saved_manual(
    name: &str,
    entry: &RegisteredManualEntry,
    cf_path: &Path,
) -> Option<(IndexedManual, CachedUtcpManual)> {
    let raw_bytes = fs::read(cf_path).ok()?;
    let manual = serde_json::from_slice::<CachedUtcpManual>(&raw_bytes).ok()?;
    let summary = IndexedManual {
        name: name.to_string(),
        source_url: entry.source_url.clone(),
        cached: entry.cached,
        cached_file: Some(cf_path.to_string_lossy().into_owned()),
        sha256: manual.meta.sha256.clone(),
        fetched_at: manual.meta.fetched_at.clone(),
        protocols: manual.meta.protocols.clone(),
        tool_count: manual.tools.len(),
        file_size_bytes: raw_bytes.len() as u64,
        required_env_vars: manual.meta.required_env_vars.clone(),
        env_var_names: manual_env_var_names([(name, &manual)])
            .into_iter()
            .collect(),
    };
    Some((summary, manual))
}

fn placeholder_indexed_manual(
    name: &str,
    entry: &RegisteredManualEntry,
    cf_path: &Path,
) -> IndexedManual {
    IndexedManual {
        name: name.to_string(),
        source_url: entry.source_url.clone(),
        cached: entry.cached,
        cached_file: Some(cf_path.to_string_lossy().into_owned()),
        sha256: String::new(),
        fetched_at: String::new(),
        protocols: Vec::new(),
        tool_count: 0,
        file_size_bytes: 0,
        required_env_vars: Vec::new(),
        env_var_names: Vec::new(),
    }
}

/// The names in `available` that `patterns` select, sorted and without repeats. A pattern is an exact name or a
/// prefix ending in `*` (`azdo_*`). No patterns select every name. A pattern that selects nothing is an error that
/// lists `available`, described as `what` ("staged", "registered").
pub fn select_manual_names<'a>(
    available: impl IntoIterator<Item = &'a String>,
    patterns: &[String],
    what: &str,
) -> Result<Vec<String>> {
    let available: BTreeSet<&String> = available.into_iter().collect();
    if patterns.is_empty() {
        return Ok(available.into_iter().cloned().collect());
    }
    let listed = || {
        if available.is_empty() {
            "none".to_string()
        } else {
            available
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        }
    };
    let mut out = BTreeSet::new();
    for pattern in patterns {
        let prefix = pattern.strip_suffix('*');
        if prefix.unwrap_or(pattern).contains('*') {
            return Err(anyhow!(
                "'{}': only a trailing * is supported, as in azdo_*",
                pattern
            ));
        }
        let matched: Vec<&&String> = match prefix {
            Some(p) => available.iter().filter(|n| n.starts_with(p)).collect(),
            None => available
                .iter()
                .filter(|n| n.as_str() == pattern.as_str())
                .collect(),
        };
        if matched.is_empty() {
            return Err(match prefix {
                Some(_) => anyhow!(
                    "No {} manual matches '{}' ({}: {})",
                    what,
                    pattern,
                    what,
                    listed()
                ),
                None => anyhow!("'{}' is not {} ({}: {})", pattern, what, what, listed()),
            });
        }
        out.extend(matched.into_iter().map(|n| (*n).clone()));
    }
    Ok(out.into_iter().collect())
}

/// Re-fetch and update the registered manuals `names` selects (all of them when empty; see
/// [`select_manual_names`]) and rebuild `search_index.bin`: from their recorded `source_url`, or for `import-mcp`
/// manuals by asking the MCP server again.
pub async fn refresh_manuals(paths: &UtcpPaths, names: &[String]) -> Result<Vec<ImportSummary>> {
    let _lock = paths.acquire_write_lock()?;
    let mut registry = UtcpRegistryConfig::load(paths)?;
    let names = select_manual_names(registry.manuals.keys(), names, "registered")?;

    let mut summaries = Vec::with_capacity(names.len());
    for name in names {
        let Some(entry) = registry.manuals.get(&name).cloned() else {
            continue;
        };
        let cached_path = entry
            .cached_file
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| paths.cached_manual_path(&name));
        // An `import-mcp` manual is refreshed by asking its MCP server again: its registered
        // source is only the saved copy.
        let saved = fs::read(&cached_path)
            .ok()
            .and_then(|b| serde_json::from_slice::<CachedUtcpManual>(&b).ok());
        let mcp_import = saved.as_ref().and_then(|m| saved_mcp_import(m, &name));
        let (manual, resolved_source) = match (mcp_import, saved) {
            (Some(prov), Some(saved)) => {
                let source = saved.meta.source_url;
                let manual = discover_mcp_manual(paths, &name, &prov?, &source)
                    .await
                    .with_context(|| format!("refreshing '{}'", name))?;
                (manual, source)
            }
            _ => {
                let (bytes, resolved_source) =
                    fetch_source_bytes(paths, &name, &entry.source_url, &entry.headers).await?;
                let manual = convert_bytes_to_cached_manual(
                    &bytes,
                    &name,
                    &resolved_source,
                    entry.base_url.as_deref(),
                    &entry.headers,
                )?;
                (manual, resolved_source)
            }
        };
        let serialized = serde_json::to_vec_pretty(&manual)?;
        write_atomic(&cached_path, &serialized)?;

        if let Some(mut_entry) = registry.manuals.get_mut(&name) {
            mut_entry.cached_file = Some(cached_path.to_string_lossy().into_owned());
        }

        summaries.push(ImportSummary {
            manual_name: name,
            output_path: cached_path.to_string_lossy().into_owned(),
            source_url: resolved_source,
            sha256: manual.meta.sha256,
            tool_count: manual.meta.tool_count,
            protocols: manual.meta.protocols,
            required_env_vars: manual.meta.required_env_vars,
        });
    }

    registry.save(paths)?;
    rebuild_search_index_locked(paths, &registry)?;
    Ok(summaries)
}

/// Rebuild `search_index.bin` from all registered manuals on disk.
pub fn rebuild_search_index(paths: &UtcpPaths) -> Result<SearchIndex> {
    let _lock = paths.acquire_write_lock()?;
    let registry = UtcpRegistryConfig::load(paths)?;
    rebuild_search_index_locked(paths, &registry)
}

pub(crate) fn rebuild_search_index_locked(
    paths: &UtcpPaths,
    registry: &UtcpRegistryConfig,
) -> Result<SearchIndex> {
    let index = build_index(paths, &registry.manuals);
    index.save_to_disk(paths)?;
    Ok(index)
}

/// [`rebuild_search_index`] without saving, from any set of entries (the active or the staged manuals).
pub(crate) fn build_index(
    paths: &UtcpPaths,
    entries: &BTreeMap<String, RegisteredManualEntry>,
) -> SearchIndex {
    let mut manuals = Vec::with_capacity(entries.len());
    let mut tools = Vec::new();

    for (name, entry) in entries {
        let cf_path = entry
            .cached_file
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| paths.cached_manual_path(name));
        let Some((indexed, manual)) = read_saved_manual(name, entry, &cf_path) else {
            continue;
        };

        for t in &manual.tools {
            let qualified_name = format!("{}.{}", name, t.name);
            let protocol = extract_protocol_name(&t.provider);
            let input_schema_json =
                serde_json::to_string(&t.inputs).unwrap_or_else(|_| r#"{"type":"object"}"#.into());
            let output_schema_json =
                serde_json::to_string(&t.outputs).unwrap_or_else(|_| r#"{"type":"object"}"#.into());
            let search_blob = format!(
                "{} {} {} {}",
                qualified_name.to_ascii_lowercase(),
                t.description.to_ascii_lowercase(),
                t.tags.join(" ").to_ascii_lowercase(),
                entry
                    .hint
                    .as_deref()
                    .unwrap_or_default()
                    .to_ascii_lowercase()
            );

            tools.push(IndexedTool {
                qualified_name,
                manual_name: name.clone(),
                tool_name: t.name.clone(),
                protocol,
                description: t.description.clone(),
                tags: t.tags.clone(),
                input_schema_json,
                output_schema_json,
                search_blob,
            });
        }

        manuals.push(indexed);
    }

    let built_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    SearchIndex {
        magic: SEARCH_INDEX_MAGIC,
        built_at_unix_ms,
        manuals,
        tools,
    }
}

/// Load `SearchIndex` synchronously in Direct Mode (rebuilding automatically if missing while `.utcp_config.json` exists).
pub fn load_or_build_index_sync(paths: &UtcpPaths) -> Result<SearchIndex> {
    if let Some(index) = SearchIndex::load_from_disk(paths)? {
        return Ok(index);
    }
    if paths.config_file.exists() {
        return rebuild_search_index(paths);
    }
    Ok(SearchIndex::default())
}

/// The active manuals' index or, with `staged`, an in-memory index of the staged manuals.
fn index_for(paths: &UtcpPaths, staged: bool) -> Result<SearchIndex> {
    if staged {
        crate::staging::staged_index(paths)
    } else {
        load_or_build_index_sync(paths)
    }
}

/// The error for a tool `schema` or `call` can't find; the CLI adds what to do about it.
pub fn tool_not_found(target: &str, staged: bool) -> String {
    if staged {
        format!("Tool '{target}' not found among the staged manuals")
    } else {
        format!("Tool '{target}' not found")
    }
}

pub fn list_tools_sync(
    paths: &UtcpPaths,
    manual: Option<&str>,
    staged: bool,
) -> Result<Vec<ToolSummary>> {
    let idx = index_for(paths, staged)?;
    Ok(idx.list_tools(manual))
}

/// With `staged`, searches as if every staged manual were promoted (see
/// [`SearchIndex::search_with_staged`]).
pub fn search_tools_sync(
    paths: &UtcpPaths,
    query: &str,
    limit: usize,
    include_schemas: bool,
    staged: bool,
) -> Result<Vec<ToolSummary>> {
    let idx = load_or_build_index_sync(paths)?;
    if staged {
        let staged_idx = crate::staging::staged_index(paths)?;
        return Ok(idx.search_with_staged(&staged_idx, query, limit, include_schemas));
    }
    Ok(idx.search(query, limit, include_schemas))
}

pub fn get_schema_sync(paths: &UtcpPaths, target: &str, staged: bool) -> Result<ToolSchemaDetail> {
    let idx = index_for(paths, staged)?;
    idx.get_schema(target)
        .ok_or_else(|| anyhow!("{}", tool_not_found(target, staged)))
}

/// Load one active manual's cached JSON (a small registry read, then one file from the OS page
/// cache). A manual the registry does not list as active (deregistered, or only staged) is refused
/// even when an old copy of its file is still on disk.
pub fn load_single_cached_manual(paths: &UtcpPaths, manual_name: &str) -> Result<CachedUtcpManual> {
    let registry = UtcpRegistryConfig::load(paths)?;
    load_registered_manual(paths, &registry, manual_name)
}

/// [`load_single_cached_manual`] with the registry already loaded.
pub fn load_registered_manual(
    paths: &UtcpPaths,
    registry: &UtcpRegistryConfig,
    manual_name: &str,
) -> Result<CachedUtcpManual> {
    let Some(entry) = registry.manuals.get(manual_name) else {
        if registry.staged.contains_key(manual_name) {
            return Err(anyhow!(
                "Manual '{}' is staged, not active yet",
                manual_name
            ));
        }
        return Err(anyhow!(
            "Manual '{}' is not registered (see the manuals with: utcp list)",
            manual_name
        ));
    };
    let direct_path = paths.cached_manual_path(manual_name);
    let file = match &entry.cached_file {
        Some(cf) if !direct_path.exists() => PathBuf::from(cf),
        _ => direct_path,
    };
    let bytes = fs::read(&file)
        .with_context(|| format!("Manual '{}' has no readable cached file", manual_name))?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Split `<manual>.<tool>` (or resolve unambiguous bare `<tool>` via `SearchIndex`).
pub fn resolve_manual_and_tool(
    target: &str,
    index_hint: Option<&SearchIndex>,
    paths: &UtcpPaths,
) -> Result<(String, String)> {
    if let Some((manual, tool)) = target.split_once('.') {
        if !manual.is_empty() && !tool.is_empty() {
            return Ok((manual.to_string(), tool.to_string()));
        }
    }
    if let Some(idx) = index_hint {
        if let Some(schema) = idx.get_schema(target) {
            let short = schema
                .name
                .strip_prefix(&format!("{}.", schema.manual))
                .unwrap_or(&schema.name)
                .to_string();
            return Ok((schema.manual, short));
        }
    } else {
        let idx = load_or_build_index_sync(paths)?;
        if let Some(schema) = idx.get_schema(target) {
            let short = schema
                .name
                .strip_prefix(&format!("{}.", schema.manual))
                .unwrap_or(&schema.name)
                .to_string();
            return Ok((schema.manual, short));
        }
    }
    Err(anyhow!(
        "Invalid or unknown tool '{}'. Expected '<manual>.<tool>'",
        target
    ))
}

/// Resolve a tool call into a dry-run preview JSON with all secrets redacted, and the note
/// [`crate::arguments::check_arguments`] has about the arguments.
pub fn dry_run_tool_from_manual(
    manual: &CachedUtcpManual,
    manual_name: &str,
    tool_name: &str,
    args: &HashMap<String, Value>,
    env_map: &HashMap<String, String>,
) -> Result<(Value, Option<String>)> {
    let entry = manual
        .tools
        .iter()
        .find(|t| t.name.eq_ignore_ascii_case(tool_name))
        .ok_or_else(|| anyhow!("Tool '{}.{}' not found", manual_name, tool_name))?;

    let mut provider_val = entry.provider.clone();
    interpolate_provider_vars(&mut provider_val, manual_name, env_map, true);
    let protocol = extract_protocol_name(&provider_val);
    let mut args = args.clone();
    let note = crate::arguments::check_arguments(
        &format!("{}.{}", manual_name, entry.name),
        &entry.inputs,
        &entry.provider,
        &mut args,
    )?;
    let args = &args;

    if protocol == "http" {
        if let Ok(http_prov) = serde_json::from_value::<HttpProvider>(provider_val.clone()) {
            let mut rem = args.clone();
            let (url, consumed) = crate::arguments::fill_url(&http_prov.url, &rem);
            for k in consumed {
                rem.remove(&k);
            }
            let mut headers = http_prov.headers.clone().unwrap_or_default();
            let mut rest = rem.clone();
            for field in http_prov.header_fields.iter().flatten() {
                if let Some(v) = rest.remove(field) {
                    let v = v
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| v.to_string());
                    headers.insert(field.clone(), v);
                }
            }
            let parts = crate::transports::http::plan_request(
                &format!("{}.{}", manual_name, entry.name),
                &http_prov,
                rest,
            )?;
            let mut preview = json!({
                "dry_run": true,
                "tool": format!("{}.{}", manual_name, entry.name),
                "protocol": "http",
                "method": http_prov.http_method,
                "url": url,
                "headers": (!headers.is_empty()).then_some(headers),
                "auth": http_prov.base.auth,
                "args": rem,
                "query": parts
                    .query
                    .into_iter()
                    .map(|(k, v)| (k, Value::String(v)))
                    .collect::<serde_json::Map<String, Value>>(),
            });
            {
                use crate::transports::http::RequestBody;
                match parts.body {
                    RequestBody::None => {}
                    RequestBody::Json(v) | RequestBody::Upload { spec: v, .. } => {
                        preview["body"] = v
                    }
                    RequestBody::Text(t) => preview["body"] = json!(t),
                }
            }
            if let Some(body_type) = &http_prov.body_type {
                preview["body_type"] = json!(body_type);
            }
            if let Some(response_type) = &http_prov.response_type {
                preview["response_type"] = json!(response_type);
            }
            return Ok((preview, note));
        }
    }

    Ok((
        json!({
            "dry_run": true,
            "tool": format!("{}.{}", manual_name, entry.name),
            "protocol": protocol,
            "provider": provider_val,
            "args": args,
        }),
        note,
    ))
}

/// Execute a single tool given an already-loaded `CachedUtcpManual`, `env_map`, and `CommunicationProtocolRegistry`.
/// Automatically intercepts inline base64 media payloads into `paths.media_dir` (`tmpfs` on Linux).
/// A file the tool uploads is read through `files`; a file it answers with comes back unread.
/// Beside the output comes the note [`crate::arguments::check_arguments`] has about the arguments.
#[allow(clippy::too_many_arguments)]
pub async fn execute_loaded_tool(
    manual: &CachedUtcpManual,
    manual_name: &str,
    tool_name: &str,
    args: HashMap<String, Value>,
    env_map: &HashMap<String, String>,
    protocols: &CommunicationProtocolRegistry,
    paths: &UtcpPaths,
    files: &dyn crate::transfer::CallerFiles,
) -> Result<(crate::transfer::CallOutput, Option<String>)> {
    let entry = manual
        .tools
        .iter()
        .find(|t| t.name.eq_ignore_ascii_case(tool_name))
        .ok_or_else(|| anyhow!("Tool '{}.{}' not found", manual_name, tool_name))?;

    let mut provider_val = entry.provider.clone();
    // Only touch env_map interpolation if `${` exists anywhere in provider JSON
    let raw_str = provider_val.to_string();
    if raw_str.contains("${") {
        interpolate_provider_vars(&mut provider_val, manual_name, env_map, false);
        if let Some(err) = unset_placeholders_error(manual_name, &provider_val) {
            return Err(err);
        }
    }
    let mut args = args;
    let note = crate::arguments::check_arguments(
        &format!("{}.{}", manual_name, entry.name),
        &entry.inputs,
        &entry.provider,
        &mut args,
    )?;

    let provider = create_provider_from_value(provider_val, 0)?;
    let proto_key = provider.type_().as_key();
    let transport = protocols
        .get(proto_key)
        .ok_or_else(|| anyhow!("Communication protocol '{}' is not enabled", proto_key))?;

    let call_name = format!("{}.{}", provider.name(), entry.name);
    let output = transport
        .call_tool_files(&call_name, args, provider.as_ref(), files)
        .await?;
    let output = match output {
        crate::transfer::CallOutput::Json(mut value) => {
            crate::media::intercept_inline_media(&mut value, paths);
            crate::transfer::CallOutput::Json(value)
        }
        crate::transfer::CallOutput::Page {
            mut value,
            next_page,
        } => {
            crate::media::intercept_inline_media(&mut value, paths);
            crate::transfer::CallOutput::Page { value, next_page }
        }
        crate::transfer::CallOutput::File(mut file) => {
            file.meta.tool = entry.name.clone();
            crate::transfer::CallOutput::File(file)
        }
    };
    Ok((output, note))
}

/// Release what a manual holds open in `protocols`, for example its stdio MCP server and that
/// server's child processes. The daemon calls this for manuals that a reload removed.
pub async fn release_manual_connections(
    manual: &CachedUtcpManual,
    protocols: &CommunicationProtocolRegistry,
) {
    let mut seen = std::collections::HashSet::new();
    for entry in &manual.tools {
        if !seen.insert(entry.provider.to_string()) {
            continue;
        }
        let Ok(provider) = create_provider_from_value(entry.provider.clone(), 0) else {
            continue;
        };
        if let Some(transport) = protocols.get(provider.type_().as_key()) {
            let _ = transport.deregister_tool_provider(provider.as_ref()).await;
        }
    }
}

/// The value a path starts at in `map`: its key, the value, and how many of `parts` the key used.
/// Keys may contain dots (Azure DevOps `fields.System.State`), so the longest run of parts that
/// names a key wins: `fields.System.State` is the key `System.State` under `fields`. Case is
/// ignored when no key matches exactly and exactly one key matches without case
/// (`Commit.Author.Name` finds `commit.author.name`).
fn lookup_key<'a, S: AsRef<str>>(
    map: &'a serde_json::Map<String, Value>,
    parts: &[S],
) -> Option<(&'a String, &'a Value, usize)> {
    let joined = |n: usize| {
        parts[..n]
            .iter()
            .map(AsRef::as_ref)
            .collect::<Vec<&str>>()
            .join(".")
    };
    let exact = (1..=parts.len())
        .rev()
        .find_map(|n| map.get_key_value(&joined(n)).map(|(k, v)| (k, v, n)));
    exact.or_else(|| {
        (1..=parts.len()).rev().find_map(|n| {
            let key = joined(n);
            let mut found = map.iter().filter(|(k, _)| k.eq_ignore_ascii_case(&key));
            match (found.next(), found.next()) {
                (Some((k, v)), None) => Some((k, v, n)),
                _ => None,
            }
        })
    })
}

/// The parts of a `--fields` path; empty parts (`a..b`, a trailing `.`) are dropped.
fn field_parts(spec: &str) -> Vec<&str> {
    spec.split('.').filter(|p| !p.is_empty()).collect()
}

/// Generic post-filter for verbose JSON API responses (`--fields sha,commit.message,html_url`).
/// - Returns `value` completely untouched when `fields` is empty.
/// - Supports both top-level keys (`sha`) and dotted nested paths (`commit.message`, `commit.author.name`).
///   A key that itself contains dots is matched whole (`fields.System.State`, see [`lookup_key`]).
/// - A path that reaches an array applies the rest of the path to every element
///   (`workflow_runs.id`, `assets.name`).
/// - A path that names no top-level key projects into the response's list: `items`, `results` or
///   `data`, else its only array of objects (`{ "total_count": N, "workflow_runs": [...] }`). With
///   only such paths, the rest of the answer stays; with top-level paths too, each path is resolved
///   on its own (`name,total_count` gives `{"total_count", "workflow_runs": [{"name"}]}`).
pub fn project_json_fields(value: Value, fields: &[String]) -> Value {
    let list = value.as_object().and_then(find_envelope_list_key);
    project_fields_into(value, fields, list)
}

/// `project_json_fields` with the response's list already found (`list`), so a list that `--where`
/// emptied is still the one projected.
fn project_fields_into(value: Value, fields: &[String], list: Option<String>) -> Value {
    let cleaned: Vec<&str> = fields
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if cleaned.is_empty() {
        return value;
    }

    match value {
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|elem| project_single_object(elem, &cleaned))
                .collect(),
        ),
        Value::Object(map) => {
            let (top, inner): (Vec<&str>, Vec<&str>) = cleaned
                .iter()
                .copied()
                .partition(|spec| lookup_key(&map, &field_parts(spec)).is_some());
            let list =
                list.filter(|key| !inner.is_empty() && map.get(key).is_some_and(Value::is_array));
            let Some(key) = list else {
                return project_single_object(Value::Object(map), &cleaned);
            };
            if top.is_empty() {
                // Only item fields: the list's items are projected, the rest of the answer stays.
                let mut map = map;
                if let Some(Value::Array(arr)) = map.remove(&key) {
                    let projected = arr
                        .into_iter()
                        .map(|elem| project_single_object(elem, &cleaned))
                        .collect();
                    map.insert(key, Value::Array(projected));
                }
                return Value::Object(map);
            }
            // Both: each path on its own. Top-level paths from the answer, the others from the
            // list's items (`name,total_count` gives `{"jobs": [{"name"}], "total_count"}`). The list
            // stays when it is empty or some item has one of those paths.
            let src = Value::Object(map);
            let mut out = Value::Object(serde_json::Map::new());
            for spec in &top {
                insert_projected_path(&src, &field_parts(spec), &mut out);
            }
            let items = src[&key].as_array().map(Vec::as_slice).unwrap_or_default();
            let item_has = |spec: &&str| {
                let parts = field_parts(spec);
                items
                    .iter()
                    .any(|i| path_found(i, &parts, 0, &mut (0, BTreeSet::new())))
            };
            if items.is_empty() || inner.iter().any(item_has) {
                for spec in &inner {
                    let mut parts = vec![key.as_str()];
                    parts.extend(field_parts(spec));
                    insert_projected_path(&src, &parts, &mut out);
                }
            }
            out
        }
        other => other,
    }
}

/// The list a collection response wraps: a well-known envelope key, else its only array of objects,
/// else (an empty page: every other top-level value is plain) its only empty array.
fn find_envelope_list_key(map: &serde_json::Map<String, Value>) -> Option<String> {
    for key in ["items", "results", "data"] {
        if matches!(map.get(key), Some(Value::Array(_))) {
            return Some(key.to_string());
        }
    }
    let only = |keep: fn(&[Value]) -> bool| {
        let mut lists = map
            .iter()
            .filter(|(_, v)| v.as_array().is_some_and(|a| keep(a)));
        match (lists.next(), lists.next()) {
            (Some((key, _)), None) => Some(key.clone()),
            _ => None,
        }
    };
    let has_object_list = map
        .values()
        .any(|v| v.as_array().is_some_and(|a| a.iter().any(Value::is_object)));
    if has_object_list {
        return only(|a| a.iter().any(Value::is_object));
    }
    let page_like = map.values().all(|v| match v {
        Value::Array(a) => a.is_empty(),
        other => !other.is_object(),
    });
    if page_like {
        only(|a| a.is_empty())
    } else {
        None
    }
}

fn project_single_object(val: Value, specs: &[&str]) -> Value {
    if !val.is_object() {
        return val;
    }
    let mut out = Value::Object(serde_json::Map::new());
    for spec in specs {
        insert_projected_path(&val, &field_parts(spec), &mut out);
    }
    out
}

/// Copy `parts` from `src` into `dst`. Objects descend by key; arrays apply the remaining path to every
/// element, so `dst` keeps the array's length and order. Scalars a path cannot descend into stay as is
/// inside arrays and are dropped elsewhere.
fn insert_projected_path(src: &Value, parts: &[&str], dst: &mut Value) {
    if parts.is_empty() {
        *dst = src.clone();
        return;
    }
    match src {
        Value::Object(src_map) => {
            if !dst.is_object() {
                *dst = Value::Object(serde_json::Map::new());
            }
            let Some((key, found, used)) = lookup_key(src_map, parts) else {
                return;
            };
            let rest = &parts[used..];
            if !rest.is_empty() && !(found.is_object() || found.is_array()) {
                return;
            }
            if let Value::Object(dst_map) = dst {
                let slot = dst_map.entry(key.clone()).or_insert(Value::Null);
                insert_projected_path(found, rest, slot);
            }
        }
        Value::Array(items) => {
            if !matches!(dst, Value::Array(existing) if existing.len() == items.len()) {
                *dst = Value::Array(vec![Value::Null; items.len()]);
            }
            if let Value::Array(dst_items) = dst {
                for (item, slot) in items.iter().zip(dst_items.iter_mut()) {
                    if item.is_object() || item.is_array() {
                        insert_projected_path(item, parts, slot);
                    } else {
                        *slot = item.clone();
                    }
                }
            }
        }
        _ => {}
    }
}

/// How a JSON answer is cut down before it is printed: `--where` keeps the list items that match,
/// then `--fields` projects what is left.
#[derive(Debug, Clone, Copy, Default)]
pub struct AnswerShape<'a> {
    pub filters: &'a [String],
    pub fields: Option<&'a [String]>,
    /// A note about the call itself (from [`crate::arguments::check_arguments`]), printed before
    /// the shaping notes.
    pub note: Option<&'a str>,
}

impl<'a> AnswerShape<'a> {
    /// This shape with the call's own note.
    pub fn with_note<'b>(self, note: Option<&'b str>) -> AnswerShape<'b>
    where
        'a: 'b,
    {
        AnswerShape { note, ..self }
    }

    /// The shaped answer, and a note when the call had one, a `--where` condition kept none of a
    /// list's items, or a `--fields` path matched nothing. It never fails: the call was already
    /// sent (a write has happened), so a `--where` that can't apply to this answer (one object, a
    /// list of plain values, text) leaves the answer unfiltered, with a note saying why.
    pub fn apply(&self, value: Value) -> (Value, Option<String>) {
        let fields = self.fields.filter(|f| !f.is_empty());
        // Found before `--where`, which may leave the list empty.
        let list = fields
            .and(value.as_object())
            .and_then(find_envelope_list_key);
        let (value, filter_note) = if self.filters.iter().all(|f| f.trim().is_empty()) {
            (value, None)
        } else {
            let unfiltered = value.clone();
            match filter_json_items(value, self.filters) {
                Ok(filtered) => filtered,
                Err(e) => (unfiltered, Some(where_not_applied(&e))),
            }
        };
        let (value, projection_note) = match fields {
            Some(f) => {
                let list = list.or_else(|| value.as_object().and_then(find_envelope_list_key));
                let note = fields_note(&value, f, list.as_deref());
                (project_fields_into(value, f, list), note)
            }
            None => (value, None),
        };
        let notes: Vec<String> = self
            .note
            .map(str::to_string)
            .into_iter()
            .chain(filter_note)
            .chain(projection_note)
            .collect();
        (value, (!notes.is_empty()).then(|| notes.join("; ")))
    }

    /// The note for an answer that is a file: the call's own note, and that `--where` and `--fields`
    /// were not applied, since they shape JSON answers.
    pub fn file_note(&self) -> Option<String> {
        let given: Vec<&str> = [
            (self.filters.iter().any(|f| !f.trim().is_empty()), "--where"),
            (self.fields.is_some_and(|f| !f.is_empty()), "--fields"),
        ]
        .into_iter()
        .filter_map(|(given, flag)| given.then_some(flag))
        .collect();
        let shape_note = (!given.is_empty()).then(|| {
            format!(
                "the call succeeded; {} not applied: the answer is a file, and they shape JSON answers",
                if given.len() == 1 {
                    format!("{} was", given[0])
                } else {
                    format!("{} were", given.join(" and "))
                }
            )
        });
        let notes: Vec<String> = self
            .note
            .map(str::to_string)
            .into_iter()
            .chain(shape_note)
            .collect();
        (!notes.is_empty()).then(|| notes.join("; "))
    }

    /// A dry run's request description, with a note when `--fields` or `--where` were given: a dry
    /// run has no answer for them to shape.
    pub fn for_dry_run(&self, request: Value) -> Value {
        let given: Vec<&str> = [
            (self.fields.is_some_and(|f| !f.is_empty()), "--fields"),
            (!self.filters.is_empty(), "--where"),
        ]
        .into_iter()
        .filter_map(|(given, flag)| given.then_some(flag))
        .collect();
        let shape_note =
            (!given.is_empty()).then(|| {
                format!(
                "a dry run sends nothing, so {} had no answer to shape; {} apply to the real call",
                given.join(" and "),
                if given.len() == 1 { "it would" } else { "they would" }
            )
            });
        let notes: Vec<String> = self
            .note
            .map(str::to_string)
            .into_iter()
            .chain(shape_note)
            .collect();
        if notes.is_empty() {
            return request;
        }
        crate::transfer::ShapedAnswer {
            result: request,
            next_page: None,
            note: Some(notes.join("; ")),
        }
        .into_value()
    }
}

/// The note for a `--where` that could not apply to the answer it got: the call itself succeeded,
/// so its answer keeps all its items, never dropped (it may have been a write).
fn where_not_applied(e: &anyhow::Error) -> String {
    let reason = format!("{e:#}");
    let reason = reason.strip_prefix("--where ").unwrap_or(&reason);
    format!("the call succeeded; --where was not applied, so no items were left out: {reason}")
}

/// Why `--fields` left something out: the paths that matched nothing in `value` (with the keys found
/// where each stopped), or that the answer is text. `None` when every path matched, or when there is
/// nothing to look in (an empty page).
fn fields_note(value: &Value, fields: &[String], list: Option<&str>) -> Option<String> {
    let specs: Vec<&str> = fields
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if specs.is_empty() {
        return None;
    }
    if value.is_string() {
        return Some("--fields works on JSON answers; this answer is text, shown whole".into());
    }
    if value.as_array().is_some_and(Vec::is_empty) {
        return None;
    }
    // The same base `project_fields_into` projects each path against: the answer for a top-level
    // path, else the answer's list.
    let list = match (value, list) {
        (Value::Object(map), Some(list)) => {
            map.get(list).filter(|l| l.is_array()).map(|l| (map, l))
        }
        _ => None,
    };
    let mut missed = Vec::new();
    for spec in specs {
        let parts = field_parts(spec);
        let base = match list {
            Some((map, list)) if lookup_key(map, &parts).is_none() => list,
            _ => value,
        };
        if base.as_array().is_some_and(Vec::is_empty) {
            continue;
        }
        let mut stop = (0, BTreeSet::new());
        if path_found(base, &parts, 0, &mut stop) {
            continue;
        }
        let (depth, keys) = stop;
        let where_ = if depth == 0 {
            "keys here".to_string()
        } else {
            format!("keys under {}", parts[..depth].join("."))
        };
        missed.push(if keys.is_empty() {
            format!("--fields '{spec}' matched nothing")
        } else {
            format!(
                "--fields '{spec}' matched nothing ({where_}: {})",
                capped_list(keys, 15)
            )
        });
    }
    (!missed.is_empty()).then(|| missed.join("; "))
}

/// Whether `parts` reaches a value in `v` (through lists, in any item). When it doesn't, `stop`
/// holds the deepest level the path reached and the keys found there.
fn path_found(
    v: &Value,
    parts: &[&str],
    depth: usize,
    stop: &mut (usize, BTreeSet<String>),
) -> bool {
    if parts.is_empty() {
        return true;
    }
    match v {
        Value::Object(map) => {
            if let Some((_, child, used)) = lookup_key(map, parts) {
                return path_found(child, &parts[used..], depth + used, stop);
            }
            if depth > stop.0 {
                *stop = (depth, BTreeSet::new());
            }
            if depth == stop.0 {
                stop.1.extend(map.keys().cloned());
            }
            false
        }
        // An empty list (often one `--where` emptied) has nothing to check the path against.
        Value::Array(items) if items.is_empty() => true,
        // `project_fields_into` keeps plain values inside lists as they are.
        Value::Array(items) => {
            let mut found = items.iter().any(|i| !(i.is_object() || i.is_array()));
            for item in items {
                found |= path_found(item, parts, depth, stop);
            }
            found
        }
        _ => false,
    }
}

/// `a, b, c` with at most `cap` names, then `and N more`.
fn capped_list(names: BTreeSet<String>, cap: usize) -> String {
    let total = names.len();
    let mut shown: Vec<String> = names.into_iter().take(cap).collect();
    if total > cap {
        shown.push(format!("and {} more", total - cap));
    }
    shown.join(", ")
}

/// One `--where` condition: `path=a|b` keeps the list items whose field at `path` is `a` or `b`,
/// `path~=a|b` those whose field contains `a` or `b`, `path^=a|b` those whose field starts with one;
/// a `!` before the operator (`!=`, `!~=`, `!^=`) keeps the others. Case is ignored in values (and
/// in path parts that match no key exactly), `\|` is a `|`
/// inside a value, and a missing field counts as `null`. With `=`, a date alone matches any time
/// that day, in UTC. `path>v`, `<`, `>=` and `<=` compare
/// numbers, or dates (a date alone compares whole days, in UTC); a field that isn't one is left out.
#[derive(Debug, Clone, PartialEq)]
pub struct WhereCondition {
    path: Vec<String>,
    values: Vec<String>,
    op: WhereOp,
    negate: bool,
    /// The value of a comparison, parsed.
    bound: Option<Bound>,
}

/// How a `--where` value is compared with a field: as text, ignoring case (`=`, `~=`, `^=`), or as a
/// number or date (`>`, `>=`, `<`, `<=`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WhereOp {
    Equals,
    Contains,
    StartsWith,
    Greater,
    GreaterEq,
    Less,
    LessEq,
}

impl WhereOp {
    fn matches_text(self, text: &str, wanted: &str) -> bool {
        let (text, wanted) = (text.to_lowercase(), wanted.to_lowercase());
        match self {
            WhereOp::Equals => text == wanted,
            WhereOp::Contains => text.contains(&wanted),
            WhereOp::StartsWith => text.starts_with(&wanted),
            _ => false,
        }
    }

    fn holds(self, order: std::cmp::Ordering) -> bool {
        match self {
            WhereOp::Greater => order.is_gt(),
            WhereOp::GreaterEq => order.is_ge(),
            WhereOp::Less => order.is_lt(),
            WhereOp::LessEq => order.is_le(),
            _ => false,
        }
    }

    fn is_comparison(self) -> bool {
        matches!(
            self,
            WhereOp::Greater | WhereOp::GreaterEq | WhereOp::Less | WhereOp::LessEq
        )
    }

    fn symbol(self) -> &'static str {
        match self {
            WhereOp::Equals => "=",
            WhereOp::Contains => "~=",
            WhereOp::StartsWith => "^=",
            WhereOp::Greater => ">",
            WhereOp::GreaterEq => ">=",
            WhereOp::Less => "<",
            WhereOp::LessEq => "<=",
        }
    }
}

/// What a comparison compares fields with.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Bound {
    Number(f64),
    /// A date with a time: fields compare as instants.
    Instant(chrono::DateTime<Utc>),
    /// A date alone: fields compare by their day, in UTC.
    Day(chrono::NaiveDate),
}

/// A date or date and time read from text.
enum When {
    Instant(chrono::DateTime<Utc>),
    Day(chrono::NaiveDate),
}

impl Bound {
    fn parse(text: &str) -> Option<Self> {
        if let Some(n) = parse_number(text) {
            return Some(Bound::Number(n));
        }
        Some(match parse_when(text)? {
            When::Instant(t) => Bound::Instant(t),
            When::Day(d) => Bound::Day(d),
        })
    }

    fn kind(&self) -> &'static str {
        match self {
            Bound::Number(_) => "a number",
            _ => "a date",
        }
    }

    /// How the field value `v` compares with the bound; `None` when it is not a number (for a
    /// number) or a date (for a date). Numbers written as strings count.
    fn compare(&self, v: &Value) -> Option<std::cmp::Ordering> {
        match self {
            Bound::Number(bound) => {
                let n = match v {
                    Value::Number(n) => n.as_f64()?,
                    Value::String(s) => parse_number(s)?,
                    _ => return None,
                };
                n.partial_cmp(bound)
            }
            Bound::Instant(bound) => Some(match parse_when(v.as_str()?)? {
                When::Instant(t) => t.cmp(bound),
                When::Day(d) => d.and_time(chrono::NaiveTime::MIN).and_utc().cmp(bound),
            }),
            Bound::Day(bound) => Some(match parse_when(v.as_str()?)? {
                When::Instant(t) => t.date_naive().cmp(bound),
                When::Day(d) => d.cmp(bound),
            }),
        }
    }
}

/// A JSON number (`12`, `-3.5`, `1e3`); `NaN`, `inf` and `007` are not.
fn parse_number(text: &str) -> Option<f64> {
    text.trim().parse::<serde_json::Number>().ok()?.as_f64()
}

/// An ISO date: with a time and zone (`2026-11-24T10:00:00Z`, `+01:00`), with a time and no zone
/// (UTC), or a date alone (`2026-11-24`).
fn parse_when(text: &str) -> Option<When> {
    let text = text.trim();
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(When::Instant(t.with_timezone(&Utc)));
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(t) = chrono::NaiveDateTime::parse_from_str(text, format) {
            return Some(When::Instant(t.and_utc()));
        }
    }
    chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .ok()
        .map(When::Day)
}

impl WhereCondition {
    pub fn parse(spec: &str) -> Result<Self> {
        let spec = spec.trim();
        let Some(at) = spec.find(['=', '<', '>']) else {
            return Err(anyhow!(
                "--where '{spec}' has no operator: write path=value[|value...] (equals), path~=value \
                 (contains) or path^=value (starts with), where a '!' before the operator (!=, !~=, !^=) \
                 keeps the others; or compare a number or date with >, >=, < or <="
            ));
        };
        let (path, rest, op, negate) = if spec[at..].starts_with('=') {
            let (path, op) = match spec[..at].strip_suffix('~') {
                Some(path) => (path, WhereOp::Contains),
                None => match spec[..at].strip_suffix('^') {
                    Some(path) => (path, WhereOp::StartsWith),
                    None => (&spec[..at], WhereOp::Equals),
                },
            };
            match path.strip_suffix('!') {
                Some(path) => (path, &spec[at + 1..], op, true),
                None => (path, &spec[at + 1..], op, false),
            }
        } else {
            let or_equal = spec[at + 1..].starts_with('=');
            let op = match (&spec[at..at + 1], or_equal) {
                (">", false) => WhereOp::Greater,
                (">", true) => WhereOp::GreaterEq,
                ("<", false) => WhereOp::Less,
                _ => WhereOp::LessEq,
            };
            let path = &spec[..at];
            if let Some(c) = path.chars().last().filter(|c| matches!(c, '!' | '~' | '^')) {
                return Err(anyhow!(
                    "--where '{spec}': '{c}{}' is not an operator; compare with >, >=, < or <= \
                     (the opposite of > is <=)",
                    op.symbol()
                ));
            }
            (path, &spec[at + 1 + usize::from(or_equal)..], op, false)
        };
        if path.trim().is_empty() {
            return Err(anyhow!(
                "--where '{spec}' names no field before the '{}'",
                op.symbol()
            ));
        }
        let path: Vec<String> = path.split('.').map(|p| p.trim().to_string()).collect();
        if path.iter().any(String::is_empty) {
            return Err(anyhow!(
                "--where '{spec}': the path has an empty part; separate field names with one '.'"
            ));
        }
        if rest.starts_with('=') {
            return Err(anyhow!(
                "--where '{spec}': write one '=', as in {}{}{}",
                path.join("."),
                op.symbol(),
                rest.trim_start_matches('=').trim()
            ));
        }
        if op == WhereOp::Equals && !negate {
            if let Some(turned) = ["<", ">"].into_iter().find(|c| rest.starts_with(c)) {
                if Bound::parse(&rest[1..]).is_some() {
                    return Err(anyhow!(
                        "--where '{spec}': write {}{turned}={} to compare",
                        path.join("."),
                        rest[1..].trim()
                    ));
                }
            }
        }
        let values = split_where_values(rest);
        let mut bound = None;
        if op.is_comparison() {
            let [value] = values.as_slice() else {
                return Err(anyhow!(
                    "--where '{spec}': {} compares with one value; write one condition for each limit",
                    op.symbol()
                ));
            };
            bound = Some(Bound::parse(value).ok_or_else(|| {
                anyhow!(
                    "--where '{spec}': {} compares numbers or dates (2026-11-24, 2026-11-24T10:00:00Z), \
                     and '{value}' is neither",
                    op.symbol()
                )
            })?);
        } else if op != WhereOp::Equals && values.iter().any(String::is_empty) {
            return Err(anyhow!(
                "--where '{spec}': an empty value would match everything with {}; remove the stray '|'",
                op.symbol()
            ));
        }
        Ok(Self {
            path,
            values,
            op,
            negate,
            bound,
        })
    }

    /// Filters the list this condition names: the root array; the top-level array its path starts
    /// with (`records.result`); else the answer's list (see `list_key`). A path that is only a list's
    /// name, or starts with any other top-level field, is an error: that field belongs to the answer,
    /// not to the list's items. A condition that keeps none of a non-empty list's items adds a note.
    fn apply(&self, value: Value, notes: &mut Vec<String>) -> Result<Value> {
        match value {
            Value::Array(items) => {
                self.check_items("the answer", &items)?;
                Ok(Value::Array(self.keep(
                    items,
                    &self.path,
                    "the answer",
                    notes,
                )))
            }
            Value::Object(mut map) => {
                let (key, path) = match lookup_key(&map, &self.path) {
                    Some((key, Value::Array(_), used)) if used < self.path.len() => {
                        (key.clone(), &self.path[used..])
                    }
                    Some((key, Value::Array(items), _)) => {
                        return Err(self.list_name_error(key, items))
                    }
                    None => (self.list_key(&map)?, &self.path[..]),
                    Some((key, _, _)) => return Err(self.answer_field_error(key, &map)),
                };
                if let Some(Value::Array(items)) = map.remove(&key) {
                    self.check_items(&key, &items)?;
                    let kept = self.keep(items, path, &key, notes);
                    map.insert(key, Value::Array(kept));
                }
                Ok(Value::Object(map))
            }
            _ => Err(anyhow!(
                "--where '{}': the answer is not a JSON list or object",
                self.spec()
            )),
        }
    }

    /// The answer's list: its `value`, `items`, `results` or `data` list, else its only list of
    /// objects, else (an empty page: every other top-level value is plain) its only empty list.
    /// Lists of strings or numbers don't count.
    fn list_key(&self, map: &serde_json::Map<String, Value>) -> Result<String> {
        for key in ["value", "items", "results", "data"] {
            if matches!(map.get(key), Some(Value::Array(_))) {
                return Ok(key.to_string());
            }
        }
        let lists = |keep: fn(&[Value]) -> bool| -> Vec<&String> {
            map.iter()
                .filter(|(_, v)| v.as_array().is_some_and(|a| keep(a)))
                .map(|(k, _)| k)
                .collect()
        };
        let mut arrays = lists(|a| a.iter().any(Value::is_object));
        let page_like = map.values().all(|v| match v {
            Value::Array(a) => a.is_empty(),
            other => !other.is_object(),
        });
        if arrays.is_empty() && page_like {
            arrays = lists(|a| a.is_empty());
        }
        match arrays.as_slice() {
            [key] => Ok((*key).clone()),
            [] => Err(anyhow!(
                "--where '{}': the answer is one object, not a list; --where keeps the items of a list",
                self.spec()
            )),
            several => {
                let first = &self.path[0];
                let lists = several
                    .iter()
                    .map(|k| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let having: Vec<&str> = several
                    .iter()
                    .filter(|k| {
                        map[k.as_str()].as_array().is_some_and(|a| {
                            a.iter().any(|i| {
                                i.as_object()
                                    .is_some_and(|o| lookup_key(o, &self.path).is_some())
                            })
                        })
                    })
                    .map(|k| k.as_str())
                    .collect();
                let path = self.path.join(".");
                Err(match having.as_slice() {
                    [] => anyhow!(
                        "--where '{}': the answer has several lists ({lists}) and none of their items \
                         has a field {first}; start the path with the list to filter, as in <list>.<field>",
                        self.spec()
                    ),
                    [one] => anyhow!(
                        "--where '{}': the answer has several lists ({lists}); {first} is a field of the \
                         items of {one}, so start the path with it: {one}.{path}",
                        self.spec()
                    ),
                    many => anyhow!(
                        "--where '{}': the answer has several lists ({lists}) and the items of {} have \
                         {first}; start the path with the one to filter, as in {}.{path}",
                        self.spec(),
                        many.join(" and "),
                        many[0]
                    ),
                })
            }
        }
    }

    /// A path that is only the name of a top-level list: `--where` compares fields of its items.
    /// An error whatever the list holds, so the answer does not depend on the data.
    fn list_name_error(&self, list: &str, items: &[Value]) -> anyhow::Error {
        if !items.is_empty() && !items.iter().any(Value::is_object) {
            return anyhow!(
                "--where '{}': {list} is a list in the answer, and of plain values (strings or numbers), \
                 not objects; --where compares fields of a list's objects, so it can't filter {list}",
                self.spec()
            );
        }
        let plain = if items.is_empty() {
            "; a list of plain strings can't be filtered"
        } else {
            ""
        };
        anyhow!(
            "--where '{}': {list} is a list in the answer, not a field of its items; --where compares \
             fields of a list's objects, so write {list}.<field> (as in {list}.{list} for the items' own \
             {list}){plain}",
            self.spec()
        )
    }

    /// A path that starts with a top-level field of an object answer: that field is the answer's own.
    fn answer_field_error(
        &self,
        first: &str,
        map: &serde_json::Map<String, Value>,
    ) -> anyhow::Error {
        match self.list_key(map) {
            Ok(list) => anyhow!(
                "--where '{}': {first} is a field of the answer itself, not of the items of its list \
                 {list}; --where keeps list items (to filter {list}, start the path with {list}.)",
                self.spec()
            ),
            Err(_) => anyhow!(
                "--where '{}': {first} is a field of the answer itself, which is one object, not a \
                 list; --where keeps the items of a list",
                self.spec()
            ),
        }
    }

    /// `--where` compares fields of objects: a list of strings or numbers (log lines) is an error.
    /// An empty list is fine, so an empty page still keeps its `next_page`.
    fn check_items(&self, list: &str, items: &[Value]) -> Result<()> {
        if items.is_empty() || items.iter().any(Value::is_object) {
            return Ok(());
        }
        Err(anyhow!(
            "--where '{}': {list} is a list of plain values (strings or numbers), not of objects; \
             --where compares fields of objects",
            self.spec()
        ))
    }

    /// Whether `item` matches: any value `path` reaches in it does (none, with a `!`).
    fn holds(&self, item: &Value, path: &[String]) -> bool {
        let mut any = false;
        for_each_leaf(item, path, &mut |leaf| any = any || self.leaf_matches(leaf));
        any != self.negate
    }

    /// Whether one value matches: compared as text (`null` for null; an object never matches; with
    /// `=`, a number field and a number value compare as numbers), or, for a comparison, as a number
    /// or date (anything else doesn't match).
    fn leaf_matches(&self, leaf: &Value) -> bool {
        if let Some(bound) = &self.bound {
            return bound.compare(leaf).is_some_and(|o| self.op.holds(o));
        }
        let text = match leaf {
            Value::Object(_) => return false,
            Value::String(s) => std::borrow::Cow::Borrowed(s.as_str()),
            other => std::borrow::Cow::Owned(other.to_string()),
        };
        // `RemainingWork=0` matches the 0.0 Azure DevOps sends.
        let number = match (self.op, leaf) {
            (WhereOp::Equals, Value::Number(n)) => n.as_f64(),
            _ => None,
        };
        // `completed_at=2026-11-24` matches any time that day (UTC), as the comparisons do.
        let instant = match (self.op, leaf) {
            (WhereOp::Equals, Value::String(s)) => match parse_when(s) {
                Some(When::Instant(t)) => Some(t),
                _ => None,
            },
            _ => None,
        };
        self.values.iter().any(|w| {
            if let (Some(n), Some(w)) = (number, parse_number(w)) {
                return n == w;
            }
            if let (Some(t), Some(When::Day(day))) = (instant, parse_when(w)) {
                return t.date_naive() == day;
            }
            self.op.matches_text(&text, w)
        })
    }

    /// The items that match. When none of a non-empty list does, `notes` gets one line with the
    /// values the path has on this page, so a wrong value (`Success`, `failed` for `failure`) shows.
    /// When a comparison left out items whose value is not a number (or date), a line says so.
    fn keep(
        &self,
        items: Vec<Value>,
        path: &[String],
        list: &str,
        notes: &mut Vec<String>,
    ) -> Vec<Value> {
        let total = items.len();
        // A field no item has (often a typo) counts as null everywhere: `!=` then keeps every item.
        let missing = if total > 0 {
            missing_everywhere(&items, path)
        } else {
            None
        };
        let (kept, dropped): (Vec<Value>, Vec<Value>) =
            items.into_iter().partition(|item| self.holds(item, path));
        if kept.is_empty() && total > 0 {
            notes.push(self.none_kept_note(&dropped, path, list, missing.as_deref()));
        } else if let Some(missing) = missing {
            notes.push(format!(
                "--where '{}': no item in {list} has {} ({missing}), so it counted as null in each",
                self.spec(),
                path.join(".")
            ));
        } else if let Some(note) = self.not_comparable_note(&dropped, path, list, total) {
            notes.push(note);
        }
        kept
    }

    fn none_kept_note(
        &self,
        items: &[Value],
        path: &[String],
        list: &str,
        missing: Option<&str>,
    ) -> String {
        const SHOWN: usize = 10;
        let full = path.join(".");
        let head = format!(
            "--where '{}' kept none of the {} item{} in {list}",
            self.spec(),
            items.len(),
            if items.len() == 1 { "" } else { "s" }
        );
        if let Some(missing) = missing {
            return format!("{head} (no item has {full}; {missing})");
        }
        let mut seen = Vec::new();
        for item in items {
            path_values(item, path, &mut seen);
        }
        if seen.is_empty() {
            return format!("{head} (no item has a value at {full})");
        }
        let more = seen.len().saturating_sub(SHOWN);
        seen.truncate(SHOWN);
        let more = if more > 0 {
            format!(", and {more} more")
        } else {
            String::new()
        };
        let kind = match &self.bound {
            Some(bound) => format!("; {} compares {}", self.op.symbol(), bound.kind()),
            None => String::new(),
        };
        format!("{head} ({full} here: {}{more}{kind})", seen.join(", "))
    }

    /// For a comparison: the items left out because their value at `path` is there but is not a
    /// number (or date), which would otherwise look like items that failed the comparison.
    fn not_comparable_note(
        &self,
        dropped: &[Value],
        path: &[String],
        list: &str,
        total: usize,
    ) -> Option<String> {
        let bound = self.bound.as_ref()?;
        let mut count = 0;
        let mut seen = Vec::new();
        for item in dropped {
            let mut odd = Vec::new();
            for_each_leaf(item, path, &mut |leaf| {
                if !leaf.is_null() && bound.compare(leaf).is_none() {
                    odd.push(leaf_text(leaf));
                }
            });
            if !odd.is_empty() {
                count += 1;
                for text in odd {
                    if seen.len() < 5 && !seen.contains(&text) {
                        seen.push(text);
                    }
                }
            }
        }
        (count > 0).then(|| {
            format!(
                "--where '{}' left out {count} of the {total} items in {list} whose {} is not {} \
                 (as: {})",
                self.spec(),
                self.path.join("."),
                bound.kind(),
                seen.join(", ")
            )
        })
    }

    fn spec(&self) -> String {
        format!(
            "{}{}{}{}",
            self.path.join("."),
            if self.negate { "!" } else { "" },
            self.op.symbol(),
            self.values
                .iter()
                .map(|v| v.replace('|', "\\|"))
                .collect::<Vec<_>>()
                .join("|")
        )
    }
}

/// Splits a `--where` value list at `|`; `\|` is a `|` inside a value. Values are trimmed.
fn split_where_values(text: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                current.push('|');
                chars.next();
            }
            '|' => values.push(std::mem::take(&mut current).trim().to_string()),
            other => current.push(other),
        }
    }
    values.push(current.trim().to_string());
    values
}

static NULL: Value = Value::Null;

/// Calls `f` with every value `path` reaches in `value`. Arrays on the way apply the rest of the
/// path to every element, so an empty array reaches nothing; a missing field, or a path that runs
/// through `null`, reaches `null`; a path that runs into a plain value reaches nothing. Keys with
/// dots are found as [`lookup_key`] finds them.
fn for_each_leaf(value: &Value, path: &[String], f: &mut dyn FnMut(&Value)) {
    match value {
        Value::Array(items) => {
            for item in items {
                for_each_leaf(item, path, f);
            }
        }
        Value::Object(map) if !path.is_empty() => match lookup_key(map, path) {
            Some((_, child, used)) => for_each_leaf(child, &path[used..], f),
            None => f(&NULL),
        },
        Value::Null => f(value),
        _ if path.is_empty() => f(value),
        _ => {}
    }
}

/// When `path` reaches nothing in any of `items` (a field none of them has): where it stopped, as
/// `keys here: a, b` or `keys under x: a, b`. `None` when some item has it, even as `null`.
fn missing_everywhere(items: &[Value], path: &[String]) -> Option<String> {
    if items.iter().any(|item| path_present(item, path)) {
        return None;
    }
    let parts: Vec<&str> = path.iter().map(String::as_str).collect();
    let mut stop = (0, BTreeSet::new());
    for item in items {
        path_found(item, &parts, 0, &mut stop);
    }
    let (depth, keys) = stop;
    let place = if depth == 0 {
        "keys here".to_string()
    } else {
        format!("keys under {}", parts[..depth].join("."))
    };
    Some(if keys.is_empty() {
        "nothing to look in".to_string()
    } else {
        format!("{place}: {}", capped_list(keys, 15))
    })
}

/// Whether `path` is in `value`: it reaches a value, or `null` on the way; through lists, in any
/// element (an empty list can't tell, so it counts).
fn path_present(value: &Value, path: &[String]) -> bool {
    if path.is_empty() {
        return true;
    }
    match value {
        Value::Object(map) => {
            lookup_key(map, path).is_some_and(|(_, child, used)| path_present(child, &path[used..]))
        }
        Value::Array(items) => items.is_empty() || items.iter().any(|i| path_present(i, path)),
        Value::Null => true,
        _ => false,
    }
}

/// A value as a note shows it: JSON text (strings quoted), `{...}` for an object.
fn leaf_text(leaf: &Value) -> String {
    match leaf {
        Value::Object(_) => "{...}".to_string(),
        other => other.to_string(),
    }
}

/// The distinct values `path` reaches in `value`, as a note shows them, in order.
fn path_values(value: &Value, path: &[String], seen: &mut Vec<String>) {
    for_each_leaf(value, path, &mut |leaf| {
        let text = leaf_text(leaf);
        if !seen.contains(&text) {
            seen.push(text);
        }
    });
}

/// Generic `--where` filter for list answers (`--where 'records.result=failed|canceled'`). Every
/// condition must hold; only the matching items stay, and the rest of the answer (`count`,
/// `total_count`) is unchanged. Returns `value` untouched when `filters` is empty, and beside it
/// the notes of conditions that kept none of a list's items (joined with `; `).
pub fn filter_json_items(value: Value, filters: &[String]) -> Result<(Value, Option<String>)> {
    let conditions = parse_where(filters)?;
    let mut notes = Vec::new();
    let value = conditions
        .iter()
        .try_fold(value, |value, c| c.apply(value, &mut notes))?;
    Ok((value, (!notes.is_empty()).then(|| notes.join("; "))))
}

/// Parses `--where` conditions, ignoring empty ones; errors name the bad one.
pub fn parse_where(filters: &[String]) -> Result<Vec<WhereCondition>> {
    filters
        .iter()
        .filter(|s| !s.trim().is_empty())
        .map(|s| WhereCondition::parse(s))
        .collect()
}

/// Direct Mode single-tool execution: reads ONLY `manuals/<manual>.utcp.json` (and `.env` only when `${` is present).
/// With `staged`, the staged copy in `manuals/staged/` instead. This process reads the files the tool
/// uploads and saves the file it answers with (to `out`, else the downloads folder).
pub async fn call_tool_direct(
    paths: &UtcpPaths,
    target: &str,
    args: HashMap<String, Value>,
    dry_run: bool,
    shape: AnswerShape<'_>,
    staged: bool,
    out: Option<&crate::transfer::OutTarget>,
) -> Result<Value> {
    let (manual_name, tool_name, manual) = if staged {
        let index = crate::staging::staged_index(paths)?;
        let (m, t) = resolve_manual_and_tool(target, Some(&index), paths)?;
        let manual = crate::staging::load_staged_manual(paths, &m)?;
        (m, t, manual)
    } else {
        let (m, t) = resolve_manual_and_tool(target, None, paths)?;
        let manual = load_single_cached_manual(paths, &m)?;
        (m, t, manual)
    };
    let env_map = if dry_run
        || manual
            .tools
            .iter()
            .any(|t| t.provider.to_string().contains("${"))
    {
        // Only this manual's names, so a direct call still reads no other manual or the index.
        let mut env_map = load_file_env_map(paths);
        if let Some(secrets) = UtcpSettings::load(paths).secrets {
            let wanted = manual_env_var_names([(manual_name.as_str(), &manual)]);
            if let Some(run) = run_direct_secrets(paths, &secrets, &wanted) {
                env_map.extend(run.values);
            }
        }
        env_map
    } else {
        HashMap::new()
    };

    if dry_run {
        return dry_run_tool_from_manual(&manual, &manual_name, &tool_name, &args, &env_map)
            .map(|(request, note)| shape.with_note(note.as_deref()).for_dry_run(request));
    }

    let protocols =
        CommunicationProtocolRegistry::with_shared_http_client(build_secure_http_client());
    let limits = crate::transfer::Limits::load(paths);
    let files = crate::transfer::LocalFiles {
        limits: limits.clone(),
    };
    let (output, note) = execute_loaded_tool(
        &manual,
        &manual_name,
        &tool_name,
        args,
        &env_map,
        &protocols,
        paths,
        &files,
    )
    .await?;
    let shape = shape.with_note(note.as_deref());
    crate::transfer::finish_output_locally(output, &tool_name, shape, out, paths, &limits).await
}

/// Execute a batch of tool calls concurrently in Direct Mode over a single shared HTTP/TLS connection pool.
/// With `staged`, every item calls a staged manual.
pub async fn batch_call_direct(
    paths: &UtcpPaths,
    items: Vec<BatchRequestItem>,
    concurrency: usize,
    staged: bool,
) -> Result<Vec<BatchResultItem>> {
    let index = index_for(paths, staged)?;
    let env_map = if staged {
        load_env_map_for(paths, Some(&index.env_var_names()))
    } else {
        load_env_map(paths)
    };
    let protocols =
        CommunicationProtocolRegistry::with_shared_http_client(build_secure_http_client());
    let limits = crate::transfer::Limits::load(paths);
    let files = crate::transfer::LocalFiles {
        limits: limits.clone(),
    };

    let mut manual_cache: HashMap<String, CachedUtcpManual> = HashMap::new();
    for item in &items {
        if let Ok((mname, _)) = resolve_manual_and_tool(&item.tool, Some(&index), paths) {
            if let std::collections::hash_map::Entry::Vacant(slot) = manual_cache.entry(mname) {
                let loaded = if staged {
                    crate::staging::load_staged_manual(paths, slot.key())
                } else {
                    load_single_cached_manual(paths, slot.key())
                };
                if let Ok(m) = loaded {
                    slot.insert(m);
                }
            }
        }
    }

    let max_conc = concurrency.max(1);
    let results: Vec<(usize, BatchResultItem)> = stream::iter(items.into_iter().enumerate())
        .map(|(idx, item)| {
            let protocols = protocols.clone();
            let env_map = &env_map;
            let index = &index;
            let manual_cache = &manual_cache;
            let files = &files;
            let limits = &limits;
            async move {
                let started = Instant::now();
                let item_fields = item.fields.clone();
                let item_filters = item.filters.clone();
                let out = item.out.as_deref().map(crate::transfer::OutTarget::parse);
                let outcome = async {
                    let (mname, tname) = resolve_manual_and_tool(&item.tool, Some(index), paths)?;
                    let manual = manual_cache
                        .get(&mname)
                        .ok_or_else(|| anyhow!("Manual '{}' not found", mname))?;
                    let (output, note) = execute_loaded_tool(
                        manual, &mname, &tname, item.args, env_map, &protocols, paths, files,
                    )
                    .await?;
                    crate::transfer::finish_output_locally(
                        output,
                        &tname,
                        AnswerShape {
                            filters: &item_filters,
                            fields: item_fields.as_deref(),
                            note: note.as_deref(),
                        },
                        out.as_ref(),
                        paths,
                        limits,
                    )
                    .await
                }
                .await;
                let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
                match outcome {
                    Ok(val) => (
                        idx,
                        BatchResultItem::succeeded(item.id, item.tool, val, latency_ms),
                    ),
                    Err(err) => (
                        idx,
                        BatchResultItem::failed(item.id, item.tool, format!("{err:#}"), latency_ms),
                    ),
                }
            }
        })
        .buffer_unordered(max_conc)
        .collect()
        .await;

    let mut ordered = results;
    ordered.sort_by_key(|(idx, _)| *idx);
    Ok(ordered.into_iter().map(|(_, r)| r).collect())
}

/// Produce a `DoctorReport` checking paths, index freshness, credentials (`SET` vs `MISSING`, and
/// where each value comes from), the secrets command, `settings.toml`, and daemon state. The
/// credentials are this process's view; when the daemon runs, `doctor` shows the daemon's instead.
pub fn build_doctor_report(
    paths: &UtcpPaths,
    daemon_running: bool,
    daemon_pid: Option<u32>,
) -> Result<DoctorReport> {
    let settings_error = UtcpSettings::load_checked(&paths.settings_file)
        .err()
        .map(|e| format!("{e:#}"));
    let settings = UtcpSettings::load(paths);
    let (download_dir, download_dir_source) = crate::transfer::download_folder(paths);
    let logging_enabled = crate::activity::is_logging_enabled(paths);
    let index_present = paths.search_index_file.exists();
    let idx = load_or_build_index_sync(paths)?;
    let staged_idx = crate::staging::staged_index(paths).unwrap_or_default();
    let mut wanted = idx.env_var_names();
    wanted.extend(staged_idx.env_var_names());
    let mut env_map = load_file_env_map(paths);
    // With the daemon running, `doctor` reports the daemon's credentials and secrets command run.
    let secrets = settings
        .secrets
        .as_ref()
        .filter(|_| !daemon_running)
        .map(|secrets| {
            run_direct_secrets(paths, secrets, &wanted).unwrap_or_else(|| {
                crate::secrets::SecretsRun {
                    values: HashMap::new(),
                    status: crate::secrets::SecretsCommandStatus {
                        command: secrets.command.join(" "),
                        error: None,
                        provided: Vec::new(),
                    },
                }
            })
        });
    let secret_values = secrets
        .as_ref()
        .map(|run| run.values.clone())
        .unwrap_or_default();
    env_map.extend(secret_values.clone());

    let mut credentials = Vec::new();
    let all = idx
        .manuals
        .iter()
        .map(|m| (m, false))
        .chain(staged_idx.manuals.iter().map(|m| (m, true)));
    for (m, staged) in all {
        for req_var in &m.required_env_vars {
            let resolved = resolve_secret_var(&m.name, req_var, &env_map);
            let is_set = resolved.is_some();
            let source = resolved.as_ref().map(|(value, via)| {
                if secret_values.get(via).map(String::as_str) == Some(*value) {
                    SOURCE_SECRETS_COMMAND
                } else {
                    SOURCE_ENV
                }
                .to_string()
            });
            credentials.push(CredentialStatus {
                manual: m.name.clone(),
                env_var: req_var.clone(),
                resolved_via: resolved.map(|(_, via)| via),
                is_set,
                source,
                staged,
            });
        }
    }

    Ok(DoctorReport {
        config_file: paths.config_file.to_string_lossy().into_owned(),
        cache_dir: paths.cache_dir.to_string_lossy().into_owned(),
        state_dir: paths.state_dir.to_string_lossy().into_owned(),
        socket_endpoint: paths.socket_endpoint.clone(),
        daemon_running,
        daemon_pid,
        daemon_version: None,
        logging_enabled,
        cache_on_register: settings.cache_on_register,
        index_present,
        manual_count: idx.manuals.len(),
        tool_count: idx.tools.len(),
        staged_count: staged_idx.manuals.len(),
        env_file: paths.env_file.to_string_lossy().into_owned(),
        env_file_exists: paths.env_file.is_file(),
        credentials,
        secrets_command: secrets.map(|run| run.status),
        settings_error,
        settings_file: paths.settings_file.to_string_lossy().into_owned(),
        unknown_settings: std::fs::read_to_string(&paths.settings_file)
            .map(|text| crate::settings_file::unknown_settings(&text))
            .unwrap_or_default(),
        max_upload_mb: settings.max_upload_mb,
        max_download_mb: settings.max_download_mb,
        download_dir: download_dir.to_string_lossy().into_owned(),
        download_dir_source: Some(download_dir_source),
    })
}

#[cfg(test)]
mod select_manual_names_tests {
    use super::select_manual_names;

    #[test]
    fn exact_names_and_trailing_star_prefixes() {
        let have: Vec<String> = ["azdo_core", "azdo_git", "github_users", "zoo"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let pick = |p: &[&str]| {
            let p: Vec<String> = p.iter().map(|s| s.to_string()).collect();
            select_manual_names(&have, &p, "staged")
        };
        assert_eq!(pick(&[]).unwrap(), have);
        assert_eq!(pick(&["azdo_*"]).unwrap(), ["azdo_core", "azdo_git"]);
        assert_eq!(
            pick(&["zoo", "azdo_*", "azdo_git"]).unwrap(),
            ["azdo_core", "azdo_git", "zoo"]
        );
        assert_eq!(pick(&["*"]).unwrap(), have);
        let err = pick(&["azdo_*", "gh_*"]).unwrap_err().to_string();
        assert_eq!(
            err,
            "No staged manual matches 'gh_*' (staged: azdo_core, azdo_git, github_users, zoo)"
        );
        let err = pick(&["azdo"]).unwrap_err().to_string();
        assert!(err.starts_with("'azdo' is not staged (staged: "), "{err}");
        assert!(pick(&["a*_git"])
            .unwrap_err()
            .to_string()
            .contains("only a trailing *"));
        let none: Vec<String> = Vec::new();
        let err = select_manual_names(&none, &["x".to_string()], "staged").unwrap_err();
        assert_eq!(err.to_string(), "'x' is not staged (staged: none)");
    }
}

#[cfg(test)]
mod field_projection_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn projects_arrays_nested_paths_and_search_envelopes() {
        let commits = json!([
            {
                "sha": "ed9b370",
                "html_url": "https://github.com/org/repo/commit/ed9b370",
                "followers_url": "https://api.github.com/users/u/followers",
                "commit": {
                    "message": "fix license",
                    "author": { "name": "Alice", "email": "a@example.com" },
                    "verification": { "verified": false, "signature": null }
                }
            }
        ]);

        let fields = vec![
            "sha".to_string(),
            "commit.message".to_string(),
            "commit.author.name".to_string(),
            "html_url".to_string(),
        ];
        let projected = project_json_fields(commits, &fields);
        assert_eq!(
            projected,
            json!([
                {
                    "sha": "ed9b370",
                    "html_url": "https://github.com/org/repo/commit/ed9b370",
                    "commit": {
                        "message": "fix license",
                        "author": { "name": "Alice" }
                    }
                }
            ])
        );

        let search_resp = json!({
            "total_count": 1,
            "incomplete_results": false,
            "items": [
                { "full_name": "org/repo", "stargazers_count": 42, "archive_url": "https://..." }
            ]
        });
        let search_projected = project_json_fields(
            search_resp,
            &["full_name".to_string(), "stargazers_count".to_string()],
        );
        assert_eq!(
            search_projected,
            json!({
                "total_count": 1,
                "incomplete_results": false,
                "items": [
                    { "full_name": "org/repo", "stargazers_count": 42 }
                ]
            })
        );
    }

    #[test]
    fn projects_dotted_paths_through_arrays_under_any_key() {
        let runs = json!({
            "total_count": 2,
            "workflow_runs": [
                { "id": 1, "name": "Lint", "conclusion": "success", "actor": { "login": "a", "id": 9 } },
                { "id": 2, "name": "Build", "conclusion": "failure", "actor": { "login": "b", "id": 8 } }
            ]
        });
        let fields: Vec<String> = [
            "total_count",
            "workflow_runs.id",
            "workflow_runs.actor.login",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            project_json_fields(runs, &fields),
            json!({
                "total_count": 2,
                "workflow_runs": [
                    { "id": 1, "actor": { "login": "a" } },
                    { "id": 2, "actor": { "login": "b" } }
                ]
            })
        );

        // Nested arrays inside each element, plus scalar arrays that a path cannot descend into.
        let releases = json!([
            { "tag_name": "v1", "tags": ["x"], "assets": [{ "name": "a.tar.xz", "size": 1 }, { "name": "b.zip", "size": 2 }] },
            { "tag_name": "v0", "tags": ["y"], "assets": [] }
        ]);
        let fields: Vec<String> = ["tag_name", "assets.name", "tags.nope"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            project_json_fields(releases, &fields),
            json!([
                { "tag_name": "v1", "tags": ["x"], "assets": [{ "name": "a.tar.xz" }, { "name": "b.zip" }] },
                { "tag_name": "v0", "tags": ["y"], "assets": [] }
            ])
        );
    }

    #[test]
    fn projects_into_the_only_object_list_when_no_top_level_key_matches() {
        let jobs = json!({
            "total_count": 1,
            "jobs": [{ "id": 7, "name": "zed", "conclusion": "failure", "runner_name": "gh",
                        "steps": [{ "number": 3, "name": "Run", "conclusion": "failure", "started_at": "t" }] }]
        });
        let fields: Vec<String> = ["name", "conclusion", "steps.name", "steps.conclusion"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            project_json_fields(jobs, &fields),
            json!({
                "total_count": 1,
                "jobs": [{ "name": "zed", "conclusion": "failure",
                            "steps": [{ "name": "Run", "conclusion": "failure" }] }]
            })
        );

        // Two object lists: ambiguous, so nothing guesses and only matching top-level keys survive.
        let ambiguous = json!({ "a": [{ "id": 1 }], "b": [{ "id": 2 }] });
        assert_eq!(
            project_json_fields(ambiguous, &["id".to_string()]),
            json!({})
        );
    }

    #[test]
    fn resolves_uppercase_and_doubled_underscore_secrets_and_preserves_manual_vars() {
        let manual_bytes = br#"{
            "manual_version": "1.0.0",
            "utcp_version": "1.0.0",
            "info": { "title": "GitHub", "version": "1.0.0" },
            "tools": [{
                "name": "get_me",
                "description": "Profile",
                "inputs": { "type": "object" },
                "outputs": { "type": "object" },
                "tool_call_template": {
                    "call_template_type": "http",
                    "http_method": "GET",
                    "url": "https://api.github.com/user",
                    "auth": {
                        "auth_type": "api_key",
                        "var_name": "Authorization",
                        "api_key": "Bearer ${GITHUB_TOKEN}",
                        "location": "header"
                    }
                }
            }]
        }"#;

        let converted = convert_bytes_to_cached_manual(
            manual_bytes,
            "github",
            "file:///github.utcp.json",
            None,
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(converted.meta.required_env_vars, vec!["GITHUB_TOKEN"]);

        let mut env_map = HashMap::new();
        env_map.insert("GITHUB_TOKEN".to_string(), "ghp_test123".to_string());
        env_map.insert("MY_SVC_API_KEY".to_string(), "svc_upper_secret".to_string());

        // Exact uppercase match
        let r1 = resolve_secret_var("github", "GITHUB_TOKEN", &env_map).unwrap();
        assert_eq!(r1.0, "ghp_test123");
        assert_eq!(r1.1, "GITHUB_TOKEN");

        // Mixed-case / doubled-underscore query resolves uppercase shell env fallback!
        let r2 = resolve_secret_var("github", "github_TOKEN", &env_map).unwrap();
        assert_eq!(r2.0, "ghp_test123");
        assert_eq!(r2.1, "GITHUB_TOKEN");

        let r3 = resolve_secret_var("my_svc", "my__svc_API_KEY", &env_map).unwrap();
        assert_eq!(r3.0, "svc_upper_secret");
        assert_eq!(r3.1, "MY_SVC_API_KEY");
    }

    // The daemon's env allow-list: only names a manual's placeholders or `required_env_vars` can
    // resolve through are kept, and every name `resolve_secret_var` reads is in that set.
    #[test]
    fn manual_env_var_names_cover_every_lookup_and_nothing_else() {
        let manual: CachedUtcpManual = serde_json::from_value(json!({
            "manual_version": "1.0.0",
            "utcp_version": "1.0.0",
            "info": { "title": "svc", "version": "1.0.0" },
            "_meta": {
                "manual_name": "my_svc", "source_url": "file:///svc.json", "sha256": "x",
                "fetched_at": "2026-01-01T00:00:00Z", "tool_count": 1,
                "protocols": ["http"], "required_env_vars": ["my__svc_API_KEY"]
            },
            "tools": [{
                "name": "get", "description": "", "inputs": {}, "outputs": {},
                "provider": {
                    "provider_type": "http", "url": "https://svc.invalid/${MY_SVC_REGION}",
                    "headers": { "X-Org": "${ORG_ID}" }
                },
                "tool_call_template": { "call_template_type": "http", "url": "${TEMPLATE_ONLY}" }
            }]
        }))
        .unwrap();

        let names = manual_env_var_names([("my_svc", &manual)]);
        for expected in [
            "my__svc_API_KEY",
            "MY_SVC_API_KEY",
            "MY_SVC_REGION",
            "my__svc_REGION",
            "ORG_ID",
            "my__svc_ORG_ID",
            "MY_SVC_ORG_ID",
            "TEMPLATE_ONLY",
        ] {
            assert!(names.contains(expected), "missing {expected}: {names:?}");
        }
        assert!(!names.contains("PATH"));
        assert!(!names.contains("UNRELATED_SECRET_TOKEN"));
        for var in ["my__svc_API_KEY", "MY_SVC_REGION", "ORG_ID"] {
            for candidate in secret_var_candidates("my_svc", var) {
                assert!(names.contains(&candidate), "lookup {candidate} not allowed");
            }
        }

        let mut env_map: HashMap<String, String> = [
            ("MY_SVC_API_KEY", "k"),
            ("ORG_ID", "o"),
            ("PATH", "/usr/bin"),
            ("UNRELATED_SECRET_TOKEN", "t"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        retain_allowed_env(&mut env_map, &names);
        let mut kept: Vec<_> = env_map.keys().cloned().collect();
        kept.sort();
        assert_eq!(kept, vec!["MY_SVC_API_KEY", "ORG_ID"]);
        // Filtering never changes what a lookup resolves to.
        assert_eq!(
            resolve_secret_var("my_svc", "my__svc_API_KEY", &env_map).unwrap(),
            ("k", "MY_SVC_API_KEY".to_string())
        );
    }

    // issues/012: a cached manual written by an older `utcp import-mcp` build stored the MCP provider
    // JSON (no `call_template_type`) as `tool_call_template`; registering that file must still work.
    #[test]
    fn converts_cached_mcp_manual_whose_template_lacks_call_template_type() {
        let manual_bytes = br#"{
            "manual_version": "1.0.0",
            "utcp_version": "1.0.0",
            "info": { "title": "ctx", "version": "1.0.0" },
            "_meta": { "manual_name": "ctx", "source_url": "mcp+http://127.0.0.1:1/mcp" },
            "tools": [{
                "name": "resolve",
                "description": "Resolve a library",
                "inputs": { "type": "object" },
                "outputs": { "type": "object" },
                "tags": ["mcp"],
                "provider": {
                    "name": "ctx", "provider_type": "mcp", "url": "http://127.0.0.1:1/mcp",
                    "allowed_communication_protocols": ["mcp"]
                },
                "tool_call_template": {
                    "name": "ctx", "provider_type": "mcp", "url": "http://127.0.0.1:1/mcp",
                    "allowed_communication_protocols": ["mcp"]
                }
            }]
        }"#;

        let converted = convert_bytes_to_cached_manual(
            manual_bytes,
            "ctx_copy",
            "file:///ctx.utcp.json",
            None,
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(converted.tools.len(), 1);
        assert_eq!(converted.meta.protocols, vec!["mcp"]);
        let tmpl = converted.tools[0].tool_call_template.as_ref().unwrap();
        assert_eq!(tmpl["call_template_type"], "mcp");

        // The repaired output must itself be accepted as a source again.
        let reserialized = serde_json::to_vec(&converted).unwrap();
        let again = convert_bytes_to_cached_manual(
            &reserialized,
            "ctx_copy2",
            "file:///ctx_copy.utcp.json",
            None,
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(again.tools.len(), 1);
    }
}

#[cfg(test)]
mod where_filter_tests {
    use super::*;
    use serde_json::json;

    fn filter(value: Value, specs: &[&str]) -> Result<Value> {
        noted(value, specs).map(|(value, _)| value)
    }

    fn noted(value: Value, specs: &[&str]) -> Result<(Value, Option<String>)> {
        let specs: Vec<String> = specs.iter().map(|s| s.to_string()).collect();
        filter_json_items(value, &specs)
    }

    fn timeline() -> Value {
        json!({
            "id": "t1",
            "records": [
                { "name": "Build", "type": "Task", "result": "failed", "errorCount": 2 },
                { "name": "Sign", "type": "Job", "result": "canceled", "errorCount": 0 },
                { "name": "Test", "type": "Task", "result": "succeeded", "errorCount": 0 },
                { "name": "Stage", "type": "Stage", "result": null }
            ]
        })
    }

    #[test]
    fn a_path_starting_with_a_top_level_array_filters_that_array() {
        let out = filter(timeline(), &["records.result=failed|canceled"]).unwrap();
        assert_eq!(out["id"], "t1", "the rest of the answer is kept");
        let names: Vec<&str> = out["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["Build", "Sign"]);
    }

    #[test]
    fn conditions_all_hold_and_not_equal_keeps_missing_fields() {
        let out = filter(
            timeline(),
            &["records.type=Task", "records.result!=succeeded"],
        )
        .unwrap();
        assert_eq!(out["records"].as_array().unwrap().len(), 1);
        assert_eq!(out["records"][0]["name"], "Build");

        // `errorCount` is missing on Stage: `=` drops it, `!=` keeps it.
        let eq = filter(timeline(), &["records.errorCount=0"]).unwrap();
        assert_eq!(eq["records"].as_array().unwrap().len(), 2);
        let ne = filter(timeline(), &["records.errorCount!=0"]).unwrap();
        let names: Vec<&str> = ne["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["Build", "Stage"]);
    }

    #[test]
    fn scalars_compare_as_text() {
        let out = filter(timeline(), &["records.result=null"]).unwrap();
        assert_eq!(out["records"][0]["name"], "Stage");
        let items = json!([{ "ok": true, "n": 5 }, { "ok": false, "n": 6 }]);
        assert_eq!(
            filter(items.clone(), &["ok=true"]).unwrap(),
            json!([{ "ok": true, "n": 5 }])
        );
        assert_eq!(
            filter(items, &["n=6"]).unwrap(),
            json!([{ "ok": false, "n": 6 }])
        );
    }

    #[test]
    fn filters_the_envelope_list_and_keeps_counts() {
        let builds = json!({
            "count": 3,
            "value": [
                { "id": 1, "result": "failed" },
                { "id": 2, "result": "succeeded" },
                { "id": 3, "result": "failed" }
            ]
        });
        let out = filter(builds, &["result=failed"]).unwrap();
        assert_eq!(out["count"], 3, "count is the server's, not recounted");
        assert_eq!(
            out["value"],
            json!([{ "id": 1, "result": "failed" }, { "id": 3, "result": "failed" }])
        );

        let empty_page = json!({ "count": 0, "value": [] });
        assert_eq!(
            filter(empty_page.clone(), &["result=failed"]).unwrap(),
            empty_page
        );
    }

    #[test]
    fn arrays_inside_items_match_when_any_element_does() {
        let issues = json!([
            { "n": 1, "labels": [{ "name": "bug" }, { "name": "ui" }] },
            { "n": 2, "labels": [{ "name": "docs" }] },
            { "n": 3, "tags": ["x", "bug"] }
        ]);
        let out = filter(issues.clone(), &["labels.name=bug"]).unwrap();
        assert_eq!(out.as_array().unwrap().len(), 1);
        assert_eq!(out[0]["n"], 1);
        let out = filter(issues.clone(), &["tags=bug"]).unwrap();
        assert_eq!(out[0]["n"], 3);
        // `!=` holds when no element matches (and when there is no such list).
        let out = filter(issues, &["labels.name!=bug"]).unwrap();
        assert_eq!(
            out,
            json!([{ "n": 2, "labels": [{ "name": "docs" }] }, { "n": 3, "tags": ["x", "bug"] }])
        );
    }

    #[test]
    fn runs_before_fields() {
        let shape = AnswerShape {
            filters: &["records.result=failed".to_string()],
            fields: Some(&["records.name".to_string()]),
            note: None,
        };
        assert_eq!(
            shape.apply(timeline()),
            (json!({ "records": [{ "name": "Build" }] }), None)
        );
    }

    /// The call was already sent (perhaps a write), so a `--where` that can't apply to the answer
    /// leaves it unfiltered, with `--fields` still applied, and a note saying why; never an error.
    #[test]
    fn a_where_that_cannot_apply_keeps_the_answer_with_a_note() {
        let comment = json!({ "id": 7, "body": "x", "user": { "login": "a" } });
        let shape = AnswerShape {
            filters: &["id>0".to_string()],
            fields: Some(&["id".to_string()]),
            note: Some("call note"),
        };
        let (out, note) = shape.apply(comment);
        assert_eq!(out, json!({ "id": 7 }));
        let note = note.unwrap();
        assert!(
            note.starts_with("call note; the call succeeded; --where was not applied"),
            "{note}"
        );
        assert!(
            note.contains("'id>0': id is a field of the answer itself"),
            "{note}"
        );

        for (answer, why) in [
            (json!(["a", "b"]), "plain values"),
            (json!("some text"), "not a JSON list or object"),
            (
                json!({ "a": [{ "x": 1 }], "b": [{ "y": 1 }] }),
                "several lists",
            ),
        ] {
            let shape = AnswerShape {
                filters: &["x=1".to_string()],
                fields: None,
                note: None,
            };
            let (out, note) = shape.apply(answer.clone());
            assert_eq!(out, answer);
            let note = note.unwrap();
            assert!(
                note.contains("--where was not applied") && note.contains(why),
                "{note}"
            );
        }
    }

    #[test]
    fn a_file_answer_notes_the_shaping_it_skipped() {
        let fields = ["a".to_string()];
        let shape = AnswerShape {
            filters: &["x=1".to_string()],
            fields: Some(&fields),
            note: None,
        };
        assert_eq!(
            shape.file_note().unwrap(),
            "the call succeeded; --where and --fields were not applied: the answer is a file, and \
             they shape JSON answers"
        );
        assert_eq!(AnswerShape::default().file_note(), None);
    }

    /// `--fields` with item keys still finds the list after `--where` emptied it, and on a page that
    /// came back empty, so the list and its count stay.
    #[test]
    fn fields_keep_a_list_that_where_emptied() {
        let jobs = json!({
            "total_count": 2,
            "jobs": [{ "name": "a", "conclusion": "success" }, { "name": "b", "conclusion": "skipped" }]
        });
        let shape = AnswerShape {
            filters: &["conclusion=failure".to_string()],
            fields: Some(&["name".to_string()]),
            note: None,
        };
        let (out, note) = shape.apply(jobs);
        assert_eq!(out, json!({ "total_count": 2, "jobs": [] }));
        assert!(note.is_some());
        let empty = json!({ "total_count": 0, "jobs": [] });
        assert_eq!(
            project_json_fields(empty.clone(), &["name".to_string()]),
            empty
        );
    }

    #[test]
    fn contains_and_starts_with_and_their_negations() {
        let jobs = json!([
            { "name": "Build & Test (x86_64-pc-windows-msvc)", "path": "src/a.rs" },
            { "name": "Build & Test (x86_64-unknown-linux-gnu)", "path": "docs/src/b.md" },
            { "name": "Lint", "path": "src/c.rs" }
        ]);
        let names = |v: Value| -> Vec<String> {
            v.as_array()
                .unwrap()
                .iter()
                .map(|j| j["name"].as_str().unwrap().to_string())
                .collect()
        };
        let out = filter(jobs.clone(), &["name~=windows|linux"]).unwrap();
        assert_eq!(names(out).len(), 2);
        let out = filter(jobs.clone(), &["name~=WINDOWS"]).unwrap();
        assert_eq!(names(out).len(), 1, "case is ignored");
        let out = filter(jobs.clone(), &["name!~=Build"]).unwrap();
        assert_eq!(names(out), ["Lint"]);
        // A folder: starts with, not contains (`docs/src/` is not under `src/`).
        let out = filter(jobs.clone(), &["path^=src/"]).unwrap();
        assert_eq!(names(out).len(), 2);
        let out = filter(jobs.clone(), &["path~=src/"]).unwrap();
        assert_eq!(names(out).len(), 3);
        let out = filter(jobs, &["path!^=src/"]).unwrap();
        assert_eq!(names(out), ["Build & Test (x86_64-unknown-linux-gnu)"]);
    }

    #[test]
    fn bad_conditions_and_answers_without_a_list_are_errors() {
        for bad in ["result", "=failed", "!=x"] {
            assert!(parse_where(&[bad.to_string()]).is_err(), "{bad}");
        }
        assert!(parse_where(&[" ".to_string()]).unwrap().is_empty());
        assert_eq!(
            WhereCondition::parse("a.b != x| y").unwrap(),
            WhereCondition {
                path: vec!["a".into(), "b".into()],
                values: vec!["x".into(), "y".into()],
                op: WhereOp::Equals,
                negate: true,
                bound: None
            }
        );
        assert_eq!(
            WhereCondition::parse("files.filename !^= src/").unwrap(),
            WhereCondition {
                path: vec!["files".into(), "filename".into()],
                values: vec!["src/".into()],
                op: WhereOp::StartsWith,
                negate: true,
                bound: None
            }
        );

        let one = filter(json!({ "id": 1, "result": "failed" }), &["result=failed"]).unwrap_err();
        assert!(format!("{one}").contains("not a list"), "{one}");
        let two = filter(json!({ "a": [{ "x": 1 }], "b": [{ "x": 2 }] }), &["x=1"]).unwrap_err();
        assert!(format!("{two}").contains("a.x"), "names the fix: {two}");
        assert_eq!(
            filter(json!({ "a": [{ "x": 1 }], "b": [{ "x": 2 }] }), &["a.x=1"]).unwrap(),
            json!({ "a": [{ "x": 1 }], "b": [{ "x": 2 }] })
        );
        assert!(filter(json!("text"), &["x=1"]).is_err());
        assert_eq!(filter(json!("text"), &[]).unwrap(), json!("text"));
    }

    fn work_items() -> Value {
        json!({
            "count": 3,
            "value": [
                { "id": 1, "fields": { "System.State": "Active", "System.Title": "One",
                  "Microsoft.VSTS.Common.Priority": 1, "System.ChangedDate": "2026-11-20T08:00:00.123Z" } },
                { "id": 2, "fields": { "System.State": "Closed", "System.Title": "Two",
                  "Microsoft.VSTS.Common.Priority": 3, "System.ChangedDate": "2026-11-24T23:30:00Z" } },
                { "id": 3, "fields": { "System.State": "Active", "System.Title": "Three",
                  "System.ChangedDate": "2026-11-25T00:00:00+01:00" } }
            ]
        })
    }

    fn ids(value: &Value) -> Vec<i64> {
        value["value"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_i64().unwrap())
            .collect()
    }

    /// Azure DevOps field names contain dots: the longest run of parts that names a key wins.
    #[test]
    fn keys_with_dots_are_found_whole() {
        let out = filter(work_items(), &["fields.System.State=active"]).unwrap();
        assert_eq!(ids(&out), [1, 3]);
        let out = filter(
            work_items(),
            &["fields.Microsoft.VSTS.Common.Priority!=null"],
        )
        .unwrap();
        assert_eq!(ids(&out), [1, 2]);

        let (out, note) = fields_noted(work_items(), &["id", "fields.System.Title"]);
        assert_eq!(note, None);
        assert_eq!(
            out["value"][1],
            json!({ "id": 2, "fields": { "System.Title": "Two" } })
        );

        // A key with dots at the top, and one that is missing: the note names the keys there.
        let (out, _) = fields_noted(json!({ "a.b": { "c": 1 }, "d": 2 }), &["a.b.c"]);
        assert_eq!(out, json!({ "a.b": { "c": 1 } }));
        let (_, note) = fields_noted(work_items(), &["fields.System.Nope"]);
        let note = note.unwrap();
        assert!(
            note.contains("keys under fields") && note.contains("System.State"),
            "{note}"
        );

        let (_, note) = noted(work_items(), &["fields.System.State=Resolved"]).unwrap();
        assert!(note.unwrap().contains("\"Active\", \"Closed\""));
    }

    /// `Commit.Author.Name` kept nothing although the answer has `commit.author.name`.
    #[test]
    fn paths_ignore_case_when_no_key_matches_exactly() {
        let out = filter(work_items(), &["Fields.system.state=active"]).unwrap();
        assert_eq!(ids(&out), [1, 3]);
        let (out, note) = fields_noted(work_items(), &["ID", "FIELDS.System.Title"]);
        assert_eq!(note, None);
        assert_eq!(
            out["value"][1],
            json!({ "id": 2, "fields": { "System.Title": "Two" } }),
            "the answer's own spelling is kept"
        );
        // An exact key wins; two keys that differ only in case leave the path unmatched.
        let items = json!([{ "a": 1, "A": 2 }]);
        assert_eq!(filter(items.clone(), &["A=2"]).unwrap(), items);
        let (out, note) = fields_noted(json!({ "ab": 1, "AB": 2 }), &["Ab"]);
        assert_eq!(out, json!({}));
        assert!(note.is_some());
    }

    #[test]
    fn comparisons_compare_numbers() {
        let out = filter(work_items(), &["fields.Microsoft.VSTS.Common.Priority>=2"]).unwrap();
        assert_eq!(ids(&out), [2]);
        let out = filter(work_items(), &["fields.Microsoft.VSTS.Common.Priority<2"]).unwrap();
        assert_eq!(ids(&out), [1], "a missing field is left out");
        let out = filter(work_items(), &["id>1", "id<=2"]).unwrap();
        assert_eq!(ids(&out), [2]);
        // Numbers in strings count; 10 > 9 as numbers, not as text.
        let items = json!([{ "n": "10" }, { "n": 9 }, { "n": 9.5 }]);
        assert_eq!(
            filter(items, &["n>9"]).unwrap(),
            json!([{ "n": "10" }, { "n": 9.5 }])
        );
        // Inner lists: any element.
        let runs = json!([{ "id": 1, "jobs": [{ "attempt": 1 }, { "attempt": 3 }] }, { "id": 2, "jobs": [{ "attempt": 1 }] }]);
        assert_eq!(filter(runs, &["jobs.attempt>1"]).unwrap()[0]["id"], 1);
    }

    #[test]
    fn comparisons_compare_dates() {
        let changed = "fields.System.ChangedDate";
        // A date alone compares whole days in UTC: item 3 is 2026-11-24T23:00Z.
        let out = filter(work_items(), &[&format!("{changed}>=2026-11-24")]).unwrap();
        assert_eq!(ids(&out), [2, 3]);
        let out = filter(work_items(), &[&format!("{changed}<=2026-11-24")]).unwrap();
        assert_eq!(ids(&out), [1, 2, 3]);
        let out = filter(work_items(), &[&format!("{changed}>2026-11-24")]).unwrap();
        assert!(ids(&out).is_empty());
        let out = filter(work_items(), &[&format!("{changed}<2026-11-24")]).unwrap();
        assert_eq!(ids(&out), [1]);
        // With a time: instants, zones taken into account; no zone is UTC.
        let out = filter(work_items(), &[&format!("{changed}>2026-11-24T23:15:00Z")]).unwrap();
        assert_eq!(ids(&out), [2]);
        let out = filter(work_items(), &[&format!("{changed}>=2026-11-24T23:00")]).unwrap();
        assert_eq!(ids(&out), [2, 3]);
        let out = filter(
            work_items(),
            &[&format!("{changed}<2026-11-25T00:30:00+01:00")],
        )
        .unwrap();
        assert_eq!(ids(&out), [1, 3], "23:30Z is not before 00:30+01:00");
        let out = filter(
            work_items(),
            &[&format!("{changed}<=2026-11-25T00:30:00+01:00")],
        )
        .unwrap();
        assert_eq!(ids(&out), [1, 2, 3]);
    }

    /// With `=` and `!=`, a date alone is the whole day (UTC), as with the comparisons; a full
    /// timestamp, or a field that is itself a date alone, still compares as text.
    #[test]
    fn a_date_alone_with_equals_means_that_day() {
        let changed = "fields.System.ChangedDate";
        let out = filter(work_items(), &[&format!("{changed}=2026-11-24")]).unwrap();
        assert_eq!(ids(&out), [2, 3]);
        let out = filter(work_items(), &[&format!("{changed}!=2026-11-24")]).unwrap();
        assert_eq!(ids(&out), [1]);
        let out = filter(work_items(), &[&format!("{changed}=2026-11-20|2026-11-24")]).unwrap();
        assert_eq!(ids(&out), [1, 2, 3]);
        let days = json!([{ "d": "2026-11-24" }, { "d": "2026-11-25T00:30:00+01:00" }]);
        assert_eq!(
            filter(days, &["d=2026-11-24"])
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn comparisons_explain_what_they_left_out() {
        let items = json!([{ "n": 5 }, { "n": "n/a" }, { "n": null }, { "m": 1 }]);
        let (out, note) = noted(items.clone(), &["n>1"]).unwrap();
        assert_eq!(out, json!([{ "n": 5 }]));
        assert_eq!(
            note.unwrap(),
            "--where 'n>1' left out 1 of the 4 items in the answer whose n is not a number (as: \"n/a\")"
        );
        let (out, note) = noted(items, &["n>2026-01-01"]).unwrap();
        assert_eq!(out, json!([]));
        let note = note.unwrap();
        assert!(
            note.contains("kept none") && note.contains("> compares a date"),
            "{note}"
        );
        // Nothing to say when every value compared.
        let (_, note) = noted(json!([{ "n": 5 }, { "n": 1 }]), &["n>1"]).unwrap();
        assert_eq!(note, None);
    }

    /// A field no item has (a typo) counts as null in each, so `!=` keeps everything: a note says so.
    #[test]
    fn a_field_no_item_has_leaves_a_note() {
        let jobs = json!({ "jobs": [{ "name": "a", "conclusion": "success" }, { "name": "b", "conclusion": "failure" }] });
        let (out, note) = noted(jobs.clone(), &["conclusoin!=success"]).unwrap();
        assert_eq!(out, jobs);
        assert_eq!(
            note.unwrap(),
            "--where 'conclusoin!=success': no item in jobs has conclusoin (keys here: conclusion, \
             name), so it counted as null in each"
        );
        let (_, note) = noted(jobs.clone(), &["conclusoin=success"]).unwrap();
        assert_eq!(
            note.unwrap(),
            "--where 'conclusoin=success' kept none of the 2 items in jobs (no item has conclusoin; keys \
             here: conclusion, name)"
        );
        // Present as null, or in some item only: no note.
        let items = json!([{ "a": null }, { "b": 1 }]);
        assert_eq!(noted(items.clone(), &["a.b!=1"]).unwrap().1, None);
        assert_eq!(noted(items, &["b!=2"]).unwrap().1, None);
    }

    #[test]
    fn equals_compares_numbers_as_numbers() {
        let items = json!([{ "w": 0.0 }, { "w": 3 }, { "w": "0" }, { "w": 2.5 }]);
        assert_eq!(
            filter(items.clone(), &["w=0"]).unwrap(),
            json!([{ "w": 0.0 }, { "w": "0" }])
        );
        assert_eq!(
            filter(items.clone(), &["w=3.0|2.50"]).unwrap(),
            json!([{ "w": 3 }, { "w": 2.5 }])
        );
        assert_eq!(
            filter(items, &["w!=0"]).unwrap(),
            json!([{ "w": 3 }, { "w": 2.5 }])
        );
    }

    #[test]
    fn bad_comparisons_are_errors_that_name_the_fix() {
        let err = |spec: &str| format!("{}", WhereCondition::parse(spec).unwrap_err());
        assert!(err("n>abc").contains("numbers or dates"));
        assert!(err("n>").contains("numbers or dates"));
        assert!(err("n>1|2").contains("one value"));
        assert!(err("n!>1").contains("opposite of > is <="));
        assert!(err("n~<1").contains("not an operator"));
        assert!(err(">1").contains("no field"));
        assert!(err("n>==1").contains("as in n>=1"));
        assert!(err("n=>1").contains("n>=1"));
        assert!(err("d=<2026-11-24").contains("d<=2026-11-24"));
        // `=` with a value that starts with `<` but is no number stays an equals condition.
        assert_eq!(
            filter(json!([{ "t": "<b>" }, { "t": "b" }]), &["t=<b>"]).unwrap(),
            json!([{ "t": "<b>" }])
        );
        for ok in [
            "n>1",
            "n >= -2.5",
            "n<1e3",
            "d<=2026-11-24",
            "d>2026-11-24T10:00:00.5+02:00",
        ] {
            assert!(WhereCondition::parse(ok).is_ok(), "{ok}");
        }
    }

    fn release(assets: Value) -> Value {
        json!({ "tag_name": "v1", "name": "One", "assets": assets, "author": { "login": "me" } })
    }

    #[test]
    fn a_top_level_field_of_an_object_is_an_error_not_a_filter_of_its_list() {
        let with_assets = release(json!([{ "name": "a.tar.gz" }]));
        let err = filter(with_assets.clone(), &["tag_name^=v"])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("tag_name is a field of the answer itself"),
            "{err}"
        );
        assert!(
            err.contains("start the path with assets."),
            "names the list: {err}"
        );
        let err = filter(with_assets, &["name~=tar"]).unwrap_err().to_string();
        assert!(
            err.contains("name is a field of the answer itself"),
            "both have it: {err}"
        );
        // An empty list in an answer that is not a page is not its list: the answer is one object.
        for cond in ["tag_name^=v", "browser_download_url~=x"] {
            let err = filter(release(json!([])), &[cond]).unwrap_err().to_string();
            assert!(err.contains("one object"), "{cond}: {err}");
        }
        for topics in [json!(["rust", "utcp"]), json!([])] {
            let repo = json!({ "name": "agent-misc", "topics": topics, "owner": { "login": "x" } });
            for cond in ["name=agent-misc", "login=x", "foo=x"] {
                let err = filter(repo.clone(), &[cond]).unwrap_err().to_string();
                assert!(err.contains("one object"), "{cond} {topics}: {err}");
                assert!(
                    !err.contains("topics"),
                    "no hint at a list of strings: {err}"
                );
            }
        }
        // An empty page: every other top-level value is plain.
        let page = json!({ "total_count": 0, "workflow_runs": [] });
        assert_eq!(filter(page.clone(), &["conclusion=failure"]).unwrap(), page);
        let err = filter(page, &["total_count=0"]).unwrap_err().to_string();
        assert!(err.contains("its list workflow_runs"), "{err}");
        // A list of objects wins over an empty list.
        let out = filter(json!({ "a": [], "b": [{ "x": 1 }, { "x": 2 }] }), &["x=2"]).unwrap();
        assert_eq!(out, json!({ "a": [], "b": [{ "x": 2 }] }));
    }

    #[test]
    fn a_path_that_is_only_a_list_name_is_an_error_whatever_the_list_holds() {
        for topics in [json!(["rust"]), json!([]), json!([{ "topics": "rust" }])] {
            let repo = json!({ "name": "r", "topics": topics, "owner": { "login": "x" } });
            let err = filter(repo, &["topics=rust"]).unwrap_err().to_string();
            assert!(
                err.contains("topics is a list in the answer"),
                "{topics}: {err}"
            );
        }
        // The items' own field of the same name: `value.value`.
        let props = json!({ "count": 2, "value": [
            { "name": "a", "value": "1" }, { "name": "b", "value": "2" }
        ] });
        let err = filter(props.clone(), &["value=2"]).unwrap_err().to_string();
        assert!(err.contains("value.value"), "{err}");
        assert_eq!(
            filter(props, &["value.value=2"]).unwrap()["value"],
            json!([{ "name": "b", "value": "2" }])
        );
    }

    #[test]
    fn a_list_of_plain_values_is_an_error_but_an_empty_list_is_not() {
        let log = json!({ "count": 2, "value": ["line one", "error here"] });
        let err = filter(log, &["text~=error"]).unwrap_err().to_string();
        assert!(err.contains("value is a list of plain values"), "{err}");
        let err = filter(json!(["a", "b"]), &["x=a"]).unwrap_err().to_string();
        assert!(
            err.contains("the answer is a list of plain values"),
            "{err}"
        );
        let err = filter(json!({ "lines": [1, 2], "id": 3 }), &["lines.x=1"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("lines is a list of plain values"), "{err}");
        assert_eq!(filter(json!([]), &["x=a"]).unwrap(), json!([]));
    }

    #[test]
    fn empty_values_are_refused_where_they_would_match_everything() {
        for bad in ["c~=fail|", "c^=", "c!~=a||b", "c!^= "] {
            let err = parse_where(&[bad.to_string()]).unwrap_err().to_string();
            assert!(err.contains("empty value"), "{bad}: {err}");
        }
        assert_eq!(
            WhereCondition::parse("c=").unwrap().values,
            vec![String::new()],
            "equals the empty string"
        );
        let items = json!([{ "c": "" }, { "c": "x" }]);
        assert_eq!(filter(items, &["c="]).unwrap(), json!([{ "c": "" }]));
    }

    #[test]
    fn a_missing_field_counts_as_null() {
        let issues = json!([
            { "n": 1 },
            { "n": 2, "pull_request": { "url": "u" } },
            { "n": 3, "pull_request": null }
        ]);
        let ns = |v: Value| -> Vec<i64> {
            v.as_array()
                .unwrap()
                .iter()
                .map(|i| i["n"].as_i64().unwrap())
                .collect()
        };
        assert_eq!(
            ns(filter(issues.clone(), &["pull_request=null"]).unwrap()),
            [1, 3]
        );
        assert_eq!(
            ns(filter(issues.clone(), &["pull_request!=null"]).unwrap()),
            [2]
        );
        assert_eq!(
            ns(filter(issues.clone(), &["pull_request.url=null"]).unwrap()),
            [1, 3]
        );
        // An object value never matches: name a field inside it.
        assert!(ns(filter(issues.clone(), &["pull_request=u"]).unwrap()).is_empty());
        assert_eq!(ns(filter(issues, &["pull_request.url=u"]).unwrap()), [2]);
        // A path through an empty inner list has no value, not even null.
        let jobs = json!([{ "n": 1, "steps": [] }, { "n": 2, "steps": [{ "c": null }] }]);
        assert_eq!(ns(filter(jobs, &["steps.c=null"]).unwrap()), [2]);
    }

    #[test]
    fn commas_are_literal_and_an_escaped_bar_is_part_of_the_value() {
        let c = WhereCondition::parse(r"name=test (ubuntu-latest, stable)|lint").unwrap();
        assert_eq!(c.values, ["test (ubuntu-latest, stable)", "lint"]);
        let jobs = json!([{ "name": "test (ubuntu-latest, stable)" }, { "name": "test (ubuntu-latest, beta)" }]);
        assert_eq!(
            filter(jobs, &["name=test (ubuntu-latest, stable)"]).unwrap(),
            json!([{ "name": "test (ubuntu-latest, stable)" }])
        );
        let c = WhereCondition::parse(r"cmd=a \| b|c").unwrap();
        assert_eq!(c.values, ["a | b", "c"]);
        assert_eq!(c.spec(), r"cmd=a \| b|c");
        assert_eq!(
            WhereCondition::parse(r"p=a\b").unwrap().values,
            [r"a\b"],
            "other backslashes stay"
        );
    }

    #[test]
    fn case_is_ignored_by_every_operator() {
        let items =
            json!([{ "c": "Success", "t": "Fix Rustdoc links" }, { "c": "failure", "t": "x" }]);
        for cond in [
            "c=success",
            "c=SUCCESS",
            "c^=succ",
            "t~=rustdoc",
            "t~=RUSTDOC",
        ] {
            assert_eq!(
                filter(items.clone(), &[cond])
                    .unwrap()
                    .as_array()
                    .unwrap()
                    .len(),
                1,
                "{cond}"
            );
        }
        assert_eq!(
            filter(items, &["c!=SUCCESS"]).unwrap(),
            json!([{ "c": "failure", "t": "x" }])
        );
        let nulls = json!([{ "c": null }, { "c": "x" }]);
        assert_eq!(filter(nulls, &["c=NULL"]).unwrap(), json!([{ "c": null }]));
    }

    #[test]
    fn a_condition_that_keeps_none_leaves_a_note_with_the_values_there() {
        let jobs = json!({ "total_count": 4, "jobs": [
            { "conclusion": "success" }, { "conclusion": "skipped" },
            { "conclusion": "success" }, { "other": 1 }
        ] });
        let (out, note) = noted(jobs.clone(), &["conclusion=failed"]).unwrap();
        assert_eq!(
            out,
            json!({ "total_count": 4, "jobs": [] }),
            "the answer is unchanged"
        );
        assert_eq!(
            note.unwrap(),
            r#"--where 'conclusion=failed' kept none of the 4 items in jobs (conclusion here: "success", "skipped", null)"#
        );
        assert_eq!(
            noted(jobs.clone(), &["conclusion=success"]).unwrap().1,
            None
        );
        // Only the condition that emptied the list notes it; an empty page has nothing to note.
        let (_, note) = noted(jobs, &["conclusion=failed", "conclusion=x"]).unwrap();
        assert!(!note.unwrap().contains("conclusion=x"));
        assert_eq!(
            noted(json!({ "count": 0, "value": [] }), &["result=failed"])
                .unwrap()
                .1,
            None
        );
        // The root array, inner lists, numbers, and more than ten values.
        let (_, note) = noted(json!([{ "n": 1 }]), &["n=2"]).unwrap();
        assert_eq!(
            note.unwrap(),
            "--where 'n=2' kept none of the 1 item in the answer (n here: 1)"
        );
        let issues = json!([{ "labels": [{ "name": "bug" }, { "name": "ui" }] }, { "labels": [] }]);
        let (_, note) = noted(issues, &["labels.name=docs"]).unwrap();
        assert!(note
            .unwrap()
            .ends_with(r#"(labels.name here: "bug", "ui")"#));
        let many: Vec<Value> = (0..13).map(|n| json!({ "n": n })).collect();
        let (_, note) = noted(Value::Array(many), &["n=x"]).unwrap();
        assert!(note.unwrap().ends_with("8, 9, and 3 more)"));
        let empty_steps = json!([{ "steps": [] }]);
        let (_, note) = noted(empty_steps, &["steps.c=x"]).unwrap();
        assert!(note.unwrap().ends_with("(no item has a value at steps.c)"));
        // Conditions on two lists each filter their own list, and each can leave a note.
        let compare = json!({ "files": [{ "filename": "a" }], "commits": [{ "sha": "1" }] });
        let (out, note) = noted(compare, &["files.filename=b", "commits.sha^=1"]).unwrap();
        assert_eq!(out, json!({ "files": [], "commits": [{ "sha": "1" }] }));
        assert!(note.unwrap().contains("in files"));
        // A negation that keeps none notes too.
        let (_, note) = noted(json!([{ "c": "a" }]), &["c!=a"]).unwrap();
        assert!(note.is_some());
    }

    fn fields_noted(value: Value, fields: &[&str]) -> (Value, Option<String>) {
        let fields: Vec<String> = fields.iter().map(|s| s.to_string()).collect();
        let shape = AnswerShape {
            filters: &[],
            fields: Some(&fields),
            note: None,
        };
        shape.apply(value)
    }

    /// Top-level paths and item paths together: each is resolved on its own.
    #[test]
    fn fields_mix_top_level_keys_and_item_fields() {
        let jobs = json!({
            "total_count": 2,
            "jobs": [{ "name": "a", "id": 1 }, { "name": "b", "id": 2 }]
        });
        let (out, note) = fields_noted(jobs.clone(), &["name", "total_count"]);
        assert_eq!(
            out,
            json!({ "total_count": 2, "jobs": [{ "name": "a" }, { "name": "b" }] })
        );
        assert_eq!(note, None);
        // A list path and an item path merge into the same items.
        let (out, _) = fields_noted(jobs.clone(), &["jobs.id", "name", "total_count"]);
        assert_eq!(
            out,
            json!({ "total_count": 2, "jobs": [{ "id": 1, "name": "a" }, { "id": 2, "name": "b" }] })
        );
        // An item path no item has: the list is left out, and the note names the items' keys.
        let (out, note) = fields_noted(jobs, &["nmae", "total_count"]);
        assert_eq!(out, json!({ "total_count": 2 }));
        assert_eq!(
            note.unwrap(),
            "--fields 'nmae' matched nothing (keys here: id, name)"
        );
        // An empty list stays, so the count and the list both show.
        let empty = json!({ "total_count": 0, "jobs": [] });
        let (out, note) = fields_noted(empty.clone(), &["name", "total_count"]);
        assert_eq!(out, empty);
        assert_eq!(note, None);
    }

    #[test]
    fn fields_that_match_nothing_leave_a_note_with_the_keys_there() {
        let repo = json!({ "name": "a", "full_name": "o/a", "owner": { "login": "o" } });
        let (out, note) = fields_noted(repo.clone(), &["nmae"]);
        assert_eq!(out, json!({}));
        assert_eq!(
            note.unwrap(),
            "--fields 'nmae' matched nothing (keys here: full_name, name, owner)"
        );
        let (_, note) = fields_noted(repo.clone(), &["name", "owner.logn"]);
        assert_eq!(
            note.unwrap(),
            "--fields 'owner.logn' matched nothing (keys under owner: login)"
        );
        assert_eq!(fields_noted(repo, &["name", "owner.login"]).1, None);
        // Into the list: one item having the field is enough; an empty page has nothing to say.
        let runs = json!({ "total_count": 2, "workflow_runs": [{ "id": 1 }, { "id": 2, "x": 1 }] });
        assert_eq!(fields_noted(runs.clone(), &["x"]).1, None);
        let (_, note) = fields_noted(runs, &["idd"]);
        assert_eq!(
            note.unwrap(),
            "--fields 'idd' matched nothing (keys here: id, x)"
        );
        let empty = json!({ "total_count": 0, "workflow_runs": [] });
        assert_eq!(fields_noted(empty.clone(), &["idd"]).1, None);
        assert_eq!(fields_noted(empty, &["workflow_runs.idd"]).1, None);
        // Text answers can't be projected; a --where note and a --fields note are joined.
        let (out, note) = fields_noted(json!("log line"), &["id"]);
        assert_eq!(out, json!("log line"));
        assert!(note.unwrap().contains("this answer is text"));
        let fields = vec!["nope".to_string()];
        let filters = vec!["id=3".to_string()];
        let shape = AnswerShape {
            filters: &filters,
            fields: Some(&fields),
            note: None,
        };
        let (_, note) = shape.apply(json!([{ "id": 1 }]));
        assert_eq!(
            note.unwrap(),
            "--where 'id=3' kept none of the 1 item in the answer (id here: 1)"
        );
    }

    #[test]
    fn several_lists_suggest_the_one_whose_items_have_the_field() {
        let compare = json!({
            "files": [{ "filename": "a", "sha": "f1" }],
            "commits": [{ "sha": "c1" }]
        });
        let err = filter(compare.clone(), &["filename^=a"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("field of the items of files"), "{err}");
        assert!(err.contains("files.filename"), "{err}");
        assert!(!err.contains("commits.filename"), "{err}");
        let err = filter(compare.clone(), &["sha=c1"])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("the items of commits and files have sha"),
            "{err}"
        );
        let err = filter(compare, &["nope=1"]).unwrap_err().to_string();
        assert!(
            err.contains("none of their items has a field nope"),
            "{err}"
        );
        assert!(
            !err.contains("files.nope"),
            "no example that can't work: {err}"
        );
    }

    #[test]
    fn a_list_name_of_plain_values_says_only_that_it_cannot_be_filtered() {
        let repo = json!({ "name": "r", "topics": ["rust"] });
        let err = filter(repo, &["topics=rust"]).unwrap_err().to_string();
        assert!(err.contains("can't filter topics"), "{err}");
        assert!(!err.contains("topics.topics"), "{err}");
        let objects = json!({ "name": "r", "topics": [{ "topics": "rust" }] });
        let err = filter(objects, &["topics=rust"]).unwrap_err().to_string();
        assert!(err.contains("topics.topics"), "{err}");
        assert!(!err.contains("plain strings"), "{err}");
        let empty = json!({ "name": "r", "topics": [] });
        let err = filter(empty, &["topics=rust"]).unwrap_err().to_string();
        assert!(
            err.contains("topics.topics") && err.contains("plain strings"),
            "{err}"
        );
    }

    #[test]
    fn malformed_conditions_are_errors() {
        let err = parse_where(&["state==open".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("state=open"), "shows the fix: {err}");
        for bad in ["labels..name=x", ".name=x", "labels.=x", "a. .b=x"] {
            let err = parse_where(&[bad.to_string()]).unwrap_err().to_string();
            assert!(err.contains("empty part"), "{bad}: {err}");
        }
        assert!(WhereCondition::parse("a=b=c").unwrap().values == ["b=c"]);
    }
}

#[cfg(test)]
mod mcp_import_tests {
    use super::*;
    use crate::providers::base::{BaseProvider, ProviderType};
    use crate::providers::mcp::McpProvider;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn stdio_provider(command: &str, args: Vec<String>) -> McpProvider {
        McpProvider {
            base: BaseProvider {
                name: "probe".to_string(),
                provider_type: ProviderType::Mcp,
                auth: None,
                allowed_communication_protocols: None,
            },
            url: None,
            headers: None,
            command: Some(command.to_string()),
            args: Some(args),
            env_vars: None,
        }
    }

    #[test]
    fn infers_mcp_names_from_urls_and_launch_commands() {
        let infer = |cmd: Option<&str>, a: &[&str], url: Option<&str>| {
            infer_mcp_manual_name(cmd, &args(a), url)
        };
        assert_eq!(
            infer(None, &[], Some("https://mcp.context7.com/mcp")),
            "context7"
        );
        assert_eq!(
            infer(None, &[], Some("https://api.githubcopilot.com/mcp/")),
            "githubcopilot"
        );
        assert_eq!(
            infer(Some("npx"), &["-y", "@upstash/context7-mcp"], None),
            "context7"
        );
        assert_eq!(
            infer(
                Some("npx"),
                &["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
                None
            ),
            "filesystem"
        );
        assert_eq!(infer(Some("uvx"), &["mcp-server-git@1.2.0"], None), "git");
        assert_eq!(
            infer(Some("python3"), &["-m", "mcp_server_time"], None),
            "time"
        );
        assert_eq!(
            infer(
                Some("docker"),
                &["run", "-i", "--rm", "ghcr.io/github/github-mcp-server"],
                None
            ),
            "github"
        );
        assert_eq!(
            infer(Some("/opt/bin/weather-mcp"), &["--stdio"], None),
            "weather"
        );
        assert_eq!(infer(None, &[], None), "mcp");
    }

    #[tokio::test]
    async fn discovery_times_out_on_a_silent_server() {
        // Starts, reads nothing and answers nothing (node: portable, already a test requirement).
        let prov = stdio_provider("node", args(&["-e", "setTimeout(() => {}, 30000)"]));
        let started = std::time::Instant::now();
        let err = discover_mcp_tools(&prov, std::time::Duration::from_millis(300))
            .await
            .unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert!(err.to_string().contains("did not answer"), "{err:#}");
    }

    #[tokio::test]
    async fn discovery_names_a_command_that_cannot_start() {
        let prov = stdio_provider("/nonexistent/utcp-mcp-probe", Vec::new());
        let err = discover_mcp_tools(&prov, std::time::Duration::from_secs(5))
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("failed to start MCP server command '/nonexistent/utcp-mcp-probe'"),
            "{msg}"
        );
    }
}

#[cfg(test)]
mod hint_tests {
    use super::*;
    use tempfile::TempDir;

    fn test_paths(tmp: &TempDir) -> UtcpPaths {
        let base = tmp.path();
        UtcpPaths {
            config_dir: base.join("config"),
            cache_dir: base.join("cache"),
            state_dir: base.join("state"),
            media_dir: base.join("media"),
            config_file: base.join("config/.utcp_config.json"),
            settings_file: base.join("config/settings.toml"),
            env_file: base.join("config/.env"),
            lock_file: base.join("config/.utcp.lock"),
            manuals_dir: base.join("cache/manuals"),
            search_index_file: base.join("cache/search_index.bin"),
            logging_sentinel: base.join("state/logging.enabled"),
            activity_log_file: base.join("state/activity.jsonl"),
            activity_log_bak: base.join("state/activity.jsonl.bak"),
            socket_endpoint: base.join("cache/utcp.sock").to_string_lossy().into_owned(),
            daemon_pid_file: base.join("state/daemon.pid"),
            separate_setup: true,
        }
    }

    fn entry(name: &str, hint: Option<&str>) -> RegisteredManualEntry {
        RegisteredManualEntry {
            name: name.to_string(),
            source_url: format!("https://example.test/{name}.json"),
            cached: true,
            cached_file: None,
            base_url: None,
            headers: BTreeMap::new(),
            hint: hint.map(str::to_string),
        }
    }

    #[test]
    fn unset_placeholder_note_names_this_machines_env_file() {
        let tmp = TempDir::new().unwrap();
        let paths = test_paths(&tmp);
        let note = unset_placeholders_note(&paths, &BTreeSet::from(["TOKEN".to_string()]));
        assert!(note.contains("not set: TOKEN;"), "{note}");
        assert!(
            note.contains(&paths.env_file.display().to_string()),
            "{note}"
        );
        assert!(unset_placeholders_note(&paths, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn normalizes_whitespace_and_rejects_long_or_control_text() {
        assert_eq!(
            normalize_manual_hint("  library   and\nframework\tdocs ").unwrap(),
            Some("library and framework docs".to_string())
        );
        assert_eq!(normalize_manual_hint("   \n ").unwrap(), None);
        assert_eq!(normalize_manual_hint("").unwrap(), None);
        assert!(normalize_manual_hint("bad\u{7}bell").is_err());
        let at_limit = "x".repeat(MAX_MANUAL_HINT_CHARS);
        assert_eq!(normalize_manual_hint(&at_limit).unwrap(), Some(at_limit));
        let err = normalize_manual_hint(&"x".repeat(MAX_MANUAL_HINT_CHARS + 1)).unwrap_err();
        assert!(format!("{err}").contains("a few words"), "{err}");
    }

    #[test]
    fn set_hint_updates_active_and_deregistered_entries_and_clears() {
        let tmp = TempDir::new().unwrap();
        let paths = test_paths(&tmp);
        let mut reg = UtcpRegistryConfig::default();
        reg.manuals
            .insert("context7".into(), entry("context7", None));
        reg.deregistered.insert("old".into(), entry("old", None));
        reg.save(&paths).unwrap();

        let up = set_manual_hint(&paths, "context7", Some(" library  docs ")).unwrap();
        assert_eq!(
            up,
            ManualHintUpdate {
                hint: Some("library docs".into()),
                active: true
            }
        );
        assert_eq!(
            manual_hints(&paths).get("context7").map(String::as_str),
            Some("library docs")
        );

        let up = set_manual_hint(&paths, "old", Some("legacy")).unwrap();
        assert!(!up.active);
        let reg = UtcpRegistryConfig::load(&paths).unwrap();
        assert_eq!(reg.deregistered["old"].hint.as_deref(), Some("legacy"));
        assert!(
            !manual_hints(&paths).contains_key("old"),
            "deregistered manuals are not in the skill"
        );

        let up = set_manual_hint(&paths, "context7", None).unwrap();
        assert_eq!(up.hint, None);
        let up = set_manual_hint(&paths, "old", Some("  ")).unwrap();
        assert_eq!(up.hint, None);
        assert!(manual_hints(&paths).is_empty());

        let err = set_manual_hint(&paths, "nope", Some("x")).unwrap_err();
        assert!(format!("{err}").contains("known: context7, old"), "{err}");
    }

    #[test]
    fn hint_words_are_searchable_until_the_hint_is_cleared() {
        let tmp = TempDir::new().unwrap();
        let paths = test_paths(&tmp);
        std::fs::create_dir_all(&paths.manuals_dir).unwrap();
        let manual = serde_json::json!({
            "manual_version": "1.0.0",
            "utcp_version": "1.0.0",
            "info": { "title": "c7", "version": "1", "description": "c7" },
            "_meta": {
                "manual_name": "c7",
                "source_url": "https://example.test/c7.json",
                "sha256": "00",
                "fetched_at": "2026-01-01T00:00:00Z",
                "tool_count": 1,
                "protocols": ["http"],
                "required_env_vars": [],
            },
            "tools": [{
                "name": "query",
                "description": "Answers questions.",
                "inputs": { "type": "object" },
                "outputs": { "type": "object" },
                "provider": { "provider_type": "http", "url": "http://localhost" },
            }],
        });
        std::fs::write(paths.cached_manual_path("c7"), manual.to_string()).unwrap();
        let mut reg = UtcpRegistryConfig::default();
        reg.manuals.insert("c7".into(), entry("c7", None));
        reg.save(&paths).unwrap();

        let names = |q: &str| -> Vec<String> {
            search_tools_sync(&paths, q, 10, false, false)
                .unwrap()
                .into_iter()
                .map(|t| t.name)
                .collect()
        };
        assert!(names("framework").is_empty());

        set_manual_hint(&paths, "c7", Some("library and framework docs")).unwrap();
        assert_eq!(names("framework"), vec!["c7.query".to_string()]);
        assert_eq!(names("library docs"), vec!["c7.query".to_string()]);

        set_manual_hint(&paths, "c7", None).unwrap();
        assert!(names("framework").is_empty());
    }

    #[test]
    fn hint_is_omitted_from_json_when_unset_and_read_from_older_files() {
        let json = serde_json::to_string(&entry("d2", None)).unwrap();
        assert!(!json.contains("hint"), "{json}");
        let parsed: RegisteredManualEntry =
            serde_json::from_str(r#"{"name":"d2","source_url":"x","cached":true}"#).unwrap();
        assert_eq!(parsed.hint, None);
    }

    #[test]
    fn activate_keeps_previous_hint_unless_a_new_one_is_given() {
        let mut reg = UtcpRegistryConfig::default();
        reg.activate("c7", entry("c7", Some("library docs")));
        // Re-registering from a source without --hint keeps the hint.
        reg.activate("c7", entry("c7", None));
        assert_eq!(reg.manuals["c7"].hint.as_deref(), Some("library docs"));
        // A new hint wins.
        reg.activate("c7", entry("c7", Some("docs")));
        assert_eq!(reg.manuals["c7"].hint.as_deref(), Some("docs"));

        // A deregistered entry's hint carries over to a fresh registration.
        let kept = reg.manuals.remove("c7").unwrap();
        reg.deregistered.insert("c7".into(), kept);
        reg.activate("c7", entry("c7", None));
        assert!(reg.deregistered.is_empty());
        assert_eq!(reg.manuals["c7"].hint.as_deref(), Some("docs"));
    }
}
