//! Independent exhaustive document evaluator, deliberately without FXI's AST,
//! planner, candidate selection, verifier, line map, or scoring implementation.
//! Regex leaves share the public regex engine (not an independent regex engine).
use fxi::index::{build::build_index_with_progress, reader::IndexReader};
use fxi::query::{QueryExecutor, try_parse_query};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

#[derive(Clone, Debug, Serialize, Deserialize)]
enum Expr {
    Bare(String),
    Phrase(String),
    Regex(String),
    Boost(Box<Expr>),
    Near(Vec<String>, u32),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
}
impl Expr {
    fn source(&self) -> String {
        match self {
            Self::Bare(s) => s.clone(),
            Self::Phrase(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
            Self::Regex(s) => format!("re:/{}/", s.replace('/', "\\/")),
            Self::Boost(e) => format!("^2:{}", e.source()),
            Self::Near(terms, distance) => format!("near:{},{distance}", terms.join(",")),
            Self::And(a, b) => format!("({} {})", a.source(), b.source()),
            Self::Or(a, b) => format!("({} | {})", a.source(), b.source()),
            Self::Not(e) => format!("-({})", e.source()),
        }
    }
    fn supports_word(&self) -> bool {
        match self {
            Self::Near(..) | Self::Boost(_) => false,
            Self::And(a, b) | Self::Or(a, b) => a.supports_word() && b.supports_word(),
            Self::Not(e) => e.supports_word(),
            _ => true,
        }
    }
    fn reductions(&self) -> Vec<Self> {
        let mut out = Vec::new();
        match self {
            Self::And(a, b) | Self::Or(a, b) => {
                out.extend([*a.clone(), *b.clone()]);
                for replacement in a.reductions() {
                    out.push(if matches!(self, Self::And(..)) {
                        Self::And(Box::new(replacement), b.clone())
                    } else {
                        Self::Or(Box::new(replacement), b.clone())
                    });
                }
                for replacement in b.reductions() {
                    out.push(if matches!(self, Self::And(..)) {
                        Self::And(a.clone(), Box::new(replacement))
                    } else {
                        Self::Or(a.clone(), Box::new(replacement))
                    });
                }
            }
            Self::Not(e) | Self::Boost(e) => out.push(*e.clone()),
            Self::Near(terms, distance) if terms.len() > 2 => {
                out.push(Self::Near(terms[..2].to_vec(), *distance));
            }
            _ => {}
        }
        out
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum Filter {
    Extension(String),
    Rust,
    Directory(String),
    Filename(String),
    Larger(u64),
    Smaller(u64),
    Newer(u64),
    Older(u64),
    Lines(u32, u32),
}
impl Filter {
    fn source(&self) -> String {
        match self {
            Self::Extension(s) => format!("ext:{s}"),
            Self::Rust => "lang:rust".into(),
            Self::Directory(s) => format!("path:{s}/**"),
            Self::Filename(s) => format!("file:{s}"),
            Self::Larger(n) => format!("size:>{n}"),
            Self::Smaller(n) => format!("size:<{n}"),
            Self::Newer(n) => format!("mtime:>{n}"),
            Self::Older(n) => format!("mtime:<{n}"),
            Self::Lines(a, b) => format!("line:{a}-{b}"),
        }
    }
    fn accepts(&self, doc: &Document) -> bool {
        match self {
            Self::Extension(ext) => doc
                .path
                .extension()
                .unwrap()
                .to_str()
                .unwrap()
                .eq_ignore_ascii_case(ext),
            Self::Rust => doc.path.extension().unwrap() == "rs",
            Self::Directory(dir) => doc.path.starts_with(dir),
            Self::Filename(name) => doc
                .path
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .eq_ignore_ascii_case(name),
            Self::Larger(n) => doc.text.len() as u64 > *n,
            Self::Smaller(n) => (doc.text.len() as u64) < *n,
            Self::Newer(n) => doc.mtime > *n,
            Self::Older(n) => doc.mtime < *n,
            Self::Lines(..) => true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Case {
    expression: Expr,
    filters: Vec<Filter>,
    insensitive: bool,
    word: bool,
    scope: Option<PathBuf>,
    limit: usize,
    before: u32,
    after: u32,
}
impl Case {
    fn source(&self) -> String {
        format!(
            "{} {}",
            self.filters
                .iter()
                .map(Filter::source)
                .collect::<Vec<_>>()
                .join(" "),
            self.expression.source()
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Document {
    path: PathBuf,
    text: String,
    mtime: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Replay {
    seed: u64,
    case_number: usize,
    case: Case,
    documents: Vec<Document>,
}

// This reference compiles expressions directly from our generated model. It
// never interprets the production parser's output as its expected semantics.
enum Reference {
    Leaf(Regex),
    Near(Vec<Regex>, u32),
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
    Not(Box<Self>),
}
type Evidence = BTreeMap<u32, (usize, usize)>;
impl Reference {
    fn new(expr: &Expr, insensitive: bool, word: bool) -> Self {
        let leaf = |pattern: String, ignore_case: bool| {
            let pattern = if word {
                format!(r"\b(?:{pattern})\b")
            } else {
                pattern
            };
            let pattern = if ignore_case {
                format!("(?i:{pattern})")
            } else {
                pattern
            };
            Self::Leaf(Regex::new(&pattern).unwrap())
        };
        match expr {
            Expr::Bare(s) => leaf(regex::escape(s), true),
            Expr::Phrase(s) => leaf(regex::escape(s), insensitive),
            Expr::Regex(s) => leaf(s.clone(), insensitive),
            Expr::Boost(e) => Self::new(e, insensitive, word),
            Expr::Near(terms, distance) => Self::Near(
                terms
                    .iter()
                    .map(|s| Regex::new(&format!("(?i:{})", regex::escape(s))).unwrap())
                    .collect(),
                *distance,
            ),
            Expr::And(a, b) => Self::And(
                Box::new(Self::new(a, insensitive, word)),
                Box::new(Self::new(b, insensitive, word)),
            ),
            Expr::Or(a, b) => Self::Or(
                Box::new(Self::new(a, insensitive, word)),
                Box::new(Self::new(b, insensitive, word)),
            ),
            Expr::Not(e) => Self::Not(Box::new(Self::new(e, insensitive, word))),
        }
    }
    fn evaluate(&self, lines: &[&str]) -> (bool, Evidence) {
        match self {
            Self::Leaf(regex) => {
                let hits: Evidence = lines
                    .iter()
                    .enumerate()
                    .filter_map(|(i, line)| {
                        regex
                            .find(line)
                            .map(|m| (i as u32 + 1, (m.start(), m.end())))
                    })
                    .collect();
                (!hits.is_empty(), hits)
            }
            Self::Not(e) => (!e.evaluate(lines).0, Evidence::new()),
            Self::And(a, b) | Self::Or(a, b) => {
                let (a_yes, mut a_hits) = a.evaluate(lines);
                let (b_yes, b_hits) = b.evaluate(lines);
                let yes = if matches!(self, Self::And(..)) {
                    a_yes && b_yes
                } else {
                    a_yes || b_yes
                };
                if !yes {
                    return (false, Evidence::new());
                }
                for (line, span) in b_hits {
                    a_hits
                        .entry(line)
                        .and_modify(|old| *old = (*old).min(span))
                        .or_insert(span);
                }
                (true, a_hits)
            }
            Self::Near(terms, distance) => {
                // Brute-force every possible line window; no production sweep
                // algorithm, positions index or term order heuristic is reused.
                let mut hits = Evidence::new();
                for start in 0..lines.len() {
                    let end = (start + *distance as usize + 1).min(lines.len());
                    let window = &lines[start..end];
                    if terms
                        .iter()
                        .all(|term| window.iter().any(|line| term.is_match(line)))
                    {
                        for (i, line) in window.iter().enumerate() {
                            if let Some(m) = terms[0].find(line) {
                                hits.insert((start + i + 1) as u32, (m.start(), m.end()));
                            }
                        }
                    }
                }
                (!hits.is_empty(), hits)
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Row {
    path: PathBuf,
    line: u32,
    text: String,
    start: usize,
    end: usize,
    before: Vec<(u32, String)>,
    after: Vec<(u32, String)>,
}
fn expected(case: &Case, docs: &[Document]) -> Vec<Row> {
    let reference = Reference::new(&case.expression, case.insensitive, case.word);
    let mut rows = Vec::new();
    for doc in docs {
        if !case.filters.iter().all(|f| f.accepts(doc))
            || case
                .scope
                .as_ref()
                .is_some_and(|scope| !doc.path.starts_with(scope))
        {
            continue;
        }
        let lines: Vec<_> = doc.text.lines().collect();
        let (yes, mut hits) = reference.evaluate(&lines);
        if !yes {
            continue;
        }
        let range = case.filters.iter().find_map(|f| {
            if let Filter::Lines(a, b) = f {
                Some((*a, *b))
            } else {
                None
            }
        });
        if let Some((a, b)) = range {
            hits.retain(|line, _| *line >= a && *line <= b);
        }
        if hits.is_empty() && range.is_none() {
            rows.push(Row {
                path: doc.path.clone(),
                line: 1,
                text: String::new(),
                start: 0,
                end: 0,
                before: vec![],
                after: vec![],
            });
        }
        for (line, (start, end)) in hits {
            let index = line as usize - 1;
            let slice = |lo, hi| {
                (lo..hi)
                    .map(|i: usize| (i as u32 + 1, lines[i].to_owned()))
                    .collect()
            };
            rows.push(Row {
                path: doc.path.clone(),
                line,
                text: lines[index].into(),
                start,
                end,
                before: slice(index.saturating_sub(case.before as usize), index),
                after: slice(
                    index + 1,
                    (index + 1 + case.after as usize).min(lines.len()),
                ),
            });
        }
    }
    rows.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
    rows
}

fn mismatch(reader: &IndexReader, case: &Case, docs: &[Document]) -> Option<String> {
    let mut query = match try_parse_query(&case.source()) {
        Ok(q) => q,
        Err(e) => return Some(format!("parse: {e}")),
    };
    query.options.case_insensitive = case.insensitive;
    query.filters.search_scope = case.scope.clone();
    if case.word {
        let result = query.apply_word_boundaries();
        if !case.expression.supports_word() {
            return result
                .is_ok()
                .then(|| "word: unsupported expression was accepted".into());
        }
        if let Err(e) = result {
            return Some(format!("word: {e}"));
        }
    }
    let expected = expected(case, docs);
    let executor = QueryExecutor::new(reader);
    let mut files: Vec<_> = expected.iter().map(|r| r.path.clone()).collect();
    files.dedup();
    if case.limit > 0 {
        files.truncate(case.limit);
    }
    match executor.execute_files_only(&query, case.limit) {
        Ok(actual) if actual == files => {}
        other => return Some(format!("files: expected {files:?}, got {other:?}")),
    }
    let mut counts = BTreeMap::new();
    for row in expected.iter().take(if case.limit == 0 {
        usize::MAX
    } else {
        case.limit
    }) {
        *counts.entry(row.path.clone()).or_insert(0usize) += 1;
    }
    let counts: Vec<_> = counts.into_iter().collect();
    match executor.execute_match_counts(&query, case.limit) {
        Ok(actual) if actual == counts => {}
        other => return Some(format!("counts: expected {counts:?}, got {other:?}")),
    }
    let actual = match executor.execute_with_content(&query, case.before, case.after) {
        Ok(hits) => hits
            .into_iter()
            .map(|h| Row {
                path: h.path,
                line: h.line_number,
                text: h.line_content,
                start: h.match_start,
                end: h.match_end,
                before: h.context_before,
                after: h.context_after,
            })
            .collect::<Vec<_>>(),
        Err(e) => return Some(format!("content: {e}")),
    };
    if actual != expected {
        return Some(format!("content: expected {expected:?}, got {actual:?}"));
    }
    // Independently validate path-sorted ranked identities. Fixture filenames
    // contain none of the meaningful generated terms, avoiding filename boosts.
    query.options.sort = fxi::query::parser::SortOrder::Path;
    query.options.limit = case.limit;
    let identities: Vec<_> = expected
        .iter()
        .take(if case.limit == 0 {
            usize::MAX
        } else {
            case.limit
        })
        .map(|r| (r.path.clone(), r.line))
        .collect();
    match executor.execute(&query) {
        Ok(actual)
            if actual.iter().all(|r| r.score.is_finite())
                && actual
                    .iter()
                    .map(|r| (r.path.clone(), r.line_number))
                    .collect::<Vec<_>>()
                    == identities => {}
        other => return Some(format!("ranked: expected {identities:?}, got {other:?}")),
    }
    // Score is a heuristic, so don't duplicate its formula in the oracle.
    // Check independently expected membership, order invariants and exact top-k
    // prefix consistency for generated expressions including boosts/negation.
    let mut all_identities: Vec<_> = expected.iter().map(|r| (r.path.clone(), r.line)).collect();
    all_identities.sort();
    for sort in [
        fxi::query::parser::SortOrder::Score,
        fxi::query::parser::SortOrder::Recency,
    ] {
        query.options.sort = sort;
        query.options.limit = 0;
        let all = match executor.execute(&query) {
            Ok(all) => all,
            Err(e) => return Some(format!("ranking: {e}")),
        };
        let mut actual_ids: Vec<_> = all
            .iter()
            .map(|r| (r.path.clone(), r.line_number))
            .collect();
        actual_ids.sort();
        if actual_ids != all_identities || all.iter().any(|r| !r.score.is_finite()) {
            return Some(format!(
                "ranking: wrong membership or nonfinite score for {sort:?}"
            ));
        }
        let ordered = match sort {
            fxi::query::parser::SortOrder::Score => {
                all.windows(2).all(|w| w[0].score >= w[1].score)
            }
            _ => {
                let times: BTreeMap<_, _> = docs.iter().map(|d| (&d.path, d.mtime)).collect();
                all.windows(2)
                    .all(|w| times[&w[0].path] >= times[&w[1].path])
            }
        };
        if !ordered {
            return Some(format!("ranking: wrong order for {sort:?}"));
        }
        query.options.limit = case.limit.max(1);
        let prefix: Vec<_> = all
            .iter()
            .take(query.options.limit)
            .map(|r| (&r.path, r.line_number))
            .collect();
        match executor.execute(&query) {
            Ok(limited)
                if limited
                    .iter()
                    .map(|r| (&r.path, r.line_number))
                    .collect::<Vec<_>>()
                    == prefix => {}
            other => {
                return Some(format!(
                    "ranking: top-k differs from full {sort:?} prefix: {other:?}"
                ));
            }
        }
    }
    None
}

struct Fixture {
    directory: tempfile::TempDir,
    reader: Option<IndexReader>,
    uncached: Option<IndexReader>,
}
impl Fixture {
    fn new(docs: &[Document]) -> Self {
        fxi::utils::app_data::isolate_test_storage().unwrap();
        let directory = tempfile::tempdir().unwrap();
        for doc in docs {
            let path = directory.path().join(&doc.path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, &doc.text).unwrap();
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(
                    fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(doc.mtime)),
                )
                .unwrap();
        }
        build_index_with_progress(directory.path(), true, true).unwrap();
        let reader = Some(IndexReader::open(directory.path()).unwrap());
        assert_eq!(
            reader.as_ref().unwrap().valid_doc_ids().len() as usize,
            docs.len(),
            "Generated corpus must be completely indexed"
        );
        let uncached = Some(IndexReader::open_uncached(directory.path()).unwrap());
        Self {
            directory,
            reader,
            uncached,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.reader.take();
        self.uncached.take();
        let _ = fxi::utils::remove_index(self.directory.path());
    }
}

struct Random(u64);
impl Random {
    fn next(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
    fn leaf(&mut self) -> Expr {
        let words = ["alpha", "beta", "γ", "K", "σ", "foo-bar", "x", "missing"];
        match self.next(5) {
            0 => Expr::Bare(words[self.next(words.len())].into()),
            1 => {
                let phrases = [
                    "alpha beta",
                    "ALPHA",
                    "Kelvin",
                    "foo/bar",
                    "",
                    "a\tb",
                    "foo\rbar",
                    "foo\nbar",
                    "say \"yes\"",
                    "a\\b",
                ];
                Expr::Phrase(phrases[self.next(phrases.len())].into())
            }
            2 => {
                let patterns = [
                    "alpha|beta",
                    "^alpha$",
                    "[A-Z]+",
                    "a.*b",
                    "(?:alpha)?beta",
                    "^$",
                    "\\bα\\b",
                    "(?i:kelvin)",
                    "foo[/]bar",
                    "(?:x|γ){1,2}",
                    "a\\s+b",
                    "(?m:^beta$)",
                    "[^x]*",
                    "(?:alpha|missing).*",
                ];
                Expr::Regex(patterns[self.next(patterns.len())].into())
            }
            3 => Expr::Near(
                vec![words[self.next(5)].into(), words[self.next(5)].into()],
                self.next(4) as u32,
            ),
            _ => Expr::Boost(Box::new(if self.next(2) == 0 {
                Expr::Bare("alpha".into())
            } else {
                Expr::Phrase("alpha beta".into())
            })),
        }
    }
    fn expression(&mut self, depth: usize) -> Expr {
        if depth == 0 {
            return self.leaf();
        }
        match self.next(5) {
            0 => Expr::And(
                Box::new(self.expression(depth - 1)),
                Box::new(self.expression(depth - 1)),
            ),
            1 => Expr::Or(
                Box::new(self.expression(depth - 1)),
                Box::new(self.expression(depth - 1)),
            ),
            2 => Expr::Not(Box::new(self.expression(depth - 1))),
            _ => self.leaf(),
        }
    }
    fn documents(&mut self) -> Vec<Document> {
        let lines = [
            "alpha",
            "ALPHA beta",
            "beta",
            "γ α σ Σ ς",
            "Kelvin kelvin KELVIN",
            "prefixalphasuffix",
            "foo-bar foo/bar",
            "",
            "a\tb",
            "foo\rbar",
            "say \"yes\" a\\b",
            "x xy γγ",
            "alpha beta alpha",
            "unrelated",
            "foo",
            "bar",
        ];
        let count = std::env::var("FXI_DIFF_DOCUMENTS")
            .map(|s| s.parse::<usize>().unwrap())
            .unwrap_or(72);
        assert!(count > 0 && count <= 1000);
        (0..count)
            .map(|i| {
                let count = 1 + self.next(8);
                let mut text = (0..count)
                    .map(|_| lines[self.next(lines.len())])
                    .collect::<Vec<_>>()
                    .join(if i % 3 == 0 { "\r\n" } else { "\n" });
                if i % 2 == 0 || text.is_empty() {
                    text.push('\n');
                }
                Document {
                    path: PathBuf::from(format!(
                        "{}/d{i:03}.{}",
                        ["src", "src-other", "tests"][i % 3],
                        ["rs", "txt", "py"][i % 3]
                    )),
                    text,
                    mtime: 1_600_000_000 + i as u64 * 10,
                }
            })
            .collect()
    }
    fn case(&mut self, number: usize) -> Case {
        let filters = match number % 12 {
            0 => vec![Filter::Extension("RS".into())],
            1 => vec![Filter::Rust, Filter::Larger(20)],
            2 => vec![Filter::Directory("src".into())],
            3 => vec![Filter::Filename(format!("D{:03}.RS", self.next(24) * 3))],
            4 => vec![Filter::Larger(8), Filter::Smaller(100)],
            5 => vec![Filter::Newer(1_600_000_100), Filter::Older(1_600_000_500)],
            6 => vec![Filter::Lines(
                1 + self.next(3) as u32,
                4 + self.next(5) as u32,
            )],
            7 => vec![Filter::Extension("txt".into()), Filter::Lines(2, 6)],
            _ => vec![],
        };
        Case {
            expression: self.expression(3),
            filters,
            insensitive: self.next(2) == 0,
            word: self.next(3) == 0,
            scope: match self.next(5) {
                0 => Some("src".into()),
                1 => Some("src/d000.rs".into()),
                _ => None,
            },
            limit: [0, 1, 3, 1000][self.next(4)],
            before: self.next(3) as u32,
            after: self.next(3) as u32,
        }
    }
}

fn mismatch_both(fixture: &Fixture, case: &Case, docs: &[Document]) -> Option<String> {
    for (name, reader) in [("cached", &fixture.reader), ("uncached", &fixture.uncached)] {
        if let Some(error) = mismatch(reader.as_ref().unwrap(), case, docs) {
            return Some(format!("{name}/{error}"));
        }
    }
    None
}

fn minimize(fixture: &Fixture, replay: &mut Replay, kind: &str) {
    let fails = |case: &Case| {
        mismatch_both(fixture, case, &replay.documents).is_some_and(|e| e.starts_with(kind))
    };
    loop {
        let mut candidates: Vec<_> = replay
            .case
            .expression
            .reductions()
            .into_iter()
            .map(|expression| Case {
                expression,
                ..replay.case.clone()
            })
            .collect();
        for i in 0..replay.case.filters.len() {
            let mut c = replay.case.clone();
            c.filters.remove(i);
            candidates.push(c);
        }
        if replay.case.scope.is_some() {
            candidates.push(Case {
                scope: None,
                ..replay.case.clone()
            });
        }
        if replay.case.word {
            candidates.push(Case {
                word: false,
                ..replay.case.clone()
            });
        }
        if replay.case.insensitive {
            candidates.push(Case {
                insensitive: false,
                ..replay.case.clone()
            });
        }
        if let Some(smaller) = candidates.into_iter().find(&fails) {
            replay.case = smaller;
        } else {
            break;
        }
    }
    // Delta-debug the document set too. Fresh indexes prevent a stale candidate
    // superset from disguising a missing-result bug during corpus reduction.
    let mut granularity = 2usize;
    while replay.documents.len() > 1 {
        let chunk = replay.documents.len().div_ceil(granularity);
        let mut reduced = false;
        for start in (0..replay.documents.len()).step_by(chunk) {
            let docs: Vec<_> = replay
                .documents
                .iter()
                .enumerate()
                .filter(|(i, _)| *i < start || *i >= start + chunk)
                .map(|(_, d)| d.clone())
                .collect();
            if docs.is_empty() {
                continue;
            }
            let candidate = Fixture::new(&docs);
            if mismatch_both(&candidate, &replay.case, &docs).is_some_and(|e| e.starts_with(kind)) {
                replay.documents = docs;
                reduced = true;
                break;
            }
        }
        if reduced {
            granularity = 2;
        } else if granularity >= replay.documents.len() {
            break;
        } else {
            granularity = (granularity * 2).min(replay.documents.len());
        }
    }
    // Reduce source lines while retaining nonempty, eligible text files. This
    // runs only after a failure, never in the normal CI path.
    for index in 0..replay.documents.len() {
        loop {
            let lines: Vec<_> = replay.documents[index].text.split_inclusive('\n').collect();
            let mut reduced = None;
            for remove in 0..lines.len() {
                let text = lines
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != remove)
                    .map(|(_, line)| *line)
                    .collect::<String>();
                if text.is_empty() {
                    continue;
                }
                let mut docs = replay.documents.clone();
                docs[index].text = text;
                let candidate = Fixture::new(&docs);
                if mismatch_both(&candidate, &replay.case, &docs)
                    .is_some_and(|e| e.starts_with(kind))
                {
                    reduced = Some(docs);
                    break;
                }
            }
            if let Some(docs) = reduced {
                replay.documents = docs;
            } else {
                break;
            }
        }
    }
}

fn check(fixture: &Fixture, mut replay: Replay) {
    if let Some(error) = mismatch_both(fixture, &replay.case, &replay.documents) {
        let kind = error.split(':').next().unwrap().to_owned();
        minimize(fixture, &mut replay, &kind);
        let directory = std::env::var_os("FXI_DIFF_FAILURE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        fs::create_dir_all(&directory).unwrap();
        let artifact = directory.join(format!(
            "fxi-differential-{}-{}-{}.json",
            std::process::id(),
            replay.seed,
            replay.case_number
        ));
        fs::write(&artifact, serde_json::to_vec_pretty(&replay).unwrap()).unwrap();
        panic!(
            "seed={} case={} minimized query={:?}; replay with FXI_DIFF_REPLAY={} cargo test --test generated_differential\nOriginal mismatch: {error}",
            replay.seed,
            replay.case_number,
            replay.case.source(),
            artifact.display()
        );
    }
}

#[test]
fn generated_queries_match_exhaustive_reference() {
    if let Some(path) = std::env::var_os("FXI_DIFF_REPLAY") {
        let replay: Replay = serde_json::from_slice(&fs::read(Path::new(&path)).unwrap()).unwrap();
        let fixture = Fixture::new(&replay.documents);
        check(&fixture, replay);
        return;
    }
    let seeds = std::env::var("FXI_DIFF_SEED")
        .map(|s| vec![s.parse::<u64>().unwrap().max(1)])
        .unwrap_or_else(|_| vec![0x5123, 0xa17e, 0xcafe]);
    let cases = std::env::var("FXI_DIFF_CASES")
        .map(|s| s.parse::<usize>().unwrap())
        .unwrap_or(120);
    for seed in seeds {
        let mut random = Random(seed);
        let documents = random.documents();
        let fixture = Fixture::new(&documents);
        // Guaranteed broad scan exercises parallel verification and, in the
        // larger FXI_SOURCE_PACK=1 campaign, uncached packed verification.
        check(
            &fixture,
            Replay {
                seed,
                case_number: usize::MAX,
                documents: documents.clone(),
                case: Case {
                    expression: Expr::Regex(".*".into()),
                    filters: vec![],
                    insensitive: false,
                    word: false,
                    scope: None,
                    limit: 0,
                    before: 1,
                    after: 1,
                },
            },
        );
        for case_number in 0..cases {
            let case = random.case(case_number);
            check(
                &fixture,
                Replay {
                    seed,
                    case_number,
                    case,
                    documents: documents.clone(),
                },
            );
        }
    }
}

#[test]
fn replay_minimized_search_regressions() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/differential");
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "json") {
            let replay: Replay = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
            let fixture = Fixture::new(&replay.documents);
            check(&fixture, replay);
        }
    }
}
