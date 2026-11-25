//! The staging area: `utcp stage` adds a manual next to the active ones without making it active. A staged manual
//! is not in `list` or the agent skills, and `search` and calls reach it only with `--staged` (with the same secrets
//! as the active manuals), so it can be reviewed (`diff`) and tested before `promote` makes it active. `promote` keeps the
//! registration it replaced for `rollback`. Like git's index: one staging area, at most one staged copy per name.

use crate::catalog::{
    build_index, convert_bytes_to_cached_manual, fetch_source_bytes, normalize_manual_hint,
    read_saved_manual, rebuild_search_index_locked, select_manual_names, CachedUtcpManual,
    ImportSummary, RegisteredManualEntry, UtcpRegistryConfig,
};
use crate::index::SearchIndex;
use crate::paths::{write_atomic, UtcpPaths, UtcpSettings};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

/// What a staged manual replaces when it is promoted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Replaces {
    /// No manual of that name: promoting adds it.
    Nothing,
    /// The active manual of that name.
    Active,
    /// A deregistered manual of that name (its kept settings and saved copy).
    Deregistered,
}

impl Replaces {
    fn of(registry: &UtcpRegistryConfig, name: &str) -> Self {
        if registry.manuals.contains_key(name) {
            Self::Active
        } else if registry.deregistered.contains_key(name) {
            Self::Deregistered
        } else {
            Self::Nothing
        }
    }
}

/// One staged manual (`utcp list --staged`, `utcp diff`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagedManual {
    pub name: String,
    pub tool_count: usize,
    pub source_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    pub replaces: Replaces,
    pub required_env_vars: Vec<String>,
}

/// A staged manual compared with the manual it replaces (`utcp diff`). For a new manual every tool is `added`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManualDiff {
    pub name: String,
    pub replaces: Replaces,
    pub tool_count: usize,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// Tools in both whose description, inputs, outputs, tags or request differ, and how.
    pub changed: Vec<ToolChange>,
    pub unchanged: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint_before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint_after: Option<String>,
}

/// A tool in both versions that differs (`utcp diff`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolChange {
    pub tool: String,
    /// What differs, one entry per part: `description`, `inputs added: a, b`, `inputs removed: c`,
    /// `now required: d`, `no longer required: e`, `inputs changed: f`, `outputs`, `tags`,
    /// `method: GET -> POST`, `url: <old> -> <new>`, `auth`, `headers`, or another key's name. Header
    /// and auth values are never shown.
    pub what: Vec<String>,
}

