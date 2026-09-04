//! Parses the raw text of a `/** ... */` KDoc comment (as captured by
//! `lexer::Lexer`'s doc-comment index — everything between the opening
//! `/**` and the closing `*/`, not including either marker) into a
//! structured summary plus block tags, following the real KDoc standard
//! (kotlinlang.org/docs/kotlin-doc.html): a leading description, then
//! `@tag ...` lines. Only `@param`/`@return` are rendered anywhere today
//! (see `resolver.rs`); every other real KDoc tag (`@constructor`,
//! `@receiver`, `@property`, `@throws`/`@exception`, `@sample`, `@see`,
//! `@author`, `@since`, `@suppress`) still correctly ends the summary and
//! is excluded from it — it's just not specially collected — so a later
//! phase (classes, etc.) can start rendering one without re-touching this
//! parser. An entirely unrecognized `@word` behaves identically: it still
//! ends the summary, its own text is simply dropped.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedDoc {
    pub summary: String,
    /// `(parameter name, description)`, in source order. A duplicate name
    /// isn't deduplicated here — `param()` below resolves it (last wins).
    pub params: Vec<(String, String)>,
    pub returns: Option<String>,
}

impl ParsedDoc {
    /// `true` when there's nothing worth showing at all — an entirely
    /// blank comment (just stars/whitespace), or one containing only tags
    /// this parser doesn't render, collapses to this so a caller can treat
    /// it exactly like "no doc comment was written", not an empty block.
    pub fn is_empty(&self) -> bool {
        self.summary.is_empty() && self.params.is_empty() && self.returns.is_none()
    }

    /// The description for the `@param` tag named `name`, if any — the
    /// *last* matching tag wins on a duplicate name.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.iter().rev().find(|(n, _)| n == name).map(|(_, d)| d.as_str())
    }
}

/// Strips each line's leading whitespace, then a leading `*` and (if
/// present) one space right after it — the standard KDoc/Javadoc per-line
/// prefix — and trims trailing whitespace. A line with no `*` prefix at
/// all (e.g. a single-line `/** summary */`, whose whole body is one
/// "line" with no continuation) is trimmed the same way, just without a
/// star to strip.
fn clean_lines(raw: &str) -> Vec<&str> {
    raw.lines()
        .map(|line| {
            let trimmed = line.trim_start();
            let no_star = trimmed.strip_prefix('*').unwrap_or(trimmed);
            no_star.strip_prefix(' ').unwrap_or(no_star).trim_end()
        })
        .collect()
}

/// Flushes whatever tag block was being accumulated (if any) into
/// `params`/`returns`, then clears the accumulator — called both when a
/// new `@tag` line starts (to close out the previous one) and once more
/// at the very end of the comment.
fn flush_tag(
    current_tag: &mut Option<String>,
    current_param_name: &mut Option<String>,
    current_text: &mut Vec<String>,
    params: &mut Vec<(String, String)>,
    returns: &mut Vec<String>,
) {
    let Some(tag) = current_tag.take() else { return };
    let joined = current_text.join("\n").trim().to_string();
    current_text.clear();
    match tag.as_str() {
        "param" => {
            if let Some(name) = current_param_name.take() {
                if !joined.is_empty() {
                    params.push((name, joined));
                }
            }
        }
        "return" => {
            if !joined.is_empty() {
                returns.push(joined);
            }
        }
        // Recognized-but-not-rendered or genuinely unknown — either way,
        // its text was already excluded from `summary` by the time we get
        // here (see the loop below), so there's nothing further to do.
        _ => {}
    }
}

