//! Tool names from GitHub's operationIds (scheme C, see docs/github-manuals-spec.md).
//!
//! `repos/create-or-update-file-contents` keeps its words: `create_or_update_file_contents`. A bare verb gets the
//! area's noun (`repos/get` -> `get_repo`), a listed area gets its noun after the verb when the name lacks it
//! (`pulls/list-files` -> `list_pull_request_files`), search operations become `search_<what>`, and two tools of
//! one manual that still share a name get their area after the verb (`get_dependabot_alert`). `Config::names`
//! overrides any of this. A name used twice in one manual is an error.

use std::collections::HashMap;

use crate::config::Config;
use crate::text::snake;

/// Words that qualify a verb without naming what it acts on (`list-for-repo`, `get-authenticated`).
const QUALIFIERS: [&str; 10] = [
    "for",
    "by",
    "in",
    "on",
    "authenticated",
    "with",
    "from",
    "all",
    "commonly",
    "used",
];
/// Owners a qualifier may name; a name made only of these still lacks its noun.
const OWNERS: [&str; 5] = ["repo", "user", "org", "enterprise", "authenticated"];

fn split_op(op_id: &str) -> (&str, &str) {
    op_id.split_once('/').unwrap_or(("", op_id))
}

/// The operationId after the `/`, in snake_case, with the area's noun when it has only a verb and qualifiers.
fn with_noun(op_id: &str, conf: &Config) -> String {
    let (tag, rest) = split_op(op_id);
    let words: Vec<String> = snake(rest).split('_').map(String::from).collect();
    let after_verb: Vec<&String> = words[1..]
        .iter()
        .filter(|w| !QUALIFIERS.contains(&w.as_str()))
        .collect();
    if !after_verb.is_empty() && !after_verb.iter().all(|w| OWNERS.contains(&w.as_str())) {
        return words.join("_");
    }
    let mut noun = conf.nouns.get(tag).cloned().unwrap_or_else(|| snake(tag));
    if words[0] == "list" && !noun.ends_with('s') {
        noun.push('s');
    }
    let mut out = vec![words[0].clone()];
    let mut tail = &words[1..];
    if tail.first().is_some_and(|w| w == "authenticated") {
        out.push("authenticated".into());
        tail = &tail[1..];
    }
    out.push(noun);
    out.extend(tail.iter().cloned());
    out.join("_")
}

/// The name before clashes are resolved.
pub fn base_name(op_id: &str, conf: &Config) -> String {
    if let Some(name) = conf.names.get(op_id) {
        return name.clone();
    }
    let (tag, rest) = split_op(op_id);
    if tag == "search" {
        return format!("search_{}", snake(rest));
    }
    let name = with_noun(op_id, conf);
    let Some(tn) = conf.tag_nouns.get(tag) else {
        return name;
    };
    let words: Vec<&str> = name.split('_').collect();
    if words.iter().any(|w| tn.markers.iter().any(|m| m == w)) {
        return name;
    }
    // `list_reaction_for_issue` reads wrong: a list names its noun in the plural when nothing else follows it.
    let mut noun = tn.noun.clone();
    let next = words.get(1);
    if words[0] == "list" && next.is_none_or(|w| QUALIFIERS.contains(w)) && !noun.ends_with('s') {
        noun.push('s');
    }
    let mut out = vec![words[0].to_string(), noun];
    out.extend(words[1..].iter().map(|w| w.to_string()));
    out.join("_")
}

/// Names for the operationIds of one manual, in the given order. The tools of the manual's home tag keep the plain
/// name in a clash; the others get their area after the verb.
pub fn assign(manual: &str, op_ids: &[&str], conf: &Config) -> Result<Vec<String>, String> {
    let mut names: Vec<String> = op_ids.iter().map(|op| base_name(op, conf)).collect();
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, n) in names.iter().enumerate() {
        groups.entry(n.clone()).or_default().push(i);
    }
    let home = conf.home_tags.get(manual).map(String::as_str);
    for members in groups.values().filter(|m| m.len() > 1) {
        for &i in members {
            let (tag, _) = split_op(op_ids[i]);
            if Some(tag) == home || conf.names.contains_key(op_ids[i]) {
                continue;
            }
            let word = conf
                .clash_words
                .get(tag)
                .cloned()
                .unwrap_or_else(|| snake(tag));
            let (verb, rest) = names[i].split_once('_').unwrap_or((names[i].as_str(), ""));
            names[i] = if rest.is_empty() {
                format!("{verb}_{word}")
            } else {
                format!("{verb}_{word}_{rest}")
            };
        }
    }
    let mut seen: HashMap<&str, &str> = HashMap::new();
    for (i, n) in names.iter().enumerate() {
        if let Some(first) = seen.insert(n.as_str(), op_ids[i]) {
            return Err(format!(
                "{manual}: tool name '{n}' is used by {first} and {}; add one of them to config names",
                op_ids[i]
            ));
        }
    }
    Ok(names)
}