/// How tool `old` became `new` (both as saved in a manual), for [`ToolChange::what`].
fn tool_changes(old: &serde_json::Value, new: &serde_json::Value) -> Vec<String> {
    use serde_json::Value;
    let empty = serde_json::Map::new();
    let obj = |v: &Value| v.as_object().cloned().unwrap_or_default();
    let (old, new) = (obj(old), obj(new));
    let keys: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
    let mut what = Vec::new();
    for key in keys {
        let (a, b) = (old.get(key), new.get(key));
        if a == b {
            continue;
        }
        match key.as_str() {
            "inputs" => {
                let props = |v: Option<&Value>| {
                    v.and_then(|v| v.get("properties"))
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default()
                };
                let required = |v: Option<&Value>| -> BTreeSet<String> {
                    v.and_then(|v| v.get("required"))
                        .and_then(Value::as_array)
                        .map(|r| {
                            r.iter()
                                .filter_map(|s| s.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default()
                };
                let (pa, pb) = (props(a), props(b));
                let (ra, rb) = (required(a), required(b));
                let list = |names: Vec<&String>| {
                    names
                        .into_iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let before = what.len();
                let added: Vec<&String> = pb.keys().filter(|k| !pa.contains_key(*k)).collect();
                let removed: Vec<&String> = pa.keys().filter(|k| !pb.contains_key(*k)).collect();
                let changed: Vec<&String> = pa
                    .iter()
                    .filter(|(k, v)| pb.get(*k).is_some_and(|w| w != *v))
                    .map(|(k, _)| k)
                    .collect();
                let now: Vec<&String> = rb.difference(&ra).collect();
                let no_longer: Vec<&String> =
                    ra.difference(&rb).filter(|k| pb.contains_key(*k)).collect();
                for (label, names) in [
                    ("inputs added", added),
                    ("inputs removed", removed),
                    ("now required", now),
                    ("no longer required", no_longer),
                    ("inputs changed", changed),
                ] {
                    if !names.is_empty() {
                        what.push(format!("{label}: {}", list(names)));
                    }
                }
                if what.len() == before {
                    what.push("inputs".to_string());
                }
            }
            "provider" | "tool_provider" | "tool_call_template" => {
                let (pa, pb) = (
                    a.and_then(Value::as_object).unwrap_or(&empty),
                    b.and_then(Value::as_object).unwrap_or(&empty),
                );
                let sub: BTreeSet<&String> = pa.keys().chain(pb.keys()).collect();
                for k in sub {
                    let (x, y) = (pa.get(k), pb.get(k));
                    if x == y {
                        continue;
                    }
                    let shown = |v: Option<&Value>| match v {
                        Some(Value::String(s)) => s.clone(),
                        Some(v) => v.to_string(),
                        None => "none".to_string(),
                    };
                    match k.as_str() {
                        "http_method" => what.push(format!("method: {} -> {}", shown(x), shown(y))),
                        "url" => what.push(format!("url: {} -> {}", shown(x), shown(y))),
                        other => what.push(other.to_string()),
                    }
                }
            }
            other => what.push(other.to_string()),
        }
    }
    // A cached tool keeps its request as both `provider` and `tool_call_template`; name each change once.
    let mut seen = BTreeSet::new();
    what.retain(|w| seen.insert(w.clone()));
    what
}

/// One manual made active by `utcp promote`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Promoted {
    pub name: String,
    pub tool_count: usize,
    pub replaced: Replaces,
    /// Whether the replaced registration was kept for `utcp rollback`.
    pub rollback_available: bool,
}

/// Result of `utcp rollback`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RolledBack {
    pub name: String,
    pub tool_count: usize,
    /// Whether the version it replaced is now the previous one (so a second rollback undoes the first).
    pub undo_available: bool,
}

fn entry_file(paths: &UtcpPaths, name: &str, entry: &RegisteredManualEntry) -> PathBuf {
    entry
        .cached_file
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| paths.cached_manual_path(name))
}

fn names_list(names: impl IntoIterator<Item = impl AsRef<str>>) -> String {
    let names: Vec<String> = names.into_iter().map(|n| n.as_ref().to_string()).collect();
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

/// The staged names `names` selects (every one when empty); see [`select_manual_names`].
fn selected(registry: &UtcpRegistryConfig, names: &[String]) -> Result<Vec<String>> {
    select_manual_names(registry.staged.keys(), names, "staged")
}

/// Fetch and convert `source` like `register`, but put it in the staging area. A staged copy of the same name is
/// replaced. Without `hint`, the hint of the staged, active or deregistered manual of that name is kept. The active
/// manuals, the search index and the skills are not touched; tell a running daemon to reload so it can call it.
pub async fn stage_manual(
    paths: &UtcpPaths,
    name: &str,
    source: &str,
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
    let (bytes, resolved_source) = fetch_source_bytes(paths, name, source, &headers).await?;
    let manual =
        convert_bytes_to_cached_manual(&bytes, name, &resolved_source, base_url, &headers)?;

    let staged_path = paths.staged_manual_path(name);
    write_atomic(&staged_path, &serde_json::to_vec_pretty(&manual)?)?;

    let mut registry = UtcpRegistryConfig::load(paths)?;
    let hint = hint.or_else(|| {
        registry
            .staged
            .get(name)
            .or_else(|| registry.manuals.get(name))
            .or_else(|| registry.deregistered.get(name))
            .and_then(|e| e.hint.clone())
    });
    registry.staged.insert(
        name.to_string(),
        RegisteredManualEntry {
            name: name.to_string(),
            source_url: resolved_source.clone(),
            cached: settings.cache_on_register,
            cached_file: Some(staged_path.to_string_lossy().into_owned()),
            base_url: base_url.map(str::to_string),
            headers,
            hint,
        },
    );
    registry.save(paths)?;

    Ok(ImportSummary {
        manual_name: name.to_string(),
        output_path: staged_path.to_string_lossy().into_owned(),
        source_url: resolved_source,
        sha256: manual.meta.sha256,
        tool_count: manual.meta.tool_count,
        protocols: manual.meta.protocols,
        required_env_vars: manual.meta.required_env_vars,
    })
}

/// An in-memory index of the staged manuals (never saved), for `list`, `schema` and `call` with `--staged`.
pub fn staged_index(paths: &UtcpPaths) -> Result<SearchIndex> {
    let registry = UtcpRegistryConfig::load(paths)?;
    Ok(build_index(paths, &registry.staged))
}

/// The staged copy of `name`.
pub fn load_staged_manual(paths: &UtcpPaths, name: &str) -> Result<CachedUtcpManual> {
    let registry = UtcpRegistryConfig::load(paths)?;
    let entry = registry.staged.get(name).ok_or_else(|| {
        anyhow!(
            "'{}' is not staged (staged: {})",
            name,
            names_list(registry.staged.keys())
        )
    })?;
    let file = entry_file(paths, name, entry);
    let bytes = fs::read(&file)
        .with_context(|| format!("cannot read the staged copy {}", file.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("the staged copy {} is not a utcp manual", file.display()))
}

/// Every staged manual, sorted by name.
pub fn list_staged(paths: &UtcpPaths) -> Result<Vec<StagedManual>> {
    let registry = UtcpRegistryConfig::load(paths)?;
    Ok(registry
        .staged
        .iter()
        .map(|(name, entry)| {
            let saved = read_saved_manual(name, entry, &entry_file(paths, name, entry));
            StagedManual {
                name: name.clone(),
                tool_count: saved.as_ref().map_or(0, |(m, _)| m.tool_count),
                source_url: entry.source_url.clone(),
                hint: entry.hint.clone(),
                replaces: Replaces::of(&registry, name),
                required_env_vars: saved.map(|(m, _)| m.required_env_vars).unwrap_or_default(),
            }
        })
        .collect())
}

/// The tools of staged manual `name` and of the manual it replaces (empty when none), by name, as saved.
type ToolMap = BTreeMap<String, serde_json::Value>;
fn staged_and_old_tools(
    paths: &UtcpPaths,
    registry: &UtcpRegistryConfig,
    name: &str,
) -> Result<(ToolMap, ToolMap)> {
    let staged = load_staged_manual(paths, name)?;
    let old: Option<CachedUtcpManual> = registry
        .manuals
        .get(name)
        .or_else(|| registry.deregistered.get(name))
        .and_then(|e| {
            let bytes = fs::read(entry_file(paths, name, e)).ok()?;
            serde_json::from_slice(&bytes).ok()
        });
    let as_map = |m: &CachedUtcpManual| -> ToolMap {
        m.tools
            .iter()
            .map(|t| (t.name.clone(), serde_json::to_value(t).unwrap_or_default()))
            .collect()
    };
    Ok((
        as_map(&staged),
        old.as_ref().map(as_map).unwrap_or_default(),
    ))
}

/// How one tool of a staged manual compares with the version it replaces (`utcp diff <manual>.<tool>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    /// Only in the staged manual.
    Added,
    /// Only in the manual the staged one replaces.
    Removed,
    Changed,
    Unchanged,
}

/// One tool compared part by part (`utcp diff <manual>.<tool>`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDiff {
    pub manual: String,
    pub tool: String,
    pub replaces: Replaces,
    pub status: ToolStatus,
    /// Each part that differs, with its old and new value (`null` where it is absent).
    pub parts: Vec<PartChange>,
}

/// One part of a tool that differs: `description`, `input <name>` (its schema with `"required"`), `inputs` (the
/// rest of the input schema), `method`, `url`, `auth`, `headers`, another request key, or another top-level key
/// (`outputs`, `tags`). Auth and header values are shown as `***` unless they are a `${VAR}` placeholder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PartChange {
    pub part: String,
    pub old: serde_json::Value,
    pub new: serde_json::Value,
}

