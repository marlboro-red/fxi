//! Conservative byte-gram constraints derived from regex HIR.
//!
//! Every match must satisfy the emitted plan. Empty steps mean MatchAll.
//! Enumeration is deliberately bounded: budget exhaustion loses selectivity,
//! never matches. Verification remains responsible for ordering and distances.
use super::planner::{PlanStep, QueryPlan};
use crate::index::types::bytes_to_trigram;
use regex_syntax::hir::{Class, Hir, HirKind};

const MAX_ALTERNATIVES: usize = 16;
const MAX_LITERAL_BYTES: usize = 256;

/// A byte substring required by every match. This is only a necessary
/// condition: callers still verify the complete containing line. HIR literals
/// already reflect case folding; classes and optional branches are declined.
pub(super) fn required_literal(pattern: &str) -> Option<Vec<u8>> {
    fn required(hir: &Hir) -> Option<&[u8]> {
        match hir.kind() {
            HirKind::Literal(lit) => Some(&lit.0),
            HirKind::Capture(cap) => required(&cap.sub),
            HirKind::Repetition(rep) if rep.min > 0 => required(&rep.sub),
            HirKind::Concat(parts) => parts.iter().filter_map(required).max_by_key(|s| s.len()),
            HirKind::Alternation(parts) => {
                let first = required(parts.first()?)?;
                parts
                    .iter()
                    .all(|p| required(p) == Some(first))
                    .then_some(first)
            }
            _ => None,
        }
    }
    let hir = regex_syntax::Parser::new().parse(pattern).ok()?;
    let literal = required(&hir)?;
    (literal.len() >= 3 && !literal.contains(&b'\n') && !literal.contains(&b'\r'))
        .then(|| literal.to_vec())
}

/// Necessary ASCII-folded substring for line routing. Unicode classes with
/// extra folds (such as K/kelvin and S/long-s) cannot become ASCII anchors.
pub(super) fn required_ascii_literal(pattern: &str) -> Option<Vec<u8>> {
    fn atom(hir: &Hir) -> Option<Vec<u8>> {
        match hir.kind() {
            HirKind::Literal(lit) if lit.0.is_ascii() => Some(lit.0.to_ascii_lowercase()),
            HirKind::Class(Class::Unicode(class)) => {
                let mut chars = Vec::new();
                for range in class.iter() {
                    if !range.end().is_ascii() || range.end() as u32 - range.start() as u32 > 1 {
                        return None;
                    }
                    chars.extend(range.start() as u8..=range.end() as u8);
                }
                (chars.len() == 2
                    && chars[0].is_ascii_alphabetic()
                    && chars[0].eq_ignore_ascii_case(&chars[1]))
                .then(|| vec![chars[0].to_ascii_lowercase()])
            }
            HirKind::Capture(cap) => atom(&cap.sub),
            HirKind::Concat(parts) => {
                let mut bytes = Vec::new();
                for part in parts {
                    bytes.extend(atom(part)?);
                }
                Some(bytes)
            }
            _ => None,
        }
    }
    fn required(hir: &Hir) -> Option<Vec<u8>> {
        if let Some(bytes) = atom(hir) {
            return Some(bytes);
        }
        match hir.kind() {
            HirKind::Capture(cap) => required(&cap.sub),
            HirKind::Repetition(rep) if rep.min > 0 => required(&rep.sub),
            HirKind::Concat(parts) => {
                let mut best = Vec::new();
                let mut run = Vec::new();
                for part in parts {
                    if let Some(bytes) = atom(part) {
                        run.extend(bytes);
                    } else {
                        if run.len() > best.len() {
                            best = std::mem::take(&mut run);
                        }
                        run.clear();
                        if let Some(bytes) = required(part)
                            && bytes.len() > best.len()
                        {
                            best = bytes;
                        }
                    }
                }
                if run.len() > best.len() {
                    best = run;
                }
                Some(best)
            }
            HirKind::Alternation(parts) => {
                let first = required(parts.first()?)?;
                parts
                    .iter()
                    .all(|p| required(p).as_ref() == Some(&first))
                    .then_some(first)
            }
            _ => None,
        }
    }
    let hir = regex_syntax::Parser::new().parse(pattern).ok()?;
    let literal = required(&hir)?;
    (literal.len() >= 3 && !literal.contains(&b'\n') && !literal.contains(&b'\r'))
        .then_some(literal)
}