/// Parses `raw` into a `ParsedDoc`.
pub fn parse(raw: &str) -> ParsedDoc {
    let mut summary_lines: Vec<&str> = Vec::new();
    let mut params: Vec<(String, String)> = Vec::new();
    let mut returns: Vec<String> = Vec::new();

    let mut current_tag: Option<String> = None;
    let mut current_param_name: Option<String> = None;
    let mut current_text: Vec<String> = Vec::new();

    for line in clean_lines(raw) {
        if let Some(rest) = line.strip_prefix('@') {
            flush_tag(&mut current_tag, &mut current_param_name, &mut current_text, &mut params, &mut returns);

            let mut parts = rest.splitn(2, char::is_whitespace);
            let tag_name = parts.next().unwrap_or("").to_string();
            let tag_rest = parts.next().unwrap_or("").trim_start();

            if tag_name == "param" {
                let mut p = tag_rest.splitn(2, char::is_whitespace);
                current_param_name = Some(p.next().unwrap_or("").to_string());
                current_text.push(p.next().unwrap_or("").trim_start().to_string());
            } else {
                current_text.push(tag_rest.to_string());
            }
            current_tag = Some(tag_name);
        } else if current_tag.is_some() {
            current_text.push(line.to_string());
        } else {
            summary_lines.push(line);
        }
    }
    flush_tag(&mut current_tag, &mut current_param_name, &mut current_text, &mut params, &mut returns);

    ParsedDoc {
        summary: summary_lines.join("\n").trim().to_string(),
        params,
        // Real KDoc only expects one `@return`; if more than one somehow
        // appears, the last non-empty one wins (same "last wins" instinct
        // as duplicate `@param` names).
        returns: returns.into_iter().next_back(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_only() {
        let doc = parse(" just a summary ");
        assert_eq!(doc.summary, "just a summary");
        assert!(doc.params.is_empty());
        assert_eq!(doc.returns, None);
    }

    #[test]
    fn multiline_summary_with_star_prefixes() {
        let doc = parse("\n * Adds two numbers.\n * Really.\n ");
        assert_eq!(doc.summary, "Adds two numbers.\nReally.");
    }

    #[test]
    fn summary_with_one_param() {
        let doc = parse("\n * Adds two numbers.\n * @param a the first number\n ");
        assert_eq!(doc.summary, "Adds two numbers.");
        assert_eq!(doc.param("a"), Some("the first number"));
    }

    #[test]
    fn multiple_params_and_a_return() {
        let doc = parse(
            "\n * Adds two numbers.\n * @param a the first number\n * @param b the second number\n * @return the sum\n ",
        );
        assert_eq!(doc.summary, "Adds two numbers.");
        assert_eq!(doc.param("a"), Some("the first number"));
        assert_eq!(doc.param("b"), Some("the second number"));
        assert_eq!(doc.returns.as_deref(), Some("the sum"));
    }

    #[test]
    fn param_with_no_description_is_dropped() {
        let doc = parse("\n * @param x\n ");
        assert_eq!(doc.param("x"), None);
        assert!(doc.is_empty());
    }

    #[test]
    fn duplicate_param_name_last_one_wins() {
        let doc = parse("\n * @param x first\n * @param x second\n ");
        assert_eq!(doc.param("x"), Some("second"));
        assert_eq!(doc.params.len(), 2, "both are kept, param() just resolves the latest");
    }

    #[test]
    fn unrecognized_tag_ends_the_summary_but_its_text_is_dropped() {
        let doc = parse("\n * Does a thing.\n * @throws Foo bar\n * @weirdTag whatever\n ");
        assert_eq!(doc.summary, "Does a thing.");
        assert!(!doc.summary.contains("Foo"));
        assert!(!doc.summary.contains("whatever"));
    }

    #[test]
    fn continuation_lines_extend_the_current_tag_not_the_summary() {
        let doc = parse("\n * Does a thing.\n * @param x line one\n * line two\n ");
        assert_eq!(doc.param("x"), Some("line one\nline two"));
        assert_eq!(doc.summary, "Does a thing.");
    }

    #[test]
    fn entirely_blank_comment_is_empty() {
        let doc = parse("\n * \n * \n ");
        assert!(doc.is_empty());
    }

    #[test]
    fn single_line_comment_with_no_star_prefix() {
        let doc = parse(" summary ");
        assert_eq!(doc.summary, "summary");
    }
}
