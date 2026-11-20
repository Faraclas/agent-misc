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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchRequestItem {
    pub id: String,
    pub tool: String,
    #[serde(default)]
    pub args: HashMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchResultItem {
    pub id: String,
    pub tool: String,
    pub ok: bool,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub latency_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub config_file: String,
    pub cache_dir: String,
    pub state_dir: String,
    pub socket_endpoint: String,
    pub daemon_running: bool,
    pub daemon_pid: Option<u32>,
    pub logging_enabled: bool,
    pub cache_on_register: bool,
    pub index_present: bool,
    pub manual_count: usize,
    pub tool_count: usize,
    pub credentials: Vec<CredentialStatus>,
    /// The last run of `[secrets] command`; `None` when none is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets_command: Option<crate::secrets::SecretsCommandStatus>,
    /// Why `settings.toml` could not be used (utcp then runs with the default settings).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings_error: Option<String>,
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
    fn activate(&mut self, name: &str, mut entry: RegisteredManualEntry) {
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

/// Substitute `${VAR}` placeholders inside a JSON value using `env_map` (or `"***REDACTED***"` when `redact == true`).
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
                    if redact {
                        return format!("***REDACTED:{}***", var);
                    }
                    resolve_secret_var(manual_name, var, env_map)
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
    let mut files: Vec<PathBuf> = [active.as_ref(), kept.as_ref()]
        .into_iter()
        .flatten()
        .filter_map(|e| e.cached_file.as_ref().map(PathBuf::from))
        .collect();
    files.push(paths.cached_manual_path(name));
    let mut removed_file = false;
    for f in files {
        if fs::remove_file(&f).is_ok() {
            removed_file = true;
        }
    }

    if active.is_none() && kept.is_none() {
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
fn read_saved_manual(
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

/// Re-fetch and update one or all registered manuals and rebuild `search_index.bin`: from their
/// recorded `source_url`, or for `import-mcp` manuals by asking the MCP server again.
pub async fn refresh_manuals(
    paths: &UtcpPaths,
    target_name: Option<&str>,
) -> Result<Vec<ImportSummary>> {
    let _lock = paths.acquire_write_lock()?;
    let mut registry = UtcpRegistryConfig::load(paths)?;
    let names: Vec<String> = match target_name {
        Some(n) => {
            if !registry.manuals.contains_key(n) {
                return Err(anyhow!("Manual '{}' is not registered", n));
            }
            vec![n.to_string()]
        }
        None => registry.manuals.keys().cloned().collect(),
    };

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

fn rebuild_search_index_locked(
    paths: &UtcpPaths,
    registry: &UtcpRegistryConfig,
) -> Result<SearchIndex> {
    let mut manuals = Vec::with_capacity(registry.manuals.len());
    let mut tools = Vec::new();

    for (name, entry) in &registry.manuals {
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

    let index = SearchIndex {
        magic: SEARCH_INDEX_MAGIC,
        built_at_unix_ms,
        manuals,
        tools,
    };
    index.save_to_disk(paths)?;
    Ok(index)
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

pub fn list_tools_sync(paths: &UtcpPaths, manual: Option<&str>) -> Result<Vec<ToolSummary>> {
    let idx = load_or_build_index_sync(paths)?;
    Ok(idx.list_tools(manual))
}

pub fn search_tools_sync(
    paths: &UtcpPaths,
    query: &str,
    limit: usize,
    include_schemas: bool,
) -> Result<Vec<ToolSummary>> {
    let idx = load_or_build_index_sync(paths)?;
    Ok(idx.search(query, limit, include_schemas))
}

pub fn get_schema_sync(paths: &UtcpPaths, target: &str) -> Result<ToolSchemaDetail> {
    let idx = load_or_build_index_sync(paths)?;
    idx.get_schema(target)
        .ok_or_else(|| anyhow!("Tool '{}' not found in registered manuals", target))
}

/// Load a single manual's cached JSON file directly (`O(1)` 1-file read from OS page cache).
pub fn load_single_cached_manual(paths: &UtcpPaths, manual_name: &str) -> Result<CachedUtcpManual> {
    let direct_path = paths.cached_manual_path(manual_name);
    if direct_path.exists() {
        let bytes = fs::read(&direct_path)?;
        return Ok(serde_json::from_slice(&bytes)?);
    }
    let registry = UtcpRegistryConfig::load(paths)?;
    let entry = registry
        .manuals
        .get(manual_name)
        .ok_or_else(|| anyhow!("Manual '{}' is not registered", manual_name))?;
    let cf = entry
        .cached_file
        .as_ref()
        .ok_or_else(|| anyhow!("Manual '{}' has no cached manual file", manual_name))?;
    let bytes = fs::read(cf)?;
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

/// Resolve a tool call into a dry-run preview JSON with all secrets redacted.
pub fn dry_run_tool_from_manual(
    manual: &CachedUtcpManual,
    manual_name: &str,
    tool_name: &str,
    args: &HashMap<String, Value>,
    env_map: &HashMap<String, String>,
) -> Result<Value> {
    let entry = manual
        .tools
        .iter()
        .find(|t| t.name.eq_ignore_ascii_case(tool_name))
        .ok_or_else(|| anyhow!("Tool '{}.{}' not found", manual_name, tool_name))?;

    let mut provider_val = entry.provider.clone();
    interpolate_provider_vars(&mut provider_val, manual_name, env_map, true);
    let protocol = extract_protocol_name(&provider_val);

    if protocol == "http" {
        if let Ok(http_prov) = serde_json::from_value::<HttpProvider>(provider_val.clone()) {
            let mut rem = args.clone();
            let mut url = http_prov.url.clone();
            let mut consumed = Vec::new();
            for (k, v) in &rem {
                let ph = format!("{{{}}}", k);
                if url.contains(&ph) {
                    let raw = v
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| v.to_string());
                    url = url.replace(&ph, &urlencoding::encode(&raw));
                    consumed.push(k.clone());
                }
            }
            for k in consumed {
                rem.remove(&k);
            }
            return Ok(json!({
                "dry_run": true,
                "tool": format!("{}.{}", manual_name, entry.name),
                "protocol": "http",
                "method": http_prov.http_method,
                "url": url,
                "headers": http_prov.headers,
                "auth": http_prov.base.auth,
                "args": rem,
            }));
        }
    }

    Ok(json!({
        "dry_run": true,
        "tool": format!("{}.{}", manual_name, entry.name),
        "protocol": protocol,
        "provider": provider_val,
        "args": args,
    }))
}

/// Execute a single tool given an already-loaded `CachedUtcpManual`, `env_map`, and `CommunicationProtocolRegistry`.
/// Automatically intercepts inline base64 media payloads into `paths.media_dir` (`tmpfs` on Linux).
pub async fn execute_loaded_tool(
    manual: &CachedUtcpManual,
    manual_name: &str,
    tool_name: &str,
    args: HashMap<String, Value>,
    env_map: &HashMap<String, String>,
    protocols: &CommunicationProtocolRegistry,
    paths: &UtcpPaths,
) -> Result<Value> {
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
    }

    let provider = create_provider_from_value(provider_val, 0)?;
    let proto_key = provider.type_().as_key();
    let transport = protocols
        .get(proto_key)
        .ok_or_else(|| anyhow!("Communication protocol '{}' is not enabled", proto_key))?;

    let call_name = format!("{}.{}", provider.name(), entry.name);
    let mut output = transport
        .call_tool(&call_name, args, provider.as_ref())
        .await?;
    crate::media::intercept_inline_media(&mut output, paths);
    Ok(output)
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

/// Generic post-filter for verbose JSON API responses (`--fields sha,commit.message,html_url`).
/// - Returns `value` completely untouched when `fields` is empty.
/// - Supports both top-level keys (`sha`) and dotted nested paths (`commit.message`, `commit.author.name`).
/// - A path that reaches an array applies the rest of the path to every element
///   (`workflow_runs.id`, `assets.name`).
/// - When no field names a top-level key, projects into the response's list: `items`, `results` or
///   `data`, else its only array of objects (`{ "total_count": N, "workflow_runs": [...] }`).
pub fn project_json_fields(value: Value, fields: &[String]) -> Value {
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
        Value::Object(mut map) => {
            let any_top_match = cleaned.iter().any(|spec| {
                let first = spec.split('.').next().unwrap_or(spec);
                map.contains_key(first)
            });

            if !any_top_match {
                if let Some(envelope_key) = find_envelope_list_key(&map) {
                    if let Some(Value::Array(arr)) = map.remove(&envelope_key) {
                        let projected = Value::Array(
                            arr.into_iter()
                                .map(|elem| project_single_object(elem, &cleaned))
                                .collect(),
                        );
                        map.insert(envelope_key, projected);
                        return Value::Object(map);
                    }
                }
            }

            project_single_object(Value::Object(map), &cleaned)
        }
        other => other,
    }
}

/// The list a collection response wraps: a well-known envelope key, else its only array of objects.
fn find_envelope_list_key(map: &serde_json::Map<String, Value>) -> Option<String> {
    for key in ["items", "results", "data"] {
        if matches!(map.get(key), Some(Value::Array(_))) {
            return Some(key.to_string());
        }
    }
    let mut object_lists = map.iter().filter(|(_, v)| match v {
        Value::Array(arr) => arr.iter().any(Value::is_object),
        _ => false,
    });
    match (object_lists.next(), object_lists.next()) {
        (Some((key, _)), None) => Some(key.clone()),
        _ => None,
    }
}

fn project_single_object(val: Value, specs: &[&str]) -> Value {
    if !val.is_object() {
        return val;
    }
    let mut out = Value::Object(serde_json::Map::new());
    for spec in specs {
        let parts: Vec<&str> = spec.split('.').filter(|p| !p.is_empty()).collect();
        insert_projected_path(&val, &parts, &mut out);
    }
    out
}

/// Copy `parts` from `src` into `dst`. Objects descend by key; arrays apply the remaining path to every
/// element, so `dst` keeps the array's length and order. Scalars a path cannot descend into stay as is
/// inside arrays and are dropped elsewhere.
fn insert_projected_path(src: &Value, parts: &[&str], dst: &mut Value) {
    let Some((&first, rest)) = parts.split_first() else {
        *dst = src.clone();
        return;
    };
    match src {
        Value::Object(src_map) => {
            if !dst.is_object() {
                *dst = Value::Object(serde_json::Map::new());
            }
            let Some(found) = src_map.get(first) else {
                return;
            };
            if !rest.is_empty() && !(found.is_object() || found.is_array()) {
                return;
            }
            if let Value::Object(dst_map) = dst {
                let slot = dst_map.entry(first.to_string()).or_insert(Value::Null);
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

/// Direct Mode single-tool execution: reads ONLY `manuals/<manual>.utcp.json` (and `.env` only when `${` is present).
pub async fn call_tool_direct(
    paths: &UtcpPaths,
    target: &str,
    args: HashMap<String, Value>,
    dry_run: bool,
    fields: Option<&[String]>,
) -> Result<Value> {
    let (manual_name, tool_name) = resolve_manual_and_tool(target, None, paths)?;
    let manual = load_single_cached_manual(paths, &manual_name)?;
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
        return dry_run_tool_from_manual(&manual, &manual_name, &tool_name, &args, &env_map);
    }

    let protocols =
        CommunicationProtocolRegistry::with_shared_http_client(build_secure_http_client());
    let val = execute_loaded_tool(
        &manual,
        &manual_name,
        &tool_name,
        args,
        &env_map,
        &protocols,
        paths,
    )
    .await?;
    Ok(match fields {
        Some(f) if !f.is_empty() => project_json_fields(val, f),
        _ => val,
    })
}

/// Execute a batch of tool calls concurrently in Direct Mode over a single shared HTTP/TLS connection pool.
pub async fn batch_call_direct(
    paths: &UtcpPaths,
    items: Vec<BatchRequestItem>,
    concurrency: usize,
) -> Result<Vec<BatchResultItem>> {
    let index = load_or_build_index_sync(paths)?;
    let env_map = load_env_map(paths);
    let protocols =
        CommunicationProtocolRegistry::with_shared_http_client(build_secure_http_client());

    let mut manual_cache: HashMap<String, CachedUtcpManual> = HashMap::new();
    for item in &items {
        if let Ok((mname, _)) = resolve_manual_and_tool(&item.tool, Some(&index), paths) {
            if let std::collections::hash_map::Entry::Vacant(slot) = manual_cache.entry(mname) {
                if let Ok(m) = load_single_cached_manual(paths, slot.key()) {
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
            async move {
                let started = Instant::now();
                let item_fields = item.fields.clone();
                let outcome = async {
                    let (mname, tname) = resolve_manual_and_tool(&item.tool, Some(index), paths)?;
                    let manual = manual_cache
                        .get(&mname)
                        .ok_or_else(|| anyhow!("Manual '{}' not found", mname))?;
                    let val = execute_loaded_tool(
                        manual, &mname, &tname, item.args, env_map, &protocols, paths,
                    )
                    .await?;
                    Ok::<Value, anyhow::Error>(match item_fields.as_deref() {
                        Some(f) if !f.is_empty() => project_json_fields(val, f),
                        _ => val,
                    })
                }
                .await;
                let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
                match outcome {
                    Ok(val) => (
                        idx,
                        BatchResultItem {
                            id: item.id,
                            tool: item.tool,
                            ok: true,
                            status: "OK".to_string(),
                            result: Some(val),
                            error: None,
                            latency_ms,
                        },
                    ),
                    Err(err) => (
                        idx,
                        BatchResultItem {
                            id: item.id,
                            tool: item.tool,
                            ok: false,
                            status: "ERR".to_string(),
                            result: None,
                            error: Some(err.to_string()),
                            latency_ms,
                        },
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
    let logging_enabled = crate::activity::is_logging_enabled(paths);
    let index_present = paths.search_index_file.exists();
    let idx = load_or_build_index_sync(paths)?;
    let mut env_map = load_file_env_map(paths);
    // With the daemon running, `doctor` reports the daemon's credentials and secrets command run.
    let secrets = settings
        .secrets
        .as_ref()
        .filter(|_| !daemon_running)
        .map(|secrets| {
            run_direct_secrets(paths, secrets, &idx.env_var_names()).unwrap_or_else(|| {
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
    for m in &idx.manuals {
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
        logging_enabled,
        cache_on_register: settings.cache_on_register,
        index_present,
        manual_count: idx.manuals.len(),
        tool_count: idx.tools.len(),
        credentials,
        secrets_command: secrets.map(|run| run.status),
        settings_error,
    })
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
            search_tools_sync(&paths, q, 10, false)
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