/// `v` with every string replaced by `***`, except `${VAR}` placeholders and the values of keys that name a kind
/// rather than hold a secret (`auth_type`, `location`, `var_name`).
fn redact_secrets(v: &serde_json::Value, key: Option<&str>) -> serde_json::Value {
    use serde_json::Value;
    match v {
        Value::String(s) => {
            let placeholder = s.starts_with("${")
                && s.ends_with('}')
                && s[2..s.len() - 1]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_');
            let plain = matches!(key, Some("auth_type" | "location" | "var_name"));
            if placeholder || plain {
                v.clone()
            } else {
                Value::String("***".to_string())
            }
        }
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| (k.clone(), redact_secrets(x, Some(k))))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(|x| redact_secrets(x, key)).collect()),
        other => other.clone(),
    }
}

/// The parts of tool `old` that differ in `new` (both as saved in a manual), with their values.
fn tool_parts(old: &serde_json::Value, new: &serde_json::Value) -> Vec<PartChange> {
    use serde_json::Value;
    let empty = serde_json::Map::new();
    let obj = |v: &Value| v.as_object().cloned().unwrap_or_default();
    let (old, new) = (obj(old), obj(new));
    let keys: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
    let mut parts = Vec::new();
    fn push(parts: &mut Vec<PartChange>, part: String, a: Value, b: Value) {
        if a != b {
            parts.push(PartChange {
                part,
                old: a,
                new: b,
            });
        }
    }
    for key in keys {
        let (a, b) = (old.get(key), new.get(key));
        if a == b {
            continue;
        }
        match key.as_str() {
            "inputs" => {
                let split = |v: Option<&Value>| {
                    let mut rest = v.and_then(Value::as_object).cloned().unwrap_or_default();
                    let props = rest
                        .remove("properties")
                        .and_then(|p| p.as_object().cloned())
                        .unwrap_or_default();
                    let required: BTreeSet<String> = rest
                        .remove("required")
                        .and_then(|r| r.as_array().cloned())
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|s| s.as_str().map(String::from))
                        .collect();
                    (props, required, rest)
                };
                let (pa, ra, rest_a) = split(a);
                let (pb, rb, rest_b) = split(b);
                let with_required = |schema: Option<&Value>, required: bool| match schema {
                    Some(Value::Object(m)) => {
                        let mut m = m.clone();
                        m.insert("required".to_string(), Value::Bool(required));
                        Value::Object(m)
                    }
                    Some(other) => other.clone(),
                    None => Value::Null,
                };
                let names: BTreeSet<&String> = pa.keys().chain(pb.keys()).collect();
                for n in names {
                    push(
                        &mut parts,
                        format!("input {n}"),
                        with_required(pa.get(n), ra.contains(n)),
                        with_required(pb.get(n), rb.contains(n)),
                    );
                }
                push(
                    &mut parts,
                    "inputs".to_string(),
                    Value::Object(rest_a),
                    Value::Object(rest_b),
                );
            }
            "provider" | "tool_provider" | "tool_call_template" => {
                let (pa, pb) = (
                    a.and_then(Value::as_object).unwrap_or(&empty),
                    b.and_then(Value::as_object).unwrap_or(&empty),
                );
                let sub: BTreeSet<&String> = pa.keys().chain(pb.keys()).collect();
                for k in sub {
                    let value = |v: Option<&Value>| {
                        let v = v.cloned().unwrap_or(Value::Null);
                        if matches!(k.as_str(), "auth" | "headers") {
                            redact_secrets(&v, None)
                        } else {
                            v
                        }
                    };
                    let part = match k.as_str() {
                        "http_method" => "method".to_string(),
                        other => other.to_string(),
                    };
                    let (x, y) = (pa.get(k), pb.get(k));
                    if x != y {
                        let (vx, vy) = (value(x), value(y));
                        // A secret that changed still shows as a change, though both read `***`.
                        parts.push(PartChange {
                            part,
                            old: vx,
                            new: vy,
                        });
                    }
                }
            }
            other => push(
                &mut parts,
                other.to_string(),
                a.cloned().unwrap_or(Value::Null),
                b.cloned().unwrap_or(Value::Null),
            ),
        }
    }
    // A cached tool keeps its request as both `provider` and `tool_call_template`; show each change once.
    let mut unique: Vec<PartChange> = Vec::with_capacity(parts.len());
    for p in parts {
        if !unique.contains(&p) {
            unique.push(p);
        }
    }
    unique
}

