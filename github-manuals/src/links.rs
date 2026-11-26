//! GitHub's descriptions point to other endpoints by their documentation pages ("To get the contents of a commit,
//! see "[Get a commit](https://docs.github.com/rest/commits/commits#get-a-commit).""), or by method and path
//! ("`GET /repos/{owner}/{repo}/code-scanning/alerts`"). An agent calls tools, not pages or paths, so a pointer to an
//! endpoint that is a tool names the tool instead: "see github_repos.get_commit.". A pointer to a left-out endpoint
//! or to a guide keeps only its text, as every other link does.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::{Captures, Regex};
use serde_json::Value;

use crate::generate::Operation;

/// A Markdown link, with the quotes and period GitHub writes around a page title (`"[Get a commit](url)."`).
static LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(")?\[([^\]]+)\]\(([^)\s]+)(?:\s+"[^"]*")?\)(\.)?(")?"#).unwrap()
});
/// A REST reference page: `https://docs.github.com/en/rest/<category>/<subcategory>#<summary slug>`, also relative
/// and without the language, and the older `/rest/reference/<category>/<summary slug>`.
static REST_PAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:https://docs\.github\.com)?(?:/[^/#?]+)*?/rest/([^#?]+?)/?(?:\?[^#]*)?(?:#([a-z0-9_-]+))?$")
        .unwrap()
});
/// `GET /repos/{owner}/{repo}`, with or without its code span.
static ROUTE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(`)?\b(GET|POST|PUT|PATCH|DELETE) (/[^\s`]*[^\s`.,;:)\]])(`)?").unwrap()
});
/// "... and `GET /orgs/{org}/rulesets/rule-suites` for organizations": the organization half of a pointer, when
/// organization administration is left out.
static ORG_ROUTE_CLAUSE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\s+(?:and|or)\s+`?(GET|POST|PUT|PATCH|DELETE) (/orgs/[^\s`]*[^\s`.,;:)\]])`?\s+for\s+(?:an\s+)?organizations?\b")
        .unwrap()
});
/// A path placeholder, `{owner}` or the Rails way `:owner`.
static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{[^}/]+\}|:[a-z_]+").unwrap());

/// A path with every placeholder written `{}`, so a route GitHub writes with other placeholder names
/// (`{group_name}` for `{concurrency_group_name}`, `:ref`) still finds its operation.
fn route_key(method: &str, path: &str) -> (String, String) {
    (
        method.to_string(),
        PLACEHOLDER.replace_all(path, "{}").into_owned(),
    )
}
/// Link text that names an endpoint rather than a thing ("the job logs endpoint", "the asynchronous merge API").
static ENDPOINT_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(endpoints?|operations?|API|route)\b").unwrap());
static NON_SLUG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-z0-9]+").unwrap());
/// A tool name with the words GitHub writes around an endpoint ("use the github_repos.list_commits endpoint", "see
/// the documentation for the github_users.get_app_by_slug endpoint").
static ENDPOINT_AROUND: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i:\bthe\s+documentation\s+for\s+)?(?i:\bthe\s+)?(github_[a-z_]+\.[a-z][a-z0-9_]*)\s+(?i:endpoint|operation|API)\b",
    )
    .unwrap()
});
/// Words after which a page title reads as a name ("use the Get a commit endpoint"), unlike after a verb's adverb
/// ("you can manually Create a check suite").
const NOUN_BEFORE: [&str; 11] = [
    "", "see", "use", "using", "the", "and", "or", "with", "via", "from", "call",
];

/// The heading anchors docs.github.com gives a summary: GitHub's (`get-a-repositorys-readme`), and one that turns
/// every run of other characters into `-` (`get-a-repository-s-readme`).
fn slugs(summary: &str) -> [String; 2] {
    let lower = summary.trim().to_lowercase();
    let github: String = lower
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
        .map(|c| if c == ' ' { '-' } else { c })
        .collect();
    let runs = NON_SLUG
        .replace_all(&lower, "-")
        .trim_matches('-')
        .to_string();
    [github, runs]
}

/// Link text or a summary compared without case, code spans, quotes and a final period.
fn normal(text: &str) -> String {
    text.replace(['`', '"'], "")
        .trim()
        .trim_end_matches('.')
        .trim()
        .to_lowercase()
}

struct Target {
    summary: String,
    /// `manual.tool`, or `None` when the operation is left out.
    tool: Option<String>,
}

/// Every operation of the description, findable by its documentation anchor and by its method and path.
pub struct Targets {
    targets: Vec<Target>,
    /// `category/subcategory#slug` to operations.
    anchors: HashMap<String, Vec<usize>>,
    /// `slug` alone to operations, for pages whose category moved.
    slugs: HashMap<String, Vec<usize>>,
    routes: HashMap<(String, String), usize>,
}

impl Targets {
    /// `tools`: the `manual.tool` name of each kept operationId.
    pub fn new(ops: &[Operation], tools: &HashMap<String, String>) -> Self {
        let mut out = Targets {
            targets: Vec::new(),
            anchors: HashMap::new(),
            slugs: HashMap::new(),
            routes: HashMap::new(),
        };
        for (i, o) in ops.iter().enumerate() {
            let summary = o.op.get("summary").and_then(Value::as_str).unwrap_or("");
            for slug in slugs(summary) {
                let anchor = format!("{}/{}#{slug}", o.category, o.subcategory);
                for (map, key) in [(&mut out.anchors, anchor), (&mut out.slugs, slug)] {
                    let list = map.entry(key).or_default();
                    if !list.contains(&i) {
                        list.push(i);
                    }
                }
            }
            out.routes.insert(route_key(&o.method, o.path), i);
            out.targets.push(Target {
                summary: summary.to_string(),
                tool: tools.get(o.op_id).cloned(),
            });
        }
        out
    }