pub(super) fn regex_steps(pattern: &str) -> Vec<PlanStep> {
    regex_syntax::Parser::new()
        .parse(pattern)
        .map(|hir| constraints(&hir))
        .unwrap_or_default() // The executor reports invalid regexes before execution.
}

fn product(left: &[Vec<u8>], right: &[Vec<u8>]) -> Option<Vec<Vec<u8>>> {
    if left.len().checked_mul(right.len())? > MAX_ALTERNATIVES {
        return None;
    }
    let mut result = Vec::new();
    for a in left {
        for b in right {
            if a.len() + b.len() > MAX_LITERAL_BYTES {
                return None;
            }
            let mut bytes = a.clone();
            bytes.extend(b);
            result.push(bytes);
        }
    }
    Some(result)
}

/// Enumerate the entire finite language, or decline without approximating it.
fn finite(hir: &Hir) -> Option<Vec<Vec<u8>>> {
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => Some(vec![Vec::new()]),
        HirKind::Literal(literal) => {
            (literal.0.len() <= MAX_LITERAL_BYTES).then(|| vec![literal.0.to_vec()])
        }
        HirKind::Class(class) => {
            let mut result = Vec::new();
            match class {
                Class::Unicode(class) => {
                    for range in class.iter() {
                        if (range.end() as u32 - range.start() as u32) as usize + 1
                            > MAX_ALTERNATIVES - result.len()
                        {
                            return None;
                        }
                        for value in range.start() as u32..=range.end() as u32 {
                            if let Some(ch) = char::from_u32(value) {
                                result.push(ch.encode_utf8(&mut [0; 4]).as_bytes().to_vec());
                            }
                        }
                    }
                }
                Class::Bytes(class) => {
                    for range in class.iter() {
                        if usize::from(range.end() - range.start()) + 1
                            > MAX_ALTERNATIVES - result.len()
                        {
                            return None;
                        }
                        result.extend((range.start()..=range.end()).map(|b| vec![b]));
                    }
                }
            }
            Some(result)
        }
        HirKind::Capture(capture) => finite(&capture.sub),
        HirKind::Concat(parts) => {
            let mut result = vec![Vec::new()];
            for part in parts {
                result = product(&result, &finite(part)?)?;
            }
            Some(result)
        }
        HirKind::Alternation(parts) => {
            let mut result = Vec::new();
            for part in parts {
                result.extend(finite(part)?);
                if result.len() > MAX_ALTERNATIVES {
                    return None;
                }
            }
            Some(result)
        }
        HirKind::Repetition(rep) if rep.max == Some(rep.min) && rep.min <= 8 => {
            let sub = finite(&rep.sub)?;
            let mut result = vec![Vec::new()];
            for _ in 0..rep.min {
                result = product(&result, &sub)?;
            }
            Some(result)
        }
        _ => None,
    }
}

fn alternatives(strings: Vec<Vec<u8>>) -> Vec<PlanStep> {
    let mut plans = Vec::new();
    for string in strings {
        if string.len() < 3 {
            return Vec::new();
        }
        let grams = if let Ok(text) = std::str::from_utf8(&string) {
            crate::utils::query_trigrams(text)
        } else {
            let mut grams: Vec<_> = string
                .windows(3)
                .map(|b| bytes_to_trigram(b[0], b[1], b[2]))
                .collect();
            grams.sort_unstable();
            grams.dedup();
            grams
        };
        plans.push(QueryPlan {
            steps: vec![PlanStep::TrigramIntersect(grams)],
            verification: None,
        });
    }
    union(plans)
}