/// Compare tool `tool` of staged manual `manual` with its version in the manual the staged one replaces.
pub fn diff_staged_tool(paths: &UtcpPaths, manual: &str, tool: &str) -> Result<ToolDiff> {
    let registry = UtcpRegistryConfig::load(paths)?;
    if !registry.staged.contains_key(manual) {
        return Err(anyhow!(
            "'{}' is not staged (staged: {})",
            manual,
            names_list(registry.staged.keys())
        ));
    }
    let (new_tools, old_tools) = staged_and_old_tools(paths, &registry, manual)?;
    let (status, parts) = match (old_tools.get(tool), new_tools.get(tool)) {
        (None, None) => {
            return Err(anyhow!(
                "'{manual}' has no tool '{tool}', staged or active; see the tools with: utcp list {manual} --staged"
            ))
        }
        (None, Some(_)) => (ToolStatus::Added, Vec::new()),
        (Some(_), None) => (ToolStatus::Removed, Vec::new()),
        (Some(a), Some(b)) if a == b => (ToolStatus::Unchanged, Vec::new()),
        (Some(a), Some(b)) => (ToolStatus::Changed, tool_parts(a, b)),
    };
    Ok(ToolDiff {
        manual: manual.to_string(),
        tool: tool.to_string(),
        replaces: Replaces::of(&registry, manual),
        status,
        parts,
    })
}

