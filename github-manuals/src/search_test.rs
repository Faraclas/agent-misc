//! `search-test`: whether `utcp search` finds the right tool for what agents type, from a list of cases.
//!
//! A case is a line `<query> -> <tool> | <tool>`, optionally ending with `[known miss: <reason>]`; `#` starts a
//! comment. Any listed tool of a registered manual counts as a right answer. Everything here is read-only: utcp is
//! only asked to `list` and `search`.

use std::collections::BTreeSet;

use serde_json::Value;

/// The cases built into the binary.
pub const CASES: &str = include_str!("../tests/search-checks.txt");

/// How many results each search asks for (more when `--top` is larger).
pub const LIMIT: usize = 10;

const KNOWN_MISS: &str = "[known miss:";

/// One line of the cases file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Case {
    /// 1-based line number in the cases file.
    pub line: usize,
    pub query: String,
    /// Qualified tool names (`<manual>.<tool>`); any of them is a right answer.
    pub tools: Vec<String>,
    /// Why the case is a known miss: reported, but it does not fail the run.
    pub known_miss: Option<String>,
}

/// The cases in `text`, or the first malformed line (with its number).
pub fn parse_cases(text: &str) -> Result<Vec<Case>, String> {
    let mut cases = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        let body = raw.split('#').next().unwrap_or_default().trim();
        if body.is_empty() {
            continue;
        }
        let err = |why: &str| format!("line {line}: {why}: {}", raw.trim());
        let (rest, known_miss) = match body.find('[') {
            None => (body, None),
            Some(at) => {
                let reason = body[at..]
                    .strip_prefix(KNOWN_MISS)
                    .and_then(|m| m.strip_suffix(']'))
                    .map(str::trim)
                    .filter(|r| !r.is_empty() && !r.contains(['[', ']']))
                    .ok_or_else(|| err("a '[' must start a trailing [known miss: <reason>]"))?;
                (body[..at].trim_end(), Some(reason.to_string()))
            }
        };
        let (query, tools) = rest
            .split_once("->")
            .ok_or_else(|| err("expected <query> -> <tool> | <tool>"))?;
        let query = query.trim();
        if query.is_empty() {
            return Err(err("the query is empty"));
        }
        if tools.contains("->") {
            return Err(err("more than one ->"));
        }
        let tools: Vec<String> = tools.split('|').map(|t| t.trim().to_string()).collect();
        if let Some(bad) = tools.iter().find(|t| !is_tool_name(t)) {
            return Err(err(&format!(
                "'{bad}' is not a tool name (<manual>.<tool>)"
            )));
        }
        cases.push(Case {
            line,
            query: query.to_string(),
            tools,
            known_miss,
        });
    }
    if cases.is_empty() {
        return Err("no cases".into());
    }
    Ok(cases)
}

fn is_tool_name(name: &str) -> bool {
    match name.split_once('.') {
        Some((manual, tool)) => {
            !manual.is_empty()
                && !tool.is_empty()
                && !tool.contains('.')
                && !name.contains(char::is_whitespace)
        }
        None => false,
    }
}

/// The manual of a qualified tool name.
pub fn manual_of(tool: &str) -> &str {
    tool.split_once('.').map_or(tool, |(manual, _)| manual)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A listed tool is in the top N.
    Pass,
    /// None is, and the case is not a known miss.
    Fail,
    /// None is, and the case is a known miss.
    Known,
    /// None of the listed tools' manuals is registered, so the case was not searched.
    Skip,
}

impl Outcome {
    pub fn word(self) -> &'static str {
        match self {
            Outcome::Pass => "PASS",
            Outcome::Fail => "FAIL",
            Outcome::Known => "KNOWN",
            Outcome::Skip => "SKIP",
        }
    }
}

/// One case's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row<'a> {
    pub case: &'a Case,
    /// The best 1-based rank of a listed tool, `None` when none came back.
    pub rank: Option<usize>,
    /// The listed tool with that rank, else the first listed tool of a registered manual (else the first listed).
    pub tool: &'a str,
    pub outcome: Outcome,
    /// What search ranked first, for a case that missed.
    pub first: Option<String>,
}

/// Judges one case on the tool names a search returned (best first). A case passes when a listed tool of a
/// `registered` manual is in the first `top` results.
pub fn evaluate<'a>(
    case: &'a Case,
    results: &[String],
    registered: &BTreeSet<String>,
    top: usize,
) -> Row<'a> {
    let wanted: Vec<&'a str> = case
        .tools
        .iter()
        .map(String::as_str)
        .filter(|t| registered.contains(manual_of(t)))
        .collect();
    let Some(&first_wanted) = wanted.first() else {
        return Row {
            case,
            rank: None,
            tool: &case.tools[0],
            outcome: Outcome::Skip,
            first: None,
        };
    };
    let best = wanted
        .iter()
        .filter_map(|t| results.iter().position(|r| r == t).map(|i| (i + 1, *t)))
        .min();
    let (rank, tool) = match best {
        Some((rank, tool)) => (Some(rank), tool),
        None => (None, first_wanted),
    };
    let outcome = match (rank, &case.known_miss) {
        (Some(r), _) if r <= top => Outcome::Pass,
        (_, Some(_)) => Outcome::Known,
        _ => Outcome::Fail,
    };
    let first = (outcome != Outcome::Pass)
        .then(|| results.first().cloned())
        .flatten();
    Row {
        case,
        rank,
        tool,
        outcome,
        first,
    }
}

