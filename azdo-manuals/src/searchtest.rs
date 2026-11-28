//! Search test: do agents find the right tool with the words they would type?
//!
//! Each case is a query and the tools that count as a right answer. The query goes through `utcp search` of the
//! user's real utcp (`--staged` searches the staged manuals as if promoted), and the case passes when one of its tools
//! ranks in the top N. Read-only: it runs only `utcp list` and `utcp search`.
//!
//! The cases (`tests/search-checks.txt`, built into the binary) name only tools, never an organization, so they can
//! live in the repository while the manuals they are checked against cannot.
//!
//! Case file format, one case per line:
//!
//! ```text
//! # a comment
//! query words -> manual.tool | manual.other_tool [known miss: why it is not found yet]
//! ```
//!
//! `#` at the start of a line, or after a space outside the brackets, starts a comment. Tools of a manual that is not
//! registered are ignored, and a case with none registered is skipped, so one file serves several families of
//! manuals. A known miss is reported but does not fail the test.

use std::collections::BTreeSet;

use crate::utcp::Utcp;

/// The cases built into the binary.
pub const DEFAULT_CASES: &str = include_str!("../tests/search-checks.txt");

/// How many results each search asks for; a tool ranked lower counts as not found.
pub const SEARCH_LIMIT: usize = 10;

/// The ranks the totals count.
const TOTALS_AT: [usize; 3] = [1, 3, 5];

const KNOWN_MISS: &str = "[known miss:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Case {
    /// 1-based line in the case file.
    pub line: usize,
    pub query: String,
    /// `manual.tool` names; any of them is a right answer.
    pub tools: Vec<String>,
    /// The reason, for a case that is reported but does not fail.
    pub known_miss: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    Known,
    Skip,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Fail => "FAIL",
            Status::Known => "KNOWN",
            Status::Skip => "SKIP",
        }
    }
}

/// One case's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    pub case: Case,
    /// 1-based rank of the best-ranked expected tool, `None` when none was in the results (or the case was skipped).
    pub rank: Option<usize>,
    /// The tool found at `rank`, else the expected tools that could have been found (all of them for a skip).
    pub tool: String,
    pub status: Status,
}

/// Parse a case file. A malformed line is an error naming it.
pub fn parse_cases(text: &str) -> Result<Vec<Case>, String> {
    let mut cases = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        let content = strip_comment(raw).trim();
        if content.is_empty() {
            continue;
        }
        let bad = |what: &str| format!("line {line}: {what}: {}", raw.trim());
        let (query, rest) = content
            .split_once("->")
            .ok_or_else(|| bad("expected `query -> manual.tool | manual.tool`"))?;
        let query = query.trim();
        if query.is_empty() {
            return Err(bad("no query before `->`"));
        }
        if query.starts_with('-') {
            return Err(bad(
                "a query cannot start with `-` (utcp would read it as an option)",
            ));
        }
        let (tools, known_miss) = match rest.find('[') {
            None => (rest, None),
            Some(at) => {
                let marker = rest[at..].trim_end();
                let reason = marker
                    .strip_prefix(KNOWN_MISS)
                    .and_then(|r| r.strip_suffix(']'))
                    .map(str::trim)
                    .filter(|r| !r.is_empty())
                    .ok_or_else(|| bad("expected `[known miss: reason]` at the end"))?;
                (&rest[..at], Some(reason.to_string()))
            }
        };
        let tools: Vec<String> = tools.split('|').map(|t| t.trim().to_string()).collect();
        if let Some(t) = tools.iter().find(|t| !is_tool_name(t)) {
            let what = if t.is_empty() {
                "a tool name is missing".to_string()
            } else {
                format!("`{t}` is not a tool name (manual.tool)")
            };
            return Err(bad(&what));
        }
        cases.push(Case {
            line,
            query: query.to_string(),
            tools,
            known_miss,
        });
    }
    Ok(cases)
}

/// The line without its comment: `#` at its start or after whitespace, outside `[...]`.
fn strip_comment(line: &str) -> &str {
    let mut in_brackets = false;
    let mut after_space = true;
    for (i, c) in line.char_indices() {
        match c {
            '[' => in_brackets = true,
            ']' => in_brackets = false,
            '#' if after_space && !in_brackets => return &line[..i],
            _ => {}
        }
        after_space = c.is_whitespace();
    }
    line
}

fn is_tool_name(name: &str) -> bool {
    let word = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    };
    name.split_once('.')
        .is_some_and(|(manual, tool)| word(manual) && word(tool))
}

fn manual_of(tool: &str) -> &str {
    tool.split_once('.').map_or(tool, |(manual, _)| manual)
}

/// The expected tools of `case` whose manual is registered: the ones a search can find.
pub fn findable<'a>(case: &'a Case, registered: &BTreeSet<String>) -> Vec<&'a str> {
    case.tools
        .iter()
        .map(String::as_str)
        .filter(|t| registered.contains(manual_of(t)))
        .collect()
}