/// Compare the staged manuals `names` selects (every one when empty) with the manuals they would replace.
pub fn diff_staged(paths: &UtcpPaths, names: &[String]) -> Result<Vec<ManualDiff>> {
    let registry = UtcpRegistryConfig::load(paths)?;
    let mut out = Vec::new();
    for n in selected(&registry, names)? {
        let replaces = Replaces::of(&registry, &n);
        let old_entry = registry
            .manuals
            .get(&n)
            .or_else(|| registry.deregistered.get(&n));
        let (new_tools, old_tools) = staged_and_old_tools(paths, &registry, &n)?;
        let new_names: BTreeSet<&String> = new_tools.keys().collect();
        let old_names: BTreeSet<&String> = old_tools.keys().collect();
        let added = new_names
            .difference(&old_names)
            .map(|s| s.to_string())
            .collect();
        let removed = old_names
            .difference(&new_names)
            .map(|s| s.to_string())
            .collect();
        let both: Vec<&String> = new_names.intersection(&old_names).copied().collect();
        let changed: Vec<ToolChange> = both
            .iter()
            .filter_map(|t| {
                let (old, new) = (old_tools.get(*t)?, new_tools.get(*t)?);
                (old != new).then(|| ToolChange {
                    tool: t.to_string(),
                    what: tool_changes(old, new),
                })
            })
            .collect();
        out.push(ManualDiff {
            name: n.clone(),
            replaces,
            tool_count: new_tools.len(),
            added,
            removed,
            unchanged: both.len() - changed.len(),
            changed,
            hint_before: old_entry.and_then(|e| e.hint.clone()),
            hint_after: registry.staged.get(&n).and_then(|e| e.hint.clone()),
        });
    }
    Ok(out)
}

/// Make the staged manuals `names` selects (every one when empty) active. The registration each replaces (active or deregistered) is kept
/// as the previous version for [`rollback_manual`]. Rebuilds the index and refreshes the installed skills; tell a
/// running daemon to reload.
pub fn promote_staged(paths: &UtcpPaths, names: &[String]) -> Result<Vec<Promoted>> {
    let lock = paths.acquire_write_lock()?;
    let mut registry = UtcpRegistryConfig::load(paths)?;
    let names = selected(&registry, names)?;
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let mut promoted = Vec::new();
    let mut staged_files = Vec::new();
    for n in names {
        let entry = registry
            .staged
            .remove(&n)
            .expect("selected names are staged");
        let staged_file = entry_file(paths, &n, &entry);
        let bytes = fs::read(&staged_file)
            .with_context(|| format!("cannot read the staged copy {}", staged_file.display()))?;
        let manual: CachedUtcpManual = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "the staged copy {} is not a utcp manual",
                staged_file.display()
            )
        })?;

        let replaced = Replaces::of(&registry, &n);
        let old = registry
            .manuals
            .get(&n)
            .or_else(|| registry.deregistered.get(&n))
            .cloned();
        let mut rollback_available = false;
        if let Some(old) = old {
            if let Ok(old_bytes) = fs::read(entry_file(paths, &n, &old)) {
                let previous_path = paths.previous_manual_path(&n);
                write_atomic(&previous_path, &old_bytes)?;
                registry.previous.insert(
                    n.clone(),
                    RegisteredManualEntry {
                        cached_file: Some(previous_path.to_string_lossy().into_owned()),
                        ..old
                    },
                );
                rollback_available = true;
            }
        }

        let active_path = paths.cached_manual_path(&n);
        write_atomic(&active_path, &bytes)?;
        registry.activate(
            &n,
            RegisteredManualEntry {
                cached_file: Some(active_path.to_string_lossy().into_owned()),
                ..entry
            },
        );
        staged_files.push(staged_file);
        promoted.push(Promoted {
            name: n,
            tool_count: manual.tools.len(),
            replaced,
            rollback_available,
        });
    }
    registry.save(paths)?;
    for f in staged_files {
        let _ = fs::remove_file(f);
    }
    rebuild_search_index_locked(paths, &registry)?;
    drop(lock);
    crate::skills::refresh_installed_global_skills(paths);
    Ok(promoted)
}