/// Counts over the cases that were searched (not skipped).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Totals {
    pub cases: usize,
    pub skipped: usize,
    pub top1: usize,
    pub top3: usize,
    pub top5: usize,
    pub failures: usize,
    pub known: usize,
}

impl Totals {
    pub fn of(rows: &[Row]) -> Totals {
        let mut t = Totals {
            cases: rows.len(),
            ..Totals::default()
        };
        for row in rows {
            match row.outcome {
                Outcome::Skip => t.skipped += 1,
                Outcome::Fail => t.failures += 1,
                Outcome::Known => t.known += 1,
                Outcome::Pass => {}
            }
            let within = |n| row.rank.is_some_and(|r| r <= n);
            t.top1 += usize::from(within(1));
            t.top3 += usize::from(within(3));
            t.top5 += usize::from(within(5));
        }
        t
    }

    pub fn searched(&self) -> usize {
        self.cases - self.skipped
    }

    /// The summary printed after the rows.
    pub fn lines(&self, top: usize) -> Vec<String> {
        let searched = self.searched();
        vec![
            format!(
                "{} cases: {searched} searched, {} skipped (no manual of theirs registered).",
                self.cases, self.skipped
            ),
            format!(
                "Top 1: {}/{searched}, top 3: {}/{searched}, top 5: {}/{searched}.",
                self.top1, self.top3, self.top5
            ),
            format!(
                "Failures (not in the top {top}): {}; known misses: {}.",
                self.failures, self.known
            ),
        ]
    }
}

/// One line per case: query, rank (or `-`), the expected tool, the outcome, and a note for misses and skips.
pub fn row_lines(rows: &[Row]) -> Vec<String> {
    let qw = rows
        .iter()
        .map(|r| r.case.query.chars().count())
        .chain([5])
        .max()
        .unwrap_or(0);
    let tw = rows
        .iter()
        .map(|r| r.tool.chars().count())
        .chain([4])
        .max()
        .unwrap_or(0);
    let mut lines = vec![format!("{:qw$}  rank  {:tw$}  result", "query", "tool")];
    for row in rows {
        let rank = row.rank.map_or("-".to_string(), |r| r.to_string());
        let note = match (row.outcome, &row.case.known_miss, &row.first) {
            (Outcome::Pass, Some(_), _) => "  (marked known miss: remove the marker)".to_string(),
            (Outcome::Known, Some(why), first) => format!(
                "  (known miss: {why}; first: {})",
                first.as_deref().unwrap_or("nothing")
            ),
            (Outcome::Fail, _, first) => {
                format!("  (first: {})", first.as_deref().unwrap_or("nothing"))
            }
            (Outcome::Skip, ..) => "  (no manual of its tools is registered)".to_string(),
            _ => String::new(),
        };
        lines.push(format!(
            "{:qw$}  {rank:>4}  {:tw$}  {}{note}",
            row.case.query,
            row.tool,
            row.outcome.word()
        ));
    }
    lines
}

/// Runs utcp with these arguments and returns its standard output, or why it failed.
pub type Run<'r> = dyn FnMut(&[&str]) -> Result<Vec<u8>, String> + 'r;

fn json_array(out: Vec<u8>, what: &str) -> Result<Vec<Value>, String> {
    match serde_json::from_slice(&out) {
        Ok(Value::Array(items)) => Ok(items),
        _ => Err(format!("{what} did not print a JSON array")),
    }
}

/// The manuals `utcp search` sees: the active ones, plus the staged ones when `staged`.
pub fn registered(run: &mut Run, staged: bool) -> Result<BTreeSet<String>, String> {
    let name = |m: &Value| m["name"].as_str().map(String::from);
    // `utcp list --json` also lists staged manuals, marked `"staged": true`.
    let mut names: BTreeSet<String> = json_array(run(&["list", "--json"])?, "utcp list --json")?
        .iter()
        .filter(|m| m["staged"] != true)
        .filter_map(name)
        .collect();
    if staged {
        let rows = json_array(
            run(&["list", "--staged", "--json"])?,
            "utcp list --staged --json",
        )?;
        names.extend(rows.iter().filter_map(name));
    }
    Ok(names)
}

/// The tool names `utcp search` returns for `query`, best first.
pub fn search(
    run: &mut Run,
    query: &str,
    staged: bool,
    limit: usize,
) -> Result<Vec<String>, String> {
    let limit = limit.to_string();
    let mut args = vec!["search", "--json", "--limit", &limit];
    if staged {
        args.push("--staged");
    }
    args.extend(["--", query]);
    let what = format!("utcp search '{query}'");
    Ok(json_array(run(&args)?, &what)?
        .iter()
        .filter_map(|t| t["name"].as_str().map(String::from))
        .collect())
}

/// Searches every case whose tools have a registered manual, and judges it.
pub fn run_cases<'a>(
    cases: &'a [Case],
    run: &mut Run,
    staged: bool,
    top: usize,
) -> Result<Vec<Row<'a>>, String> {
    let registered = registered(run, staged)?;
    let limit = top.max(LIMIT);
    cases
        .iter()
        .map(|case| {
            let searched = case.tools.iter().any(|t| registered.contains(manual_of(t)));
            let results = if searched {
                search(run, &case.query, staged, limit)?
            } else {
                Vec::new()
            };
            Ok(evaluate(case, &results, &registered, top))
        })
        .collect()
}