    fn page(&self, url: &str) -> Option<&Target> {
        let c = REST_PAGE.captures(url)?;
        let path = &c[1];
        let unique = |list: Option<&Vec<usize>>| match list.map(Vec::as_slice) {
            Some([i]) => Some(&self.targets[*i]),
            _ => None,
        };
        match c.get(2) {
            Some(anchor) => unique(self.anchors.get(&format!("{path}#{}", anchor.as_str())))
                .or_else(|| unique(self.slugs.get(anchor.as_str()))),
            None => unique(self.slugs.get(path.rsplit('/').next()?)),
        }
    }

    fn route(&self, method: &str, path: &str) -> Option<&str> {
        self.route_target(method, path)?.tool.as_deref()
    }

    fn route_target(&self, method: &str, path: &str) -> Option<&Target> {
        let i = self.routes.get(&route_key(method, path))?;
        Some(&self.targets[*i])
    }

    fn link(&self, haystack: &str, c: &Captures) -> String {
        let (open, text, url) = (c.get(1).is_some(), &c[2], &c[3]);
        let (dot, close) = (c.get(4).is_some(), c.get(5).is_some());
        let before_word = haystack[..c.get(0).map_or(0, |m| m.start())]
            .split_whitespace()
            .last()
            .unwrap_or("");
        let before = before_word.to_lowercase();
        let quoted = |s: &str| {
            format!(
                "{}{s}{}{}",
                if open { "\"" } else { "" },
                if dot { "." } else { "" },
                if close { "\"" } else { "" }
            )
        };
        let Some(target) = self.page(url) else {
            return c[0].to_string();
        };
        let Some(tool) = &target.tool else {
            return quoted(text);
        };
        // A method and path in the text is named by `rewrite`'s route pass.
        let route = ROUTE
            .captures(text)
            .is_some_and(|r| self.route(&r[2], &r[3]).is_some());
        let title = normal(text) == normal(&target.summary);
        let titled = text
            .trim_start_matches(['`', '"'])
            .starts_with(char::is_uppercase);
        if route {
            quoted(text)
        } else if title && titled && (open && close || NOUN_BEFORE.contains(&before.as_str())) {
            // A page title ("see "Get a commit."", "use the Get a commit endpoint") is the tool itself.
            if open && close {
                format!("{tool}{}", if dot { "." } else { "" })
            } else {
                quoted(tool)
            }
        } else if !title && ENDPOINT_WORD.is_match(text) {
            // "use the job logs endpoint" is the tool: [`ENDPOINT_AROUND`] then drops "the" and "endpoint".
            quoted(&format!("{tool} endpoint"))
        } else if title {
            // A page title inside a sentence ("you can manually Create a check suite") reads as words.
            let mid_sentence = !before_word.is_empty()
                && !before_word.ends_with(['.', '!', '?', ':', '"'])
                && !open;
            let text = match text.split_once(' ') {
                Some((verb, rest))
                    if title && mid_sentence && verb.chars().skip(1).all(char::is_lowercase) =>
                {
                    format!("{} {rest}", verb.to_lowercase())
                }
                _ => text.to_string(),
            };
            quoted(&format!("{text} ({tool})"))
        } else if action_text(text, &before) {
            // "set the repository's subscription manually" is something a tool does.
            quoted(&format!("{text} ({tool})"))
        } else {
            quoted(text)
        }
    }

    /// `text` with its pointers to endpoints that are tools naming the tool ("use github_repos.list_commits", not
    /// "use the github_repos.list_commits endpoint").
    pub fn rewrite(&self, text: &str) -> String {
        let text = LINK.replace_all(text, |c: &Captures| self.link(text, c));
        let text = ORG_ROUTE_CLAUSE.replace_all(&text, |c: &Captures| {
            match self.route_target(&c[1], &c[2]) {
                Some(Target { tool: None, .. }) => String::new(),
                _ => c[0].to_string(),
            }
        });
        let text = ROUTE.replace_all(&text, |c: &Captures| match self.route(&c[2], &c[3]) {
            Some(tool) => tool.to_string(),
            None => c[0].to_string(),
        });
        ENDPOINT_AROUND.replace_all(&text, "$1").into_owned()
    }
}

/// Link text that says what to do ("set the repository's subscription manually", "fetch the result of the merge"):
/// several words, the first a lowercase verb. A thing ("the [issue]", "a [release]") or a single word in a list
/// ("When you get, create, or edit a pull request") is not.
fn action_text(text: &str, before: &str) -> bool {
    text.split_whitespace().count() > 1
        && text.starts_with(|c: char| c.is_ascii_lowercase())
        && !matches!(before, "a" | "an" | "the")
}

/// A copy of `spec` with every description rewritten by `targets`.
pub fn rewrite_spec(spec: &Value, targets: &Targets) -> Value {
    match spec {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    let v = match v {
                        Value::String(s) if k == "description" => Value::String(targets.rewrite(s)),
                        other => rewrite_spec(other, targets),
                    };
                    (k.clone(), v)
                })
                .collect(),
        ),
        Value::Array(list) => Value::Array(list.iter().map(|v| rewrite_spec(v, targets)).collect()),
        other => other.clone(),
    }
}