/// Judge one case from the tool names a search returned, best first.
pub fn check(case: &Case, registered: &BTreeSet<String>, found: &[String], top: usize) -> Checked {
    let expected = findable(case, registered);
    if expected.is_empty() {
        return Checked {
            case: case.clone(),
            rank: None,
            tool: case.tools.join(" | "),
            status: Status::Skip,
        };
    }
    let best = found
        .iter()
        .take(SEARCH_LIMIT)
        .position(|name| expected.contains(&name.as_str()));
    let rank = best.map(|i| i + 1);
    let status = match rank {
        Some(r) if r <= top => Status::Pass,
        _ if case.known_miss.is_some() => Status::Known,
        _ => Status::Fail,
    };
    Checked {
        case: case.clone(),
        rank,
        tool: best.map_or_else(|| expected.join(" | "), |i| found[i].clone()),
        status,
    }
}

/// Check every case, calling `search` (query to tool names, best first) only for those with a registered manual.
pub fn check_all(
    cases: &[Case],
    registered: &BTreeSet<String>,
    top: usize,
    mut search: impl FnMut(&str) -> Result<Vec<String>, String>,
) -> Result<Vec<Checked>, String> {
    cases
        .iter()
        .map(|case| {
            let found = if findable(case, registered).is_empty() {
                Vec::new()
            } else {
                search(&case.query)?
            };
            Ok(check(case, registered, &found, top))
        })
        .collect()
}

/// Run the cases through the user's utcp: `utcp list` for the registered manuals, `utcp search` per case.
pub fn run(utcp: &Utcp, cases: &[Case], staged: bool, top: usize) -> Result<Vec<Checked>, String> {
    let registered = utcp.registered(staged)?;
    check_all(cases, &registered, top, |query| {
        utcp.search(query, staged, SEARCH_LIMIT)
    })
}

/// Whether any case that is not a known miss failed.
pub fn failed(checked: &[Checked]) -> bool {
    checked.iter().any(|c| c.status == Status::Fail)
}

/// One line per case (query, rank or `-`, tool, result), then the totals.
pub fn report(checked: &[Checked], top: usize) -> Vec<String> {
    let query_w = checked
        .iter()
        .map(|c| c.case.query.chars().count())
        .chain(["query".len()])
        .max()
        .unwrap_or(0);
    let tool_w = checked
        .iter()
        .map(|c| c.tool.chars().count())
        .chain(["tool".len()])
        .max()
        .unwrap_or(0);
    let mut lines = vec![format!(
        "{:query_w$}  {:>4}  {:tool_w$}  result",
        "query", "rank", "tool"
    )];
    for c in checked {
        let rank = c.rank.map_or("-".to_string(), |r| r.to_string());
        let note = match (c.status, &c.case.known_miss) {
            (Status::Known, Some(reason)) => format!(" ({reason})"),
            (Status::Skip, _) => " (no manual of these tools is registered)".to_string(),
            _ => String::new(),
        };
        lines.push(
            format!(
                "{:query_w$}  {rank:>4}  {:tool_w$}  {}{note}",
                c.case.query,
                c.tool,
                c.status.label()
            )
            .trim_end()
            .to_string(),
        );
    }
    let searched: Vec<&Checked> = checked
        .iter()
        .filter(|c| c.status != Status::Skip)
        .collect();
    let skipped = checked.len() - searched.len();
    let count = |n: usize| {
        searched
            .iter()
            .filter(|c| c.rank.is_some_and(|r| r <= n))
            .count()
    };
    let failures = checked.iter().filter(|c| c.status == Status::Fail).count();
    let known = checked.iter().filter(|c| c.status == Status::Known).count();
    lines.push(String::new());
    lines.push(format!(
        "{} cases: {} searched, {skipped} skipped.",
        checked.len(),
        searched.len()
    ));
    if !searched.is_empty() {
        let ranks: Vec<String> = TOTALS_AT
            .iter()
            .map(|&n| format!("top-{n} {} of {}", count(n), searched.len()))
            .collect();
        lines.push(format!("Found: {}.", ranks.join(", ")));
    }
    lines.push(format!(
        "{known} known {}; {failures} {} (not in the top {top}).",
        if known == 1 { "miss" } else { "misses" },
        if failures == 1 { "failure" } else { "failures" },
    ));
    let fixed: Vec<String> = checked
        .iter()
        .filter(|c| c.status == Status::Pass && c.case.known_miss.is_some())
        .map(|c| format!("line {} ({})", c.case.line, c.case.query))
        .collect();
    if !fixed.is_empty() {
        lines.push(format!(
            "Known misses found now, remove their marker: {}.",
            fixed.join(", ")
        ));
    }
    if searched.is_empty() {
        lines.push(
            "Nothing was searched: no case names a registered manual (staged manuals need --staged).".to_string(),
        );
    }
    lines
}