fn union(mut plans: Vec<QueryPlan>) -> Vec<PlanStep> {
    if plans.is_empty() || plans.iter().any(|p| p.steps.is_empty()) {
        Vec::new()
    } else if plans.len() == 1 {
        plans.pop().unwrap().steps
    } else {
        vec![PlanStep::Union(plans)]
    }
}

fn constraints(hir: &Hir) -> Vec<PlanStep> {
    if let Some(strings) = finite(hir) {
        return alternatives(strings);
    }
    match hir.kind() {
        HirKind::Literal(literal) => alternatives(vec![literal.0.to_vec()]),
        HirKind::Capture(capture) => constraints(&capture.sub),
        HirKind::Repetition(rep) if rep.min > 0 => constraints(&rep.sub),
        HirKind::Alternation(parts) => union(
            parts
                .iter()
                .map(|part| QueryPlan {
                    steps: constraints(part),
                    verification: None,
                })
                .collect(),
        ),
        HirKind::Concat(parts) => {
            let mut steps = Vec::new();
            let mut pending = vec![Vec::new()];
            for part in parts {
                if let Some(strings) = finite(part) {
                    if let Some(combined) = product(&pending, &strings) {
                        pending = combined;
                    } else {
                        steps.extend(alternatives(pending));
                        pending = strings;
                    }
                } else {
                    steps.extend(alternatives(pending));
                    pending = vec![Vec::new()];
                    steps.extend(constraints(part));
                }
            }
            steps.extend(alternatives(pending));
            // Bounded chunks can hide the only selective gram at a split
            // (e.g. insensitive "serverassert" splits before "rve").
            // Recover overlapping three-part windows without enumerating
            // the full exponential language. Budget failures remain MatchAll.
            for window in parts.windows(3) {
                let strings = window.iter().try_fold(vec![Vec::new()], |prefix, part| {
                    product(&prefix, &finite(part)?)
                });
                if let Some(strings) = strings {
                    steps.extend(alternatives(strings));
                }
            }
            steps
        }
        _ => Vec::new(),
    }
}

/// A sufficient (not necessary) condition for existence matching on a whole
/// buffer to preserve str::lines semantics, including CRLF and empty files.
pub(super) fn is_line_local(pattern: &str) -> bool {
    fn no_line_context(hir: &Hir) -> bool {
        match hir.kind() {
            HirKind::Empty => true,
            HirKind::Look(_) => false,
            HirKind::Literal(lit) => !lit.0.contains(&b'\n') && !lit.0.contains(&b'\r'),
            HirKind::Class(Class::Unicode(class)) => !class.iter().any(|r| {
                (r.start() <= '\n' && '\n' <= r.end()) || (r.start() <= '\r' && '\r' <= r.end())
            }),
            HirKind::Class(Class::Bytes(class)) => !class.iter().any(|r| {
                (r.start() <= b'\n' && b'\n' <= r.end()) || (r.start() <= b'\r' && b'\r' <= r.end())
            }),
            HirKind::Capture(cap) => no_line_context(&cap.sub),
            HirKind::Repetition(rep) => no_line_context(&rep.sub),
            HirKind::Concat(parts) | HirKind::Alternation(parts) => {
                parts.iter().all(no_line_context)
            }
        }
    }
    regex_syntax::Parser::new().parse(pattern).is_ok_and(|hir| {
        hir.properties().minimum_len().is_some_and(|n| n > 0) && no_line_context(&hir)
    })
}

#[cfg(test)]
mod line_tests {
    use super::*;

