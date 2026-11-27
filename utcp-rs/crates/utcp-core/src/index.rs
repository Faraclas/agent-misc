use crate::paths::{write_atomic, UtcpPaths};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;

/// Format tag at the start of `search_index.bin`. Bump it whenever the layout or the meaning of a field changes (`4`:
/// precomputed [`SearchWords`] replace the lowercased `search_blob`): a file with another tag is treated as missing
/// and rebuilt from the registry instead of failing to decode or giving stale results.
pub const SEARCH_INDEX_MAGIC: [u8; 8] = *b"UTCPIDX4";

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
    /// The words [`rank`] scores, computed once when the index is built.
    pub words: SearchWords,
}

/// A tool's searchable words, as [`words`] splits them. Each field is one string of words separated by
/// single spaces, which keeps `search_index.bin` small and quick to load.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchWords {
    /// Words of the tool name, in order (repeats kept: they weigh the name-coverage bonus).
    pub name: String,
    /// Words of the description's first sentence, in order.
    pub summary: String,
    /// Every word of the description; sorted, no repeats.
    pub description: String,
    /// Words of the manual name; sorted, no repeats.
    pub manual: String,
    /// Words of the tool's tags and the manual's hint; sorted, no repeats.
    pub meta: String,
}

impl SearchWords {
    pub fn new(manual: &str, tool: &str, description: &str, tags: &[String], hint: &str) -> Self {
        let meta = format!("{} {hint}", tags.join(" "));
        Self {
            name: words(tool).join(" "),
            summary: words(crate::text::first_sentence(description)).join(" "),
            description: sorted_set(words(description)),
            manual: sorted_set(words(manual)),
            meta: sorted_set(words(&meta)),
        }
    }
}

/// The words of a [`SearchWords`] field.
fn each(field: &str) -> impl Iterator<Item = &str> {
    field.split(' ').filter(|w| !w.is_empty())
}

fn has(field: &str, word: &str) -> bool {
    each(field).any(|w| w == word)
}

/// The lowercased words of `text`: runs of letters and digits of any alphabet, split where a lowercase letter
/// or digit is followed by an uppercase letter (`getRepo`) and before the last capital of a run followed by a
/// lowercase letter (`HTTPServer`). Only how text is written decides; there are no word lists, so every
/// manual and language is searched the same way.
fn words(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut word = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            continue;
        }
        if !word.is_empty() && c.is_uppercase() {
            let before = chars[i - 1];
            let lower_next = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            if before.is_lowercase() || before.is_numeric() || (before.is_uppercase() && lower_next)
            {
                out.push(std::mem::take(&mut word));
            }
        }
        word.extend(c.to_lowercase());
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

fn sorted_set(mut words: Vec<String>) -> String {
    words.sort_unstable();
    words.dedup();
    words.join(" ")
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
                        .then_with(|| a.1.qualified_name.len().cmp(&b.1.qualified_name.len()))
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

    // Split the query as typed, before lowercasing, so `getRepo` is two words like the tool names.
    let mut terms: Vec<String> = Vec::new();
    for w in words(query) {
        if !terms.contains(&w) {
            terms.push(w);
        }
    }
    if terms.is_empty() {
        return Vec::new();
    }

    let mut scored: Vec<(f64, &IndexedTool, bool)> = tools
        .iter()
        .filter_map(|&(tool, staged)| score(tool, &q_trim, &terms).map(|s| (s, tool, staged)))
        .collect();

    scored.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.1.qualified_name.len().cmp(&b.1.qualified_name.len()))
            .then_with(|| a.1.qualified_name.cmp(&b.1.qualified_name))
    });

    scored
        .into_iter()
        .take(limit)
        .map(|(_, t, staged)| SearchIndex::to_summary(t, staged, include_schemas))
        .collect()
}