/// Drop the staged manuals `names` selects (every one when empty) and their staged copies. Returns the names dropped.
pub fn discard_staged(paths: &UtcpPaths, names: &[String]) -> Result<Vec<String>> {
    let _lock = paths.acquire_write_lock()?;
    let mut registry = UtcpRegistryConfig::load(paths)?;
    let names = selected(&registry, names)?;
    let mut files = Vec::new();
    for n in &names {
        if let Some(entry) = registry.staged.remove(n) {
            files.push(entry_file(paths, n, &entry));
        }
    }
    if !names.is_empty() {
        registry.save(paths)?;
    }
    for f in files {
        let _ = fs::remove_file(f);
    }
    Ok(names)
}

/// Make the previous version of `name` (kept by `promote`) active again. The version it replaces becomes the
/// previous one, so a second rollback undoes the first. Rebuilds the index and refreshes the installed skills; tell
/// a running daemon to reload.
pub fn rollback_manual(paths: &UtcpPaths, name: &str) -> Result<RolledBack> {
    let lock = paths.acquire_write_lock()?;
    let mut registry = UtcpRegistryConfig::load(paths)?;
    let Some(previous) = registry.previous.remove(name) else {
        return Err(anyhow!(
            "No previous version of '{}' is kept (one is kept each time `utcp promote` replaces a manual; \
             with a previous version: {})",
            name,
            names_list(registry.previous.keys())
        ));
    };
    let previous_file = entry_file(paths, name, &previous);
    let previous_bytes = fs::read(&previous_file).with_context(|| {
        format!(
            "cannot read the previous version {}",
            previous_file.display()
        )
    })?;
    let manual: CachedUtcpManual = serde_json::from_slice(&previous_bytes).with_context(|| {
        format!(
            "the previous version {} is not a utcp manual",
            previous_file.display()
        )
    })?;

    let current = registry
        .manuals
        .remove(name)
        .or_else(|| registry.deregistered.remove(name));
    let previous_path = paths.previous_manual_path(name);
    let mut undo_available = false;
    if let Some(current) = current {
        if let Ok(current_bytes) = fs::read(entry_file(paths, name, &current)) {
            write_atomic(&previous_path, &current_bytes)?;
            registry.previous.insert(
                name.to_string(),
                RegisteredManualEntry {
                    cached_file: Some(previous_path.to_string_lossy().into_owned()),
                    ..current
                },
            );
            undo_available = true;
        }
    }
    let active_path = paths.cached_manual_path(name);
    write_atomic(&active_path, &previous_bytes)?;
    registry.activate(
        name,
        RegisteredManualEntry {
            cached_file: Some(active_path.to_string_lossy().into_owned()),
            ..previous
        },
    );
    registry.save(paths)?;
    if !undo_available {
        let _ = fs::remove_file(&previous_path);
    }
    rebuild_search_index_locked(paths, &registry)?;
    drop(lock);
    crate::skills::refresh_installed_global_skills(paths);
    Ok(RolledBack {
        name: name.to_string(),
        tool_count: manual.tools.len(),
        undo_available,
    })
}

#[cfg(test)]
mod tests {
    use super::{tool_changes, tool_parts};
    use serde_json::json;