    #[test]
    fn whole_buffer_existence_has_the_same_answer_as_line_verification() {
        let patterns = [
            "needle",
            "(?i)needle",
            "foo|bar",
            "[a-z]+",
            "[^\\r\\n]+",
            "",
            "a*",
            "^foo$",
            "\\Afoo",
            "bar\\z",
            "\\bfoo\\b",
            "foo.bar",
            "(?s)foo.bar",
            "foo\\nbar",
            "foo\\r",
            "[\\s]",
            "[^x]",
            "foo(?:)",
        ];
        let contents = [
            "",
            "\n",
            "foo\r\nbar\n",
            "needle",
            "NEEDLE\n",
            "foo\nbar",
            "xfoo\rbar",
            "foo\r",
            "foo bar",
            "K foo",
            "unrelated",
        ];
        for pattern in patterns {
            let regex = regex::Regex::new(pattern).unwrap();
            if is_line_local(pattern) {
                for content in contents {
                    assert_eq!(
                        regex.is_match(content),
                        content.lines().any(|line| regex.is_match(line)),
                        "{pattern:?} in {content:?}"
                    );
                }
            }
        }
        assert!(is_line_local("needle(?:)"));
        assert!(is_line_local("(?i)serverassert"));
        assert!(!is_line_local("^foo$"));
        assert!(!is_line_local("a*"));
        assert!(!is_line_local("foo.bar")); // dot can consume a stripped CR
    }
}

/// Existence of a literal with nullable, assertion-free surroundings is just
/// existence of that literal. This covers `.*needle` without scanning each
/// line or paying the regex engine's greedy-prefix cost.
pub(super) fn existence_literal(pattern: &str) -> Option<Vec<u8>> {
    fn no_assertions(hir: &Hir) -> bool {
        match hir.kind() {
            HirKind::Look(_) => false,
            _ => hir.kind().subs().iter().all(no_assertions),
        }
    }
    fn required(hir: &Hir) -> Option<Vec<u8>> {
        match hir.kind() {
            HirKind::Literal(lit) => Some(lit.0.to_vec()),
            HirKind::Capture(cap) => required(&cap.sub),
            HirKind::Concat(parts) => {
                let mut required_parts = parts.iter().filter(|part| {
                    part.properties().minimum_len() != Some(0) || !no_assertions(part)
                });
                let literal = required(required_parts.next()?)?;
                required_parts.next().is_none().then_some(literal)
            }
            _ => None,
        }
    }
    let hir = regex_syntax::Parser::new().parse(pattern).ok()?;
    let literal = required(&hir)?;
    (!literal.is_empty() && !literal.contains(&b'\n') && !literal.contains(&b'\r'))
        .then_some(literal)
}

#[cfg(test)]
mod existence_tests {
    use super::*;
    #[test]
    fn nullable_literal_reduction_preserves_line_existence() {
        let patterns = [
            "needle",
            ".*needle",
            "(?s).*needle.*",
            "(?:prefix)?needle(?:suffix)*",
            "^needle",
            "needle$",
            "foo.*needle",
            "(?:needle|other)",
            "needle?",
            "",
            ".*",
            "a*",
            "needle\r",
            "(?:^)?needle",
            "(?i)needle",
            "(?:x+)?needle",
        ];
        let contents = [
            "",
            "needle",
            "prefixneedlesuffix",
            "before\nneedle\r\nafter",
            "xneedle",
            "unrelated",
            "nee dle",
        ];
        for pattern in patterns {
            let re = regex::Regex::new(pattern).unwrap();
            if let Some(literal) = existence_literal(pattern) {
                for content in contents {
                    assert_eq!(
                        memchr::memmem::find(content.as_bytes(), &literal).is_some(),
                        content.lines().any(|line| re.is_match(line)),
                        "{pattern:?}: {content:?}"
                    );
                }
            }
        }
        assert_eq!(existence_literal(".*needle"), Some(b"needle".to_vec()));
        assert!(existence_literal("^needle").is_none());
        assert!(existence_literal("foo.*needle").is_none());
    }
}
