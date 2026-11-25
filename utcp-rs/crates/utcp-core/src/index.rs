use crate::paths::{write_atomic, UtcpPaths};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;

/// Format tag at the start of `search_index.bin`. Bump it whenever the layout or the meaning of a field changes (`3`:
/// `search_blob` includes the manual's hint): a file with another tag is treated as missing and rebuilt from the
/// registry instead of failing to decode or giving stale results.
pub const SEARCH_INDEX_MAGIC: [u8; 8] = *b"UTCPIDX3";

/// Compact pre-computed binary catalog stored in `search_index.bin` via `bincode`
/// so `utcp list`, `utcp search`, and `utcp schema` execute synchronously in microseconds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchIndex {
    pub magic: [u8; 8],
    pub built_at_unix_ms: u64,
    pub manuals: Vec<IndexedManual>,
    pub tools: Vec<IndexedTool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexedManual {
    pub name: String,
    pub source_url: String,
    pub cached: bool,
    pub cached_file: Option<String>,
    pub sha256: String,
    pub fetched_at: String,
    pub protocols: Vec<String>,
    pub tool_count: usize,
    pub file_size_bytes: u64,
    pub required_env_vars: Vec<String>,
    /// Every env var name this manual can read (`catalog::manual_env_var_names`): the allow-list
    /// the CLI uses to send the daemon only the caller variables it may need.
    #[serde(default)]
    pub env_var_names: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexedTool {
    /// Qualified name (`<manual>.<tool>`)
    pub qualified_name: String,
    pub manual_name: String,
    pub tool_name: String,
    pub protocol: String,
    pub description: String,
    pub tags: Vec<String>,
    /// Pre-serialized JSON string of the tool's `inputs` schema
    pub input_schema_json: String,
    /// Pre-serialized JSON string of the tool's `outputs` schema
    pub output_schema_json: String,
    /// Lowercased searchable text (`qualified_name + description + tags`) for sub-millisecond scoring
    pub search_blob: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSummary {
    pub name: String,
    pub manual: String,
    pub protocol: String,
    pub description: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inputs: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outputs: Option<serde_json::Value>,
    /// From a staged manual (`utcp search --staged`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub staged: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchemaDetail {
    pub name: String,
    pub manual: String,
    pub protocol: String,
    pub description: String,
    pub tags: Vec<String>,
    pub inputs: serde_json::Value,
    pub outputs: serde_json::Value,
}

impl Default for SearchIndex {
    fn default() -> Self {
        Self {
            magic: SEARCH_INDEX_MAGIC,
            built_at_unix_ms: 0,
            manuals: Vec::new(),
            tools: Vec::new(),
        }
    }
}

impl SearchIndex {
    /// Read `search_index.bin` directly from disk / OS page cache via `bincode`.
    pub fn load_from_disk(paths: &UtcpPaths) -> Result<Option<Self>> {
        if !paths.search_index_file.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&paths.search_index_file).with_context(|| {
            format!(
                "Failed to read search index {}",
                paths.search_index_file.display()
            )
        })?;
        // bincode writes the `[u8; 8]` magic as the first 8 raw bytes; check it before decoding so an
        // index from an older layout is rebuilt rather than reported as corrupt.
        if bytes.get(..SEARCH_INDEX_MAGIC.len()) != Some(&SEARCH_INDEX_MAGIC[..]) {
            return Ok(None);
        }
        let index: Self =
            bincode::deserialize(&bytes).context("Failed to decode search_index.bin")?;
        if index.magic != SEARCH_INDEX_MAGIC {
            return Ok(None);
        }
        Ok(Some(index))
    }

    /// Union of every registered manual's `env_var_names`.
    pub fn env_var_names(&self) -> std::collections::BTreeSet<String> {
        self.manuals
            .iter()
            .flat_map(|m| m.env_var_names.iter().cloned())
            .collect()
    }

    /// Atomically serialize and save `search_index.bin`.
    pub fn save_to_disk(&self, paths: &UtcpPaths) -> Result<()> {
        let bytes = bincode::serialize(self).context("Failed to encode search_index.bin")?;
        write_atomic(&paths.search_index_file, &bytes)
    }

    /// List tools with optional manual filter.
    pub fn list_tools(&self, manual_filter: Option<&str>) -> Vec<ToolSummary> {
        self.tools
            .iter()
            .filter(|t| {
                manual_filter
                    .map(|m| t.manual_name.eq_ignore_ascii_case(m))
                    .unwrap_or(true)
            })
            .map(|t| ToolSummary {
                name: t.qualified_name.clone(),
                manual: t.manual_name.clone(),
                protocol: t.protocol.clone(),
                description: t.description.clone(),
                tags: t.tags.clone(),
                inputs: None,
                outputs: None,
                staged: false,
            })
            .collect()
    }

    /// [`Self::search`] over the tools agents would have after promoting every staged manual: the
    /// `staged` index's tools (marked `staged`) and this index's tools of manuals not staged.
    pub fn search_with_staged(
        &self,
        staged: &SearchIndex,
        query: &str,
        limit: usize,
        include_schemas: bool,
    ) -> Vec<ToolSummary> {
        let staged_manuals: std::collections::HashSet<&str> = staged
            .tools
            .iter()
            .map(|t| t.manual_name.as_str())
            .collect();
        let tools: Vec<(&IndexedTool, bool)> = self
            .tools
            .iter()
            .filter(|t| !staged_manuals.contains(t.manual_name.as_str()))
            .map(|t| (t, false))
            .chain(staged.tools.iter().map(|t| (t, true)))
            .collect();
        rank(&tools, query, limit, include_schemas)
    }

    /// Ranked keyword/tag search returning compact 1-line summaries by default,
    /// or full JSON schemas when `include_schemas == true`.
    pub fn search(&self, query: &str, limit: usize, include_schemas: bool) -> Vec<ToolSummary> {
        let tools: Vec<(&IndexedTool, bool)> = self.tools.iter().map(|t| (t, false)).collect();
        rank(&tools, query, limit, include_schemas)
    }

    /// Retrieve exact JSON input/output schema for a single tool (`<manual>.<tool>` or unambiguous `<tool>`).
    pub fn get_schema(&self, target: &str) -> Option<ToolSchemaDetail> {
        let found = self
            .tools
            .iter()
            .find(|t| t.qualified_name.eq_ignore_ascii_case(target))
            .or_else(|| {
                let mut matches = self
                    .tools
                    .iter()
                    .filter(|t| t.tool_name.eq_ignore_ascii_case(target));
                let first = matches.next()?;
                if matches.next().is_none() {
                    Some(first)
                } else {
                    None
                }
            })?;

        let inputs = serde_json::from_str(&found.input_schema_json)
            .unwrap_or_else(|_| serde_json::json!({"type": "object"}));
        let outputs = serde_json::from_str(&found.output_schema_json)
            .unwrap_or_else(|_| serde_json::json!({"type": "object"}));

        Some(ToolSchemaDetail {
            name: found.qualified_name.clone(),
            manual: found.manual_name.clone(),
            protocol: found.protocol.clone(),
            description: found.description.clone(),
            tags: found.tags.clone(),
            inputs,
            outputs,
        })
    }

    fn to_summary(t: &IndexedTool, staged: bool, include_schemas: bool) -> ToolSummary {
        let (inputs, outputs) = if include_schemas {
            (
                serde_json::from_str(&t.input_schema_json).ok(),
                serde_json::from_str(&t.output_schema_json).ok(),
            )
        } else {
            (None, None)
        };
        ToolSummary {
            name: t.qualified_name.clone(),
            manual: t.manual_name.clone(),
            protocol: t.protocol.clone(),
            description: t.description.clone(),
            tags: t.tags.clone(),
            inputs,
            outputs,
            staged,
        }
    }
}

/// Score `tools` (each with whether it is staged) against `query`; see [`SearchIndex::search`].
fn rank(
    tools: &[(&IndexedTool, bool)],
    query: &str,
    limit: usize,
    include_schemas: bool,
) -> Vec<ToolSummary> {
    let q_trim = query.trim().to_ascii_lowercase();
    if q_trim.is_empty() {
        return tools
            .iter()
            .take(limit)
            .map(|&(t, staged)| SearchIndex::to_summary(t, staged, include_schemas))
            .collect();
    }

    // If the query is a single qualified `<manual>.<tool_prefix>` token (no whitespace),
    // scope strictly to `<manual>` so short tool words like `id` never match unrelated manuals!
    if !q_trim.chars().any(char::is_whitespace) {
        if let Some((manual_part, tool_part)) = q_trim.split_once('.') {
            if !manual_part.is_empty() {
                let mut scoped: Vec<(i64, &IndexedTool, bool)> = tools
                    .iter()
                    .filter(|(t, _)| t.manual_name.eq_ignore_ascii_case(manual_part))
                    .filter_map(|&(tool, staged)| {
                        let tname = tool.tool_name.to_ascii_lowercase();
                        if tool_part.is_empty() {
                            return Some((100, tool, staged));
                        }
                        if tname == tool_part {
                            Some((1000, tool, staged))
                        } else if tname.starts_with(tool_part) {
                            Some((500, tool, staged))
                        } else if tname.contains(tool_part) {
                            Some((250, tool, staged))
                        } else {
                            None
                        }
                    })
                    .collect();

                scoped.sort_by(|a, b| {
                    b.0.cmp(&a.0)
                        .then_with(|| a.1.qualified_name.cmp(&b.1.qualified_name))
                });
                return scoped
                    .into_iter()
                    .take(limit)
                    .map(|(_, t, staged)| SearchIndex::to_summary(t, staged, include_schemas))
                    .collect();
            }
        }
    }

    let terms: Vec<&str> = q_trim
        .split(|c: char| c.is_whitespace() || c == '.' || c == '_' || c == '-')
        .filter(|s| !s.is_empty())
        .collect();

    let mut scored: Vec<(i64, &IndexedTool, bool)> = tools
        .iter()
        .filter_map(|&(tool, staged)| {
            let mut score = 0i64;
            let qname_lower = tool.qualified_name.to_ascii_lowercase();
            let tname_lower = tool.tool_name.to_ascii_lowercase();

            if qname_lower == q_trim || tname_lower == q_trim {
                score += 1000;
            } else if q_trim.len() >= 3 && qname_lower.contains(&q_trim) {
                score += 400;
            } else if q_trim.len() >= 3 && tool.search_blob.contains(&q_trim) {
                score += 200;
            }

            for term in &terms {
                if tname_lower == *term {
                    score += 150;
                } else if term.len() <= 2 {
                    // Short 1-2 char tokens (like "id", "pr", "d2") must match whole word tokens,
                    // never substrings inside unrelated words like "val[id]ate"!
                    if tool
                        .search_blob
                        .split(|c: char| !c.is_ascii_alphanumeric())
                        .any(|w| w == *term)
                    {
                        score += 60;
                    }
                } else if qname_lower.contains(term) {
                    score += 80;
                } else if tool
                    .tags
                    .iter()
                    .any(|tag| tag.to_ascii_lowercase().contains(term))
                {
                    score += 60;
                } else if tool.search_blob.contains(term) {
                    score += 25;
                }
            }

            if score > 0 {
                Some((score, tool, staged))
            } else {
                None
            }
        })
        .collect();

    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.qualified_name.cmp(&b.1.qualified_name))
    });

    scored
        .into_iter()
        .take(limit)
        .map(|(_, t, staged)| SearchIndex::to_summary(t, staged, include_schemas))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tool(manual: &str, name: &str, desc: &str) -> IndexedTool {
        let qname = format!("{}.{}", manual, name);
        let blob = format!(
            "{} {}",
            qname.to_ascii_lowercase(),
            desc.to_ascii_lowercase()
        );
        IndexedTool {
            qualified_name: qname,
            manual_name: manual.to_string(),
            tool_name: name.to_string(),
            protocol: "http".to_string(),
            description: desc.to_string(),
            tags: vec![],
            input_schema_json: r#"{"type":"object"}"#.to_string(),
            output_schema_json: r#"{"type":"object"}"#.to_string(),
            search_blob: blob,
        }
    }

    fn index_of(tools: Vec<IndexedTool>) -> SearchIndex {
        SearchIndex {
            tools,
            ..SearchIndex::default()
        }
    }

    /// `search --staged` answers "what will agents find after promote": a staged manual replaces its
    /// active version, other active manuals stay, and staged results are marked.
    #[test]
    fn search_with_staged_replaces_the_active_version_and_marks_staged_tools() {
        let active = index_of(vec![
            sample_tool("gh", "list_issues", "List issues (old text)"),
            sample_tool("other", "issues_report", "Report on issues"),
        ]);
        let staged = index_of(vec![
            sample_tool("gh", "list_issues", "List issues for a repository"),
            sample_tool("gh", "get_issue", "Get one issue"),
        ]);
        let hits = active.search_with_staged(&staged, "issue", 10, false);
        let names: Vec<(&str, bool, &str)> = hits
            .iter()
            .map(|t| (t.name.as_str(), t.staged, t.description.as_str()))
            .collect();
        assert!(names.contains(&("gh.list_issues", true, "List issues for a repository")));
        assert!(names.contains(&("gh.get_issue", true, "Get one issue")));
        assert!(names.contains(&("other.issues_report", false, "Report on issues")));
        assert_eq!(
            hits.len(),
            3,
            "the active gh.list_issues is replaced: {names:?}"
        );
        let json = serde_json::to_value(&hits).unwrap();
        assert!(json
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t.get("staged").is_none()));
        assert!(active.search("issues", 10, false).iter().all(|t| !t.staged));
    }

    #[test]
    fn qualified_search_and_short_tokens_do_not_false_match_substrings() {
        let idx = SearchIndex {
            magic: SEARCH_INDEX_MAGIC,
            built_at_unix_ms: 0,
            manuals: vec![],
            tools: vec![
                sample_tool("context7", "resolve-library-id", "Resolve library ID"),
                sample_tool("context7", "query-docs", "Query documentation"),
                sample_tool("d2", "validate", "Validate diagram source"),
                sample_tool("d2", "themes", "List available theme IDs"),
            ],
        };

        // Exact qualified query ONLY matches context7.resolve-library-id (never d2.validate!)
        let exact = idx.search("context7.resolve-library-id", 10, false);
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].name, "context7.resolve-library-id");

        // Manual prefix `context7.` returns all context7 tools in order
        let manual_scoped = idx.search("context7.", 10, false);
        assert_eq!(manual_scoped.len(), 2);

        // Short 2-char token `id` matches whole word `ID` in context7, NOT substring `val[id]ate` or `IDs`!
        let short_token = idx.search("id", 10, false);
        assert_eq!(short_token.len(), 1);
        assert_eq!(short_token[0].name, "context7.resolve-library-id");
    }

    fn paths_in(root: &std::path::Path) -> UtcpPaths {
        UtcpPaths {
            config_dir: root.join("config"),
            cache_dir: root.join("cache"),
            state_dir: root.join("state"),
            media_dir: root.join("media"),
            config_file: root.join("config/.utcp_config.json"),
            settings_file: root.join("config/settings.toml"),
            env_file: root.join("config/.env"),
            lock_file: root.join("config/.utcp.lock"),
            manuals_dir: root.join("cache/manuals"),
            search_index_file: root.join("search_index.bin"),
            logging_sentinel: root.join("state/logging.enabled"),
            activity_log_file: root.join("state/activity.jsonl"),
            activity_log_bak: root.join("state/activity.jsonl.bak"),
            socket_endpoint: root.join("utcp.sock").to_string_lossy().into_owned(),
            daemon_pid_file: root.join("state/daemon.pid"),
            separate_setup: true,
        }
    }

    // An index written in an older layout is reported as missing (so callers rebuild it), never
    // as a decode error; the current layout round-trips `env_var_names`.
    #[test]
    fn older_index_layout_is_rebuilt_and_env_var_names_round_trip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = paths_in(tmp.path());

        let mut old = b"UTCPIDX2".to_vec();
        old.extend_from_slice(&[0u8; 64]);
        fs::write(&paths.search_index_file, &old).unwrap();
        assert!(SearchIndex::load_from_disk(&paths).unwrap().is_none());

        let manual = |name: &str, vars: &[&str]| IndexedManual {
            name: name.into(),
            source_url: String::new(),
            cached: true,
            cached_file: None,
            sha256: String::new(),
            fetched_at: String::new(),
            protocols: Vec::new(),
            tool_count: 0,
            file_size_bytes: 0,
            required_env_vars: Vec::new(),
            env_var_names: vars.iter().map(|v| v.to_string()).collect(),
        };
        let idx = SearchIndex {
            magic: SEARCH_INDEX_MAGIC,
            built_at_unix_ms: 1,
            manuals: vec![manual("a", &["A_KEY", "SHARED"]), manual("b", &["SHARED"])],
            tools: vec![],
        };
        idx.save_to_disk(&paths).unwrap();
        let loaded = SearchIndex::load_from_disk(&paths).unwrap().unwrap();
        assert_eq!(
            loaded.env_var_names().into_iter().collect::<Vec<_>>(),
            ["A_KEY", "SHARED"]
        );
    }
}