/// How well `tool` answers a query (`None`: no query word matched). `query` is the trimmed, lowercased query,
/// `terms` its [`words`] (no repeats).
///
/// An exact tool or qualified name wins outright. Otherwise each term scores where it matches best: a word of
/// the manual name 8 (10 when the tool name has it too), of the tool name 10, of the first sentence 6, of the
/// tags or hint 3, of the rest of the description 1, or the start of a name or first-sentence word (terms of 4+
/// characters) 2, so `invoice` finds `invoices`. Words otherwise match whole: `id` never matches `validate`,
/// `port` never matches `report`. The total is scaled by the share of terms that matched, then precision is
/// added: up to 6 for how much of the tool name the query covers, up to 4 for the first sentence, and 8 more when
/// the query's words are exactly the first sentence's words (order and repeats ignored), so `get a repository`
/// finds the tool whose first sentence is "Get a repository." before tools that merely have `repository` in
/// their names. Callers break ties by the shorter qualified name.
fn score(tool: &IndexedTool, query: &str, terms: &[String]) -> Option<f64> {
    if tool.qualified_name.eq_ignore_ascii_case(query) || tool.tool_name.eq_ignore_ascii_case(query)
    {
        return Some(1e9);
    }
    let w = &tool.words;
    let mut total = 0.0;
    let mut matched = 0usize;
    for term in terms {
        let term = term.as_str();
        let points = if has(&w.manual, term) {
            if has(&w.name, term) {
                10.0
            } else {
                8.0
            }
        } else if has(&w.name, term) {
            10.0
        } else if has(&w.summary, term) {
            6.0
        } else if has(&w.meta, term) {
            3.0
        } else if has(&w.description, term) {
            1.0
        } else if term.chars().count() >= 4
            && each(&w.name)
                .chain(each(&w.summary))
                .any(|x| x.starts_with(term))
        {
            2.0
        } else {
            continue;
        };
        total += points;
        matched += 1;
    }
    if matched == 0 {
        return None;
    }
    total *= matched as f64 / terms.len() as f64;
    let coverage = |field: &str| {
        let (asked, all) = each(field).fold((0usize, 0usize), |(asked, all), x| {
            (asked + usize::from(terms.iter().any(|t| t == x)), all + 1)
        });
        if all == 0 {
            0.0
        } else {
            asked as f64 / all as f64
        }
    };
    total += 6.0 * coverage(&w.name) + 4.0 * coverage(&w.summary);
    let summary_is_query = !w.summary.is_empty()
        && each(&w.summary).all(|x| terms.iter().any(|t| t == x))
        && terms.iter().all(|t| has(&w.summary, t));
    if summary_is_query {
        total += 8.0;
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tool(manual: &str, name: &str, desc: &str) -> IndexedTool {
        IndexedTool {
            qualified_name: format!("{}.{}", manual, name),
            manual_name: manual.to_string(),
            tool_name: name.to_string(),
            protocol: "http".to_string(),
            description: desc.to_string(),
            tags: vec![],
            input_schema_json: r#"{"type":"object"}"#.to_string(),
            output_schema_json: r#"{"type":"object"}"#.to_string(),
            words: SearchWords::new(manual, name, desc, &[], ""),
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

    fn tagged_tool(manual: &str, name: &str, desc: &str, tags: &[&str], hint: &str) -> IndexedTool {
        let tags: Vec<String> = tags.iter().map(|t| t.to_string()).collect();
        IndexedTool {
            words: SearchWords::new(manual, name, desc, &tags, hint),
            tags,
            ..sample_tool(manual, name, desc)
        }
    }

    fn names(idx: &SearchIndex, query: &str) -> Vec<String> {
        idx.search(query, 50, false)
            .into_iter()
            .map(|t| t.name)
            .collect()
    }

    /// Words match whole: `port` never matches `report`, `get` never matches `target`. A term of 4+ letters
    /// may also match the start of a word, below a whole-word match: `repo` finds `report` after `get_repo`.
    #[test]
    fn search_matches_whole_words_not_substrings() {
        let idx = index_of(vec![
            sample_tool("store", "get_repo", "Get a repository."),
            sample_tool("store", "create_report", "Create a report about a target."),
        ]);
        assert_eq!(names(&idx, "report"), ["store.create_report"]);
        assert!(names(&idx, "port").is_empty());
        assert_eq!(
            names(&idx, "repo"),
            ["store.get_repo", "store.create_report"]
        );
        assert_eq!(names(&idx, "target"), ["store.create_report"]);
        assert_eq!(names(&idx, "get"), ["store.get_repo"]);
        assert!(names(&idx, "zzz nothing").is_empty());
    }

    /// Names split where they are written apart: `_`, `-`, camelCase (`getRepo`, `HTTPServer`, `v2Items`),
    /// and letters of any alphabet stay in their word.
    #[test]
    fn search_splits_words_by_how_they_are_written_in_any_alphabet() {
        let idx = index_of(vec![
            sample_tool("api", "getRepo", "Get a repo."),
            sample_tool("api", "HTTPServerStatus", "Server status."),
            sample_tool("api", "list_v2Items", "List items."),
            sample_tool("api", "get-user", "Get a user."),
            sample_tool("karten", "größe_ändern", "Ändert die Größe."),
        ]);
        assert_eq!(names(&idx, "repo")[0], "api.getRepo");
        assert_eq!(names(&idx, "get repo")[0], "api.getRepo");
        assert_eq!(names(&idx, "getRepo")[0], "api.getRepo");
        assert_eq!(names(&idx, "server")[0], "api.HTTPServerStatus");
        assert_eq!(names(&idx, "http status")[0], "api.HTTPServerStatus");
        assert_eq!(names(&idx, "v2")[0], "api.list_v2Items");
        assert_eq!(names(&idx, "user")[0], "api.get-user");
        assert_eq!(names(&idx, "größe")[0], "karten.größe_ändern");
        assert_eq!(names(&idx, "Ändern")[0], "karten.größe_ändern");
    }

    /// Search knows no words: no stop words, plural rules or abbreviations, in any language. A query word of
    /// 4+ characters still finds the words it starts, so `invoice` finds `invoices`.
    #[test]
    fn search_uses_no_word_lists() {
        let idx = index_of(vec![
            sample_tool("store", "list_invoices", "List invoices."),
            sample_tool("store", "get_policy", "Get a policy."),
            sample_tool("store", "get_org", "Get an org."),
            sample_tool("store", "list_all", "List every item."),
        ]);
        assert_eq!(names(&idx, "invoice"), ["store.list_invoices"]);
        assert!(names(&idx, "policies").is_empty());
        assert!(names(&idx, "organization").is_empty());
        assert_eq!(names(&idx, "all")[0], "store.list_all");
        assert_eq!(names(&idx, "an org")[0], "store.get_org");
    }

    /// A word in the tool name beats the same word in the first sentence, which beats tags and the
    /// manual's hint, which beat the rest of the description.
    #[test]
    fn search_weighs_name_then_first_sentence_then_tags_and_hint_then_description() {
        let idx = index_of(vec![
            tagged_tool("crm", "rest", "Other text. Mentions an invoice.", &[], ""),
            tagged_tool("crm", "tagged", "Other text.", &["invoice"], ""),
            tagged_tool("crm", "summary", "Send an invoice.", &[], ""),
            tagged_tool("crm", "send_invoice", "Send it.", &[], ""),
            tagged_tool("books", "hinted", "Other text.", &[], "invoice and bills"),
        ]);
        let hits = names(&idx, "invoice");
        assert_eq!(
            &hits[..3],
            ["crm.send_invoice", "crm.summary", "crm.tagged"]
        );
        assert!(hits[3..].contains(&"books.hinted".to_string()));
        assert_eq!(hits.last().unwrap(), "crm.rest");
    }

    /// The tool whose name the query covers most ranks first; equal scores go to the shorter name, not the
    /// alphabetically first.
    #[test]
    fn search_prefers_close_names_and_breaks_ties_by_length() {
        let idx = index_of(vec![
            sample_tool(
                "store",
                "get_repo_ruleset_history",
                "Get the history of a ruleset.",
            ),
            sample_tool("store", "get_repo_secret", "Get a repository secret."),
            sample_tool("store", "get_repo", "Get a repository."),
            sample_tool("store", "b_item", "Item."),
            sample_tool("store", "aa_item", "Item."),
        ]);
        assert_eq!(names(&idx, "get a repository")[0], "store.get_repo");
        assert_eq!(names(&idx, "get repo")[0], "store.get_repo");
        assert_eq!(names(&idx, "item"), ["store.b_item", "store.aa_item"]);
    }

    /// A query whose words are exactly the first sentence's words finds that tool before tools that only
    /// have one of the query words in their names.
    #[test]
    fn search_prefers_a_first_sentence_that_is_exactly_the_query() {
        let idx = index_of(vec![
            sample_tool(
                "store",
                "get_repository_advisory",
                "Get a repository security advisory.",
            ),
            sample_tool(
                "store",
                "get_repository_advisory_comment",
                "Get a comment on a repository security advisory.",
            ),
            sample_tool("store", "get_repo", "Get a repository."),
        ]);
        assert_eq!(names(&idx, "get a repository")[0], "store.get_repo");
        assert_eq!(names(&idx, "Repository, get a")[0], "store.get_repo");
        assert_eq!(
            names(&idx, "get a repository security advisory")[0],
            "store.get_repository_advisory"
        );
    }

    /// `<manual>.<prefix>` lists the tools starting with the prefix, shortest name first.
    #[test]
    fn scoped_search_breaks_ties_by_length() {
        let idx = index_of(vec![
            sample_tool("store", "get_branch", "Get a branch."),
            sample_tool("store", "get_alert_rule", "Get an alert rule."),
            sample_tool("store", "get_repo", "Get a repository."),
            sample_tool("store", "get_view", "Get a view."),
            sample_tool("store", "list_repos", "List repositories."),
        ]);
        assert_eq!(
            names(&idx, "store.get"),
            [
                "store.get_repo",
                "store.get_view",
                "store.get_branch",
                "store.get_alert_rule"
            ]
        );
    }

    /// A tool matching every query word beats one matching only some; a word of the manual name counts
    /// too, so naming the service picks its tool.
    #[test]
    fn search_rewards_matching_every_word_including_the_manual_name() {
        let idx = index_of(vec![
            sample_tool("shop", "get_order", "Get an order."),
            sample_tool("warehouse", "get_order", "Get an order."),
            sample_tool("warehouse", "cancel_order", "Cancel an order."),
        ]);
        assert_eq!(names(&idx, "warehouse order")[0], "warehouse.get_order");
        assert_eq!(names(&idx, "shop order")[0], "shop.get_order");
        assert_eq!(names(&idx, "cancel order")[0], "warehouse.cancel_order");
    }

    /// An exact tool or `<manual>.<tool>` name ranks first, ahead of tools that only share its words.
    #[test]
    fn search_ranks_exact_names_first() {
        let idx = index_of(vec![
            sample_tool("store", "list_all", "List every item."),
            sample_tool("store", "export", "Export items. Lists all of them."),
            sample_tool("store", "list", "Other text."),
        ]);
        assert_eq!(names(&idx, "every item")[0], "store.list_all");
        assert_eq!(names(&idx, "list")[0], "store.list");
        assert_eq!(names(&idx, "store.list")[0], "store.list");
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

        for old in [b"UTCPIDX2", b"UTCPIDX3"] {
            let mut bytes = old.to_vec();
            bytes.extend_from_slice(&[0u8; 64]);
            fs::write(&paths.search_index_file, &bytes).unwrap();
            assert!(SearchIndex::load_from_disk(&paths).unwrap().is_none());
        }

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