    /// `utcp diff` says what changed in a tool, and never shows header or auth values.
    #[test]
    fn tool_changes_name_each_part_that_differs() {
        let old = json!({
            "name": "t", "description": "Old", "tags": ["a"],
            "inputs": {"type": "object", "properties": {"a": {"type": "string"}, "b": {"type": "string"},
                "c": {"type": "string"}}, "required": ["a"]},
            "outputs": {"type": "object"},
            "provider": {"http_method": "GET", "url": "https://x/a", "auth": {"password": "${OLD}"},
                "headers": {"X-Key": "${K1}"}}
        });
        let new = json!({
            "name": "t", "description": "New", "tags": ["a"],
            "inputs": {"type": "object", "properties": {"a": {"type": "string"}, "b": {"type": "number"},
                "d": {"type": "string"}}, "required": ["b"]},
            "outputs": {"type": "object"},
            "provider": {"http_method": "POST", "url": "https://x/b", "auth": {"password": "${NEW}"},
                "headers": {"X-Key": "${K2}"}}
        });
        let what = tool_changes(&old, &new);
        assert_eq!(
            what,
            vec![
                "description",
                "inputs added: d",
                "inputs removed: c",
                "now required: b",
                "no longer required: a",
                "inputs changed: b",
                "auth",
                "headers",
                "method: GET -> POST",
                "url: https://x/a -> https://x/b",
            ]
        );
        assert!(!what.join(" ").contains("${"), "no auth or header values");
        let mut same_props = new.clone();
        same_props["inputs"]["additionalProperties"] = json!(false);
        assert_eq!(tool_changes(&new, &same_props), vec!["inputs"]);
    }

    /// A cached tool keeps its request under both `provider` and `tool_call_template`; a URL change is
    /// reported once, not once per copy.
    #[test]
    fn a_request_kept_twice_is_reported_once() {
        let tool = |url: &str| {
            let req = json!({"http_method": "GET", "url": url});
            json!({"name": "t", "provider": req.clone(), "tool_call_template": req})
        };
        let (old, new) = (tool("https://x/a"), tool("https://x/b"));
        assert_eq!(
            tool_changes(&old, &new),
            vec!["url: https://x/a -> https://x/b"]
        );
        let parts = tool_parts(&old, &new);
        assert_eq!(parts.len(), 1, "{parts:?}");
        assert_eq!(parts[0].part, "url");
    }

    /// `utcp diff <manual>.<tool>` shows each differing part old against new; secrets stay hidden, but a
    /// `${VAR}` placeholder (a name, not a value) is shown.
    #[test]
    fn tool_parts_show_old_against_new() {
        let old = json!({
            "name": "t", "description": "Old text",
            "inputs": {"type": "object", "properties": {"a": {"type": "string"}, "c": {"type": "string"}},
                "required": ["a"]},
            "provider": {"http_method": "GET", "url": "https://x/a",
                "auth": {"auth_type": "basic", "password": "${OLD}"}, "headers": {"X-Key": "literal-1"}}
        });
        let new = json!({
            "name": "t", "description": "New text",
            "inputs": {"type": "object", "properties": {"a": {"type": "string", "enum": ["x"]},
                "d": {"type": "integer"}}, "required": ["d"], "additionalProperties": false},
            "provider": {"http_method": "POST", "url": "https://x/a",
                "auth": {"auth_type": "basic", "password": "${NEW}"}, "headers": {"X-Key": "literal-2"}}
        });
        let parts = tool_parts(&old, &new);
        let get = |p: &str| {
            parts
                .iter()
                .find(|c| c.part == p)
                .map(|c| (c.old.clone(), c.new.clone()))
                .unwrap_or_else(|| panic!("no part {p} in {parts:?}"))
        };
        let names: Vec<&str> = parts.iter().map(|c| c.part.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "description",
                "input a",
                "input c",
                "input d",
                "inputs",
                "auth",
                "headers",
                "method"
            ]
        );
        assert_eq!(get("description"), (json!("Old text"), json!("New text")));
        assert_eq!(
            get("input a"),
            (
                json!({"type": "string", "required": true}),
                json!({"type": "string", "enum": ["x"], "required": false})
            )
        );
        assert_eq!(get("input c").1, json!(null));
        assert_eq!(get("input d").0, json!(null));
        assert_eq!(
            get("inputs"),
            (
                json!({"type": "object"}),
                json!({"type": "object", "additionalProperties": false})
            )
        );
        assert_eq!(
            get("auth").1,
            json!({"auth_type": "basic", "password": "${NEW}"})
        );
        assert_eq!(
            get("headers"),
            (json!({"X-Key": "***"}), json!({"X-Key": "***"}))
        );
        assert_eq!(get("method"), (json!("GET"), json!("POST")));
    }
}
