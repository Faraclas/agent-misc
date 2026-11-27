//! Description text helpers shared by the CLI's one-line summaries and the search index.

/// The first non-empty line of a description, trimmed.
pub fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

/// The first sentence of a description's first non-empty line: up to the first `.` that ends a
/// sentence (see [`ends_sentence`]), or the whole line when none does.
pub fn first_sentence(text: &str) -> &str {
    let line = first_line(text);
    line.char_indices()
        .find(|&(i, c)| c == '.' && ends_sentence(line, i))
        .map_or(line, |(i, _)| &line[..=i])
}

/// Abbreviations whose `.` does not end a sentence.
const ABBREVIATIONS: [&str; 5] = ["e.g.", "i.e.", "cf.", "vs.", "etc."];

/// Whether the `.` at byte `dot` of `line` ends a sentence: a space follows it, the word it ends is
/// not an abbreviation such as `e.g.`, and the text after it does not go on in lower case.
fn ends_sentence(line: &str, dot: usize) -> bool {
    let rest = &line[dot + 1..];
    if !rest.starts_with(' ') {
        return false;
    }
    if rest.trim_start().starts_with(|c: char| c.is_lowercase()) {
        return false;
    }
    let word = line[..=dot]
        .rsplit(char::is_whitespace)
        .next()
        .unwrap_or_default()
        .trim_start_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase();
    !ABBREVIATIONS.contains(&word.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_line_is_the_first_non_empty_line() {
        assert_eq!(
            first_line("\n  Query docs.  \n\nUse it when:\n- a library\n"),
            "Query docs."
        );
        assert_eq!(first_line("one line"), "one line");
        assert_eq!(first_line(" \n "), "");
    }

    #[test]
    fn first_sentence_skips_abbreviations_and_lower_case_continuations() {
        assert_eq!(
            first_sentence("Get a run. Use it when you have the id."),
            "Get a run."
        );
        assert_eq!(first_sentence("Version 7.1.x of it"), "Version 7.1.x of it");
        assert_eq!(
            first_sentence("Get items, e.g. runs. More."),
            "Get items, e.g. runs."
        );
        assert_eq!(
            first_sentence("List repos (i.e. Git ones). More."),
            "List repos (i.e. Git ones)."
        );
        assert_eq!(
            first_sentence("Builds, tests, etc. for a run. More."),
            "Builds, tests, etc. for a run."
        );
        assert_eq!(first_sentence("Compare A vs. B. More."), "Compare A vs. B.");
        assert_eq!(
            first_sentence("Uses the v2 API. see the docs. Then more."),
            "Uses the v2 API. see the docs."
        );
        assert_eq!(first_sentence("Ends here. "), "Ends here.");
        assert_eq!(first_sentence("Line one\nLine two."), "Line one");
    }
}
