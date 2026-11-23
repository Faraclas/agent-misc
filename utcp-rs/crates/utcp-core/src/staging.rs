//! The staging area: `utcp stage` adds a manual next to the active ones without making it active. A staged manual
//! is not in search, `list` or the agent skills; it is callable only with `--staged` (with the same secrets as the
//! active manuals), so it can be reviewed (`diff`) and tested before `promote` makes it active. `promote` keeps the
//! registration it replaced for `rollback`. Like git's index: one staging area, at most one staged copy per name.

use crate::catalog::{
    build_index, convert_bytes_to_cached_manual, fetch_source_bytes, normalize_manual_hint,
    read_saved_manual, rebuild_search_index_locked, CachedUtcpManual, ImportSummary,
    RegisteredManualEntry, UtcpRegistryConfig,
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
    /// Tools in both whose description, inputs, outputs, tags or request differ.
    pub changed: Vec<String>,
    pub unchanged: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint_before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint_after: Option<String>,
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

/// The staged names `name` selects: that one (an error when it is not staged), or all of them.
fn selected(registry: &UtcpRegistryConfig, name: Option<&str>) -> Result<Vec<String>> {
    match name {
        Some(n) if registry.staged.contains_key(n) => Ok(vec![n.to_string()]),
        Some(n) => Err(anyhow!(
            "'{}' is not staged (staged: {})",
            n,
            names_list(registry.staged.keys())
        )),
        None => Ok(registry.staged.keys().cloned().collect()),
    }
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

/// Compare one staged manual (or every one) with the manual it would replace.
pub fn diff_staged(paths: &UtcpPaths, name: Option<&str>) -> Result<Vec<ManualDiff>> {
    let registry = UtcpRegistryConfig::load(paths)?;
    let mut out = Vec::new();
    for n in selected(&registry, name)? {
        let staged = load_staged_manual(paths, &n)?;
        let replaces = Replaces::of(&registry, &n);
        let old_entry = registry
            .manuals
            .get(&n)
            .or_else(|| registry.deregistered.get(&n));
        let old: Option<CachedUtcpManual> = old_entry.and_then(|e| {
            let bytes = fs::read(entry_file(paths, &n, e)).ok()?;
            serde_json::from_slice(&bytes).ok()
        });
        let as_map = |m: &CachedUtcpManual| -> BTreeMap<String, serde_json::Value> {
            m.tools
                .iter()
                .map(|t| (t.name.clone(), serde_json::to_value(t).unwrap_or_default()))
                .collect()
        };
        let new_tools = as_map(&staged);
        let old_tools = old.as_ref().map(as_map).unwrap_or_default();
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
        let changed: Vec<String> = both
            .iter()
            .copied()
            .filter(|t| new_tools.get(*t) != old_tools.get(*t))
            .cloned()
            .collect();
        out.push(ManualDiff {
            name: n.clone(),
            replaces,
            tool_count: staged.tools.len(),
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

/// Make one staged manual (or every one) active. The registration each replaces (active or deregistered) is kept
/// as the previous version for [`rollback_manual`]. Rebuilds the index and refreshes the installed skills; tell a
/// running daemon to reload.
pub fn promote_staged(paths: &UtcpPaths, name: Option<&str>) -> Result<Vec<Promoted>> {
    let lock = paths.acquire_write_lock()?;
    let mut registry = UtcpRegistryConfig::load(paths)?;
    let names = selected(&registry, name)?;
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

/// Drop one staged manual (or every one) and its staged copy. Returns the names dropped.
pub fn discard_staged(paths: &UtcpPaths, name: Option<&str>) -> Result<Vec<String>> {
    let _lock = paths.acquire_write_lock()?;
    let mut registry = UtcpRegistryConfig::load(paths)?;
    let names = selected(&registry, name)?;
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
