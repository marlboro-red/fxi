/// Parsed query representation
#[derive(Debug, Clone)]
pub struct Query {
    pub root: QueryNode,
    pub filters: QueryFilters,
    pub options: QueryOptions,
}

/// Query AST node
#[derive(Debug, Clone)]
pub enum QueryNode {
    /// Bounded compatibility-parser error, rejected by every executor.
    #[allow(dead_code)] // Compatibility library API; the CLI uses strict parsing.
    Invalid(QueryError),
    /// Simple literal search
    Literal(String),
    /// Simple literal search with boost
    BoostedLiteral { text: String, boost: f32 },
    /// Quoted phrase with a ranking boost; retains phrase case semantics.
    BoostedPhrase { text: String, boost: f32 },
    /// Exact phrase search (quoted)
    Phrase(String),
    /// Regex pattern
    Regex(String),
    /// Proximity search: terms must appear within distance lines of each other
    Near { terms: Vec<String>, distance: u32 },
    /// Boolean AND (all must match)
    And(Vec<QueryNode>),
    /// Boolean OR (any can match)
    Or(Vec<QueryNode>),
    /// Boolean NOT (exclude matches)
    Not(Box<QueryNode>),
    /// Empty query
    Empty,
}

/// Query filters
#[derive(Debug, Clone, Default)]
pub struct QueryFilters {
    /// Root-relative file or subtree selected by the client (component prefix).
    pub search_scope: Option<std::path::PathBuf>,
    /// Path glob pattern (path:src/*.rs)
    pub path: Option<String>,
    /// Filename pattern (file:foo or file:*.rs)
    pub filename: Option<String>,
    /// File extension filter (ext:rs)
    pub ext: Option<String>,
    /// Language filter (lang:rust)
    pub lang: Option<String>,
    /// Size filter (size:>1000, size:<10000)
    pub size_min: Option<u64>,
    pub size_max: Option<u64>,
    /// Line range filter (line:100-200)
    pub line_start: Option<u32>,
    pub line_end: Option<u32>,
    /// Modification time filter (mtime:>2024-01-01, mtime:<1704067200)
    pub mtime_min: Option<u64>,
    pub mtime_max: Option<u64>,
}

impl QueryFilters {
    /// Check if any filter is set
    pub fn has_any(&self) -> bool {
        self.search_scope.is_some() || self.has_query_filters()
    }

    fn has_query_filters(&self) -> bool {
        self.path.is_some()
            || self.filename.is_some()
            || self.ext.is_some()
            || self.lang.is_some()
            || self.size_min.is_some()
            || self.size_max.is_some()
            || self.line_start.is_some()
            || self.line_end.is_some()
            || self.mtime_min.is_some()
            || self.mtime_max.is_some()
    }
}

/// Query options
#[derive(Debug, Clone)]
pub struct QueryOptions {
    /// Sort order
    pub sort: SortOrder,
    /// Maximum results
    pub limit: usize,
    /// True when `top:` explicitly set the limit.
    pub explicit_limit: bool,
    /// Case-insensitive matching (-i): phrases and regexes ignore case.
    /// Bare token searches are case-insensitive regardless of this flag.
    pub case_insensitive: bool,
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            sort: SortOrder::Score,
            limit: 100,
            explicit_limit: false,
            case_insensitive: false,
        }
    }
}

/// Sort order for results
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    Score,
    Recency,
    Path,
}

/// Maximum accepted source bytes, AST nodes and recursive group depth.
/// Keeping these modest also bounds recursive planning, verification and drop.
pub const MAX_QUERY_BYTES: usize = 64 * 1024;
pub const MAX_QUERY_NODES: usize = 1024;
pub const MAX_QUERY_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryError {
    pub message: String,
    pub offset: usize,
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at byte {}", self.message, self.offset)
    }
}
impl std::error::Error for QueryError {}

type ParseResult<T> = Result<T, QueryError>;

/// Compatibility API: invalid input becomes an error node. Interactive and
/// command-line clients should use `try_parse_query` to display the error.
#[allow(dead_code)] // Retained public library API, unused by the strict CLI.
pub fn parse_query(input: &str) -> Query {
    try_parse_query(input).unwrap_or_else(|error| Query {
        root: QueryNode::Invalid(error),
        filters: QueryFilters::default(),
        options: QueryOptions::default(),
    })
}

/// Parse a complete, bounded query, rejecting malformed or ambiguous syntax.
pub fn try_parse_query(input: &str) -> ParseResult<Query> {
    if input.len() > MAX_QUERY_BYTES {
        return Err(QueryError {
            message: format!("Query exceeds {MAX_QUERY_BYTES} bytes"),
            offset: 0,
        });
    }
    let query = QueryParser::new(input).parse()?;
    query.validate()?;
    Ok(query)
}

struct QueryParser<'a> {
    input: &'a str,
    pos: usize,
    depth: usize,
    unary: usize,
    nodes: usize,
    directives: usize,
    seen_fields: std::collections::HashSet<String>,
    filters: QueryFilters,
    options: QueryOptions,
}

impl<'a> QueryParser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input,
            pos: 0,
            depth: 0,
            unary: 0,
            nodes: 0,
            directives: 0,
            seen_fields: std::collections::HashSet::new(),
            filters: QueryFilters::default(),
            options: QueryOptions::default(),
        }
    }

    fn error(&self, message: impl Into<String>) -> QueryError {
        QueryError {
            message: message.into(),
            offset: self.pos,
        }
    }

    fn parse(&mut self) -> ParseResult<Query> {
        self.skip_whitespace();
        let root = if self.is_eof() {
            QueryNode::Empty
        } else {
            self.parse_or()?
        };
        self.skip_whitespace();
        if !self.is_eof() {
            return Err(self.error("Unexpected closing delimiter"));
        }
        if self
            .filters
            .size_min
            .zip(self.filters.size_max)
            .is_some_and(|(a, b)| a > b)
            || self
                .filters
                .mtime_min
                .zip(self.filters.mtime_max)
                .is_some_and(|(a, b)| a > b)
        {
            return Err(self.error("Filter range is empty or reversed"));
        }
        Ok(Query {
            root,
            filters: self.filters.clone(),
            options: self.options.clone(),
        })
    }

    fn parse_or(&mut self) -> ParseResult<QueryNode> {
        let directives_before = self.directives;
        let mut nodes = vec![self.parse_and()?];
        self.skip_whitespace();
        while self.consume_char('|') {
            self.skip_whitespace();
            nodes.push(self.parse_and()?);
            self.skip_whitespace();
        }
        if nodes.len() > 1 && self.directives != directives_before {
            return Err(self.error("Filters/options are global; write `filter:value (a | b)`"));
        }
        Ok(if nodes.len() == 1 {
            nodes.pop().unwrap()
        } else {
            QueryNode::Or(nodes)
        })
    }

    fn parse_and(&mut self) -> ParseResult<QueryNode> {
        let mut nodes = Vec::new();
        let mut parsed_any = false;
        loop {
            self.skip_whitespace();
            if self.is_eof() || matches!(self.peek_char(), Some(')' | '|')) {
                break;
            }
            parsed_any = true;
            let node = self.parse_unary()?;
            if !matches!(node, QueryNode::Empty) {
                nodes.push(node);
            }
        }
        if !parsed_any {
            return Err(self.error("Expected a search term"));
        }
        Ok(match nodes.len() {
            0 => QueryNode::Empty,
            1 => nodes.pop().unwrap(),
            _ => QueryNode::And(nodes),
        })
    }

    fn parse_unary(&mut self) -> ParseResult<QueryNode> {
        self.nodes += 1;
        if self.nodes > MAX_QUERY_NODES {
            return Err(self.error("Query has too many terms"));
        }
        self.skip_whitespace();
        if self.consume_char('-') {
            self.unary += 1;
            let inner = self.parse_primary()?;
            self.unary -= 1;
            return Ok(QueryNode::Not(Box::new(inner)));
        }
        if self.consume_char('^') {
            let start = self.pos;
            while self
                .peek_char()
                .is_some_and(|c| c.is_ascii_digit() || c == '.')
            {
                self.advance();
            }
            let boost = if self.consume_char(':') {
                let value = &self.input[start..self.pos - 1];
                let parsed = value
                    .parse::<f32>()
                    .map_err(|_| self.error("Invalid boost"))?;
                if !parsed.is_finite() || parsed < 0.0 {
                    return Err(self.error("Boost must be finite and non-negative"));
                }
                parsed
            } else {
                self.pos = start;
                2.0
            };
            self.unary += 1;
            let inner = self.parse_primary()?;
            self.unary -= 1;
            return match inner {
                QueryNode::Literal(text) => Ok(QueryNode::BoostedLiteral { text, boost }),
                QueryNode::Phrase(text) => Ok(QueryNode::BoostedPhrase { text, boost }),
                _ => Err(self.error("Only literals and phrases can be boosted")),
            };
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> ParseResult<QueryNode> {
        self.skip_whitespace();
        if self.consume_char('(') {
            if self.depth >= MAX_QUERY_DEPTH {
                return Err(self.error("Query nesting is too deep"));
            }
            self.depth += 1;
            let node = self.parse_or()?;
            self.depth -= 1;
            if !self.consume_char(')') {
                return Err(self.error("Unclosed parenthesized expression"));
            }
            return Ok(node);
        }
        if self.peek_char() == Some('"') {
            return self.parse_phrase().map(QueryNode::Phrase);
        }
        if self.remaining().starts_with("re:/") {
            return self.parse_regex();
        }
        self.parse_term()
    }

    fn parse_phrase(&mut self) -> ParseResult<String> {
        self.consume_char('"');
        let mut text = String::new();
        while let Some(ch) = self.peek_char() {
            self.advance();
            if ch == '"' {
                return Ok(text);
            }
            if ch == '\\' && matches!(self.peek_char(), Some('"' | '\\')) {
                text.push(self.peek_char().unwrap());
                self.advance();
            } else {
                text.push(ch);
            }
        }
        Err(self.error("Unclosed quoted phrase"))
    }

    fn parse_regex(&mut self) -> ParseResult<QueryNode> {
        self.pos += 4;
        let mut pattern = String::new();
        // 0: first class token, 1: after initial ^, 2: class has content.
        let mut classes: Vec<u8> = Vec::new();
        while let Some(ch) = self.peek_char() {
            self.advance();
            if ch == '\\' {
                let next = self
                    .peek_char()
                    .ok_or_else(|| self.error("Unclosed regex escape"))?;
                self.advance();
                // Slash is a query delimiter, not a Rust regex metacharacter.
                if next != '/' {
                    pattern.push('\\');
                }
                pattern.push(next);
                if let Some(state) = classes.last_mut() {
                    *state = 2;
                }
            } else if ch == '/' && classes.is_empty() {
                // Extended-mode whitespace can make a leading `]` literal
                // even when the cheap delimiter scan saw class contents.
                // Let the regex parser resolve that ambiguous boundary.
                let unclosed_class = regex_syntax::ast::parse::Parser::new()
                    .parse(&pattern)
                    .is_err_and(|error| {
                        matches!(error.kind(), regex_syntax::ast::ErrorKind::ClassUnclosed)
                    });
                if unclosed_class {
                    classes.push(2);
                    pattern.push('/');
                } else {
                    return Ok(QueryNode::Regex(pattern));
                }
            } else {
                if ch == '[' {
                    if let Some(state) = classes.last_mut() {
                        *state = 2;
                    }
                    classes.push(0);
                } else if let Some(state) = classes.last_mut() {
                    if ch == ']' && *state == 2 {
                        classes.pop();
                    } else if ch == '^' && *state == 0 {
                        *state = 1;
                    } else {
                        *state = 2;
                    }
                }
                pattern.push(ch);
            }
        }
        Err(self.error("Unclosed regex; expected `/`"))
    }

    fn parse_term(&mut self) -> ParseResult<QueryNode> {
        let start = self.pos;
        // Field prefixes are recognized only at the start of a term.
        while self
            .peek_char()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            self.advance();
        }
        if self.consume_char(':') {
            let field = self.input[start..self.pos - 1].to_string();
            if matches!(
                field.to_ascii_lowercase().as_str(),
                "path"
                    | "file"
                    | "name"
                    | "ext"
                    | "lang"
                    | "size"
                    | "line"
                    | "mtime"
                    | "near"
                    | "sort"
                    | "top"
            ) {
                return self.parse_field(&field);
            }
        }
        self.pos = start;
        let mut internal_parens = 0usize;
        while let Some(ch) = self.peek_char() {
            if ch.is_whitespace() || ch == '|' || (ch == ')' && internal_parens == 0) {
                break;
            }
            if ch == '(' {
                internal_parens += 1;
            }
            if ch == ')' {
                internal_parens -= 1;
            }
            self.advance();
        }
        if self.pos == start {
            return Err(self.error("Expected a search term"));
        }
        Ok(QueryNode::Literal(self.input[start..self.pos].to_string()))
    }

    fn parse_field(&mut self, field: &str) -> ParseResult<QueryNode> {
        let field = field.to_ascii_lowercase();
        let value = if self.peek_char() == Some('"') {
            self.parse_phrase()?
        } else {
            let start = self.pos;
            while self
                .peek_char()
                .is_some_and(|c| !c.is_whitespace() && c != '|' && c != ')')
            {
                self.advance();
            }
            self.input[start..self.pos].to_string()
        };
        if value.is_empty() {
            return Err(self.error(format!("Missing value for {field}")));
        }
        if field == "near" {
            return self.parse_near_query(&value);
        }
        if self.depth != 0 || self.unary != 0 {
            return Err(self.error("Filters/options cannot be grouped, negated or boosted"));
        }
        if !matches!(field.as_str(), "sort" | "top") {
            self.directives += 1;
        }
        let canonical = if field == "name" { "file" } else { &field };
        let key = if matches!(canonical, "size" | "mtime") {
            format!(
                "{canonical}{}",
                value
                    .chars()
                    .next()
                    .filter(|c| *c == '>' || *c == '<')
                    .unwrap_or('=')
            )
        } else {
            canonical.to_string()
        };
        if !self.seen_fields.insert(key) {
            return Err(self.error(format!("Duplicate {field} filter/option")));
        }
        match field.as_str() {
            "path" | "file" | "name" => {
                if value.contains(['*', '?', '[', ']', '{', '}']) {
                    globset::Glob::new(&value)
                        .map_err(|e| self.error(format!("Invalid glob: {e}")))?;
                }
                if field == "path" {
                    self.filters.path = Some(value);
                } else {
                    self.filters.filename = Some(value);
                }
            }
            "ext" => self.filters.ext = Some(value),
            "lang" => {
                if !matches!(
                    value.to_ascii_lowercase().as_str(),
                    "rust"
                        | "rs"
                        | "python"
                        | "py"
                        | "javascript"
                        | "js"
                        | "typescript"
                        | "ts"
                        | "go"
                        | "java"
                        | "c"
                        | "cpp"
                        | "c++"
                        | "ruby"
                        | "rb"
                        | "shell"
                        | "sh"
                        | "bash"
                        | "unknown"
                        | "other"
                ) {
                    return Err(self.error("Unknown language filter"));
                }
                self.filters.lang = Some(value);
            }
            "size" => {
                let (min, n) = if let Some(n) = value.strip_prefix('>') {
                    (true, n)
                } else if let Some(n) = value.strip_prefix('<') {
                    (false, n)
                } else {
                    return Err(self.error("Size requires >N or <N"));
                };
                let n: u64 = n.parse().map_err(|_| self.error("Invalid size"))?;
                let bound = if min {
                    n.checked_add(1)
                } else {
                    n.checked_sub(1)
                }
                .ok_or_else(|| self.error("Size bound cannot match any file"))?;
                if min {
                    self.filters.size_min = Some(bound);
                } else {
                    self.filters.size_max = Some(bound);
                }
            }
            "line" => {
                let (a, b) = value.split_once('-').unwrap_or((&value, &value));
                let a: u32 = a.parse().map_err(|_| self.error("Invalid starting line"))?;
                let b: u32 = b.parse().map_err(|_| self.error("Invalid ending line"))?;
                if a == 0 || b < a {
                    return Err(self.error("Line range must be positive and ordered"));
                }
                self.filters.line_start = Some(a);
                self.filters.line_end = Some(b);
            }
            "mtime" => {
                if let Some(n) = value.strip_prefix('>') {
                    if self.seen_fields.contains("mtime=") {
                        return Err(self.error("Conflicting mtime filters"));
                    }
                    self.filters.mtime_min = Some(
                        Self::parse_timestamp(n)
                            .and_then(|n| n.checked_add(1))
                            .ok_or_else(|| self.error("Invalid mtime lower bound"))?,
                    );
                } else if let Some(n) = value.strip_prefix('<') {
                    if self.seen_fields.contains("mtime=") {
                        return Err(self.error("Conflicting mtime filters"));
                    }
                    self.filters.mtime_max = Some(
                        Self::parse_timestamp(n)
                            .and_then(|n| n.checked_sub(1))
                            .ok_or_else(|| self.error("Invalid mtime upper bound"))?,
                    );
                } else {
                    if self.seen_fields.contains("mtime>") || self.seen_fields.contains("mtime<") {
                        return Err(self.error("Conflicting mtime filters"));
                    }
                    let n = Self::parse_timestamp(&value)
                        .ok_or_else(|| self.error("Invalid date or timestamp"))?;
                    self.filters.mtime_min = Some(n);
                    self.filters.mtime_max = Some(
                        n.checked_add(86399)
                            .ok_or_else(|| self.error("Timestamp range overflows"))?,
                    );
                }
            }
            "sort" => {
                self.options.sort = match value.to_ascii_lowercase().as_str() {
                    "score" => SortOrder::Score,
                    "recency" | "recent" | "mtime" => SortOrder::Recency,
                    "path" | "name" => SortOrder::Path,
                    _ => return Err(self.error("Unknown sort order")),
                }
            }
            "top" => {
                self.options.limit = value
                    .parse()
                    .map_err(|_| self.error("Invalid result limit"))?;
                self.options.explicit_limit = true;
            }
            _ => unreachable!(),
        }
        Ok(QueryNode::Empty)
    }

    fn parse_timestamp(s: &str) -> Option<u64> {
        if s.bytes().all(|b| b.is_ascii_digit()) && !s.is_empty() {
            s.parse().ok()
        } else {
            Self::parse_date(s)
        }
    }

    fn parse_date(s: &str) -> Option<u64> {
        if s.len() != 10 || s.as_bytes()[4] != b'-' || s.as_bytes()[7] != b'-' {
            return None;
        }
        let year: i32 = s.get(..4)?.parse().ok()?;
        let month: u32 = s.get(5..7)?.parse().ok()?;
        let day: u32 = s.get(8..)?.parse().ok()?;
        if year < 1970 || !(1..=12).contains(&month) {
            return None;
        }
        let lengths = [
            31,
            if is_leap_year(year) { 29 } else { 28 },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ];
        if day == 0 || day > lengths[(month - 1) as usize] {
            return None;
        }
        let leap_days = |y: i32| y / 4 - y / 100 + y / 400;
        let days = (year - 1970) as u64 * 365
            + (leap_days(year - 1) - leap_days(1969)) as u64
            + days_before_month(month, is_leap_year(year))
            + (day - 1) as u64;
        days.checked_mul(86400)
    }

    fn parse_near_query(&self, value: &str) -> ParseResult<QueryNode> {
        let parts: Vec<_> = value.split(',').collect();
        let (terms, distance) =
            if parts.len() >= 3 && parts.last().unwrap().bytes().all(|b| b.is_ascii_digit()) {
                (
                    &parts[..parts.len() - 1],
                    parts
                        .last()
                        .unwrap()
                        .parse::<u32>()
                        .map_err(|_| self.error("Invalid proximity distance"))?,
                )
            } else {
                (parts.as_slice(), 10)
            };
        if terms.len() < 2 || terms.iter().any(|t| t.is_empty()) {
            return Err(self.error("near requires at least two nonempty terms"));
        }
        Ok(QueryNode::Near {
            terms: terms.iter().map(|s| s.to_string()).collect(),
            distance,
        })
    }

    fn skip_whitespace(&mut self) {
        while self.peek_char().is_some_and(char::is_whitespace) {
            self.advance();
        }
    }
    fn is_eof(&self) -> bool {
        self.pos >= self.input.len()
    }
    fn peek_char(&self) -> Option<char> {
        self.input[self.pos..].chars().next()
    }
    fn consume_char(&mut self, expected: char) -> bool {
        if self.peek_char() == Some(expected) {
            self.advance();
            true
        } else {
            false
        }
    }
    fn advance(&mut self) {
        if let Some(ch) = self.peek_char() {
            self.pos += ch.len_utf8();
        }
    }
    fn remaining(&self) -> &str {
        &self.input[self.pos..]
    }
}

/// Check if a year is a leap year
fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

/// Get days before a given month (0-indexed cumulative days)
fn days_before_month(month: u32, leap: bool) -> u64 {
    const DAYS: [u64; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let days = DAYS
        .get(month.saturating_sub(1) as usize)
        .copied()
        .unwrap_or(0);
    if leap && month > 2 { days + 1 } else { days }
}

impl Query {
    /// Validate public AST inputs as well as parsed strings before recursive work.
    pub fn validate(&self) -> ParseResult<()> {
        let mut stack = vec![(&self.root, 0usize)];
        let mut nodes = 0usize;
        let mut bytes = 0usize;
        while let Some((node, depth)) = stack.pop() {
            nodes += 1;
            if depth > MAX_QUERY_DEPTH * 3 || nodes > MAX_QUERY_NODES * 3 {
                return Err(QueryError {
                    message: "Query AST exceeds complexity limit".into(),
                    offset: 0,
                });
            }
            match node {
                QueryNode::Invalid(error) => return Err(error.clone()),
                QueryNode::And(children) | QueryNode::Or(children) => {
                    if children.len() > MAX_QUERY_NODES {
                        return Err(QueryError {
                            message: "Too many Boolean branches".into(),
                            offset: 0,
                        });
                    }
                    stack.extend(children.iter().map(|child| (child, depth + 1)));
                }
                QueryNode::Not(child) => stack.push((child, depth + 1)),
                QueryNode::BoostedLiteral { text, boost }
                | QueryNode::BoostedPhrase { text, boost } => {
                    if !boost.is_finite() || *boost < 0.0 {
                        return Err(QueryError {
                            message: "Boost must be finite and non-negative".into(),
                            offset: 0,
                        });
                    }
                    bytes = bytes.saturating_add(text.len());
                }
                QueryNode::Literal(text) | QueryNode::Phrase(text) | QueryNode::Regex(text) => {
                    bytes = bytes.saturating_add(text.len())
                }
                QueryNode::Near { terms, .. } => {
                    if terms.len() > MAX_QUERY_NODES {
                        return Err(QueryError {
                            message: "Too many proximity terms".into(),
                            offset: 0,
                        });
                    }
                    bytes = terms.iter().fold(bytes, |n, s| n.saturating_add(s.len()));
                }
                QueryNode::Empty => {}
            }
            if bytes > MAX_QUERY_BYTES * 4 {
                return Err(QueryError {
                    message: "Query AST text is too large".into(),
                    offset: 0,
                });
            }
        }
        Ok(())
    }

    /// Apply whole-word matching structurally, preserving each leaf's case mode.
    pub fn apply_word_boundaries(&mut self) -> ParseResult<()> {
        self.validate()?;
        fn rewrite(node: &QueryNode) -> ParseResult<QueryNode> {
            Ok(match node {
                QueryNode::Invalid(error) => return Err(error.clone()),
                QueryNode::Literal(s) => {
                    QueryNode::Regex(format!("(?i:\\b(?:{})\\b)", regex::escape(s)))
                }
                QueryNode::Phrase(s) => QueryNode::Regex(format!("\\b(?:{})\\b", regex::escape(s))),
                QueryNode::Regex(s) => QueryNode::Regex(format!("\\b(?:{s})\\b")),
                QueryNode::And(v) => {
                    QueryNode::And(v.iter().map(rewrite).collect::<ParseResult<_>>()?)
                }
                QueryNode::Or(v) => {
                    QueryNode::Or(v.iter().map(rewrite).collect::<ParseResult<_>>()?)
                }
                QueryNode::Not(v) => QueryNode::Not(Box::new(rewrite(v)?)),
                QueryNode::Empty => QueryNode::Empty,
                QueryNode::BoostedLiteral { .. }
                | QueryNode::BoostedPhrase { .. }
                | QueryNode::Near { .. } => {
                    return Err(QueryError {
                        message: "Whole-word mode does not support boosts or near queries".into(),
                        offset: 0,
                    });
                }
            })
        }
        self.root = rewrite(&self.root)?;
        Ok(())
    }

    /// Get the raw text for simple literal/phrase queries
    #[allow(dead_code)]
    pub fn get_search_text(&self) -> Option<&str> {
        match &self.root {
            QueryNode::Literal(s) | QueryNode::Phrase(s) => Some(s),
            _ => None,
        }
    }

    /// Check if query is empty (no search term AND no filters)
    pub fn is_empty(&self) -> bool {
        matches!(self.root, QueryNode::Empty) && !self.filters.has_query_filters()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_parser_rejects_malformed_scoped_and_unbounded_queries() {
        for input in [
            "foo) bar",
            "(foo",
            "()",
            "foo |",
            "| foo",
            "-",
            "^",
            "\"unclosed",
            "re:/foo",
            "ext:",
            "ext:rs | ext:py",
            "-ext:rs",
            "(ext:rs foo)",
            "^ext:rs",
            "ext:rs ext:py",
            "size:bogus",
            "size:>18446744073709551615",
            "size:<0",
            "line:0",
            "line:3-2",
            "line:2-x",
            "mtime:2026-02-31",
            "mtime:2100-02-29",
            "mtime:18446744073709551615",
            "mtime:2026-01-01-extra",
            "sort:nope",
            "top:abc",
            "path:[",
            "file:[",
            "lang:nonesuch",
            "near:foo",
            "near:foo,,2",
            "^99999999999999999999999999999999999999999999999999999:foo",
        ] {
            assert!(try_parse_query(input).is_err(), "{input}");
            assert!(parse_query(input).validate().is_err(), "legacy: {input}");
        }
        let nested = format!("{}foo{}", "(".repeat(10000), ")".repeat(10000));
        let wide = "foo ".repeat(MAX_QUERY_NODES + 1);
        let large = "x".repeat(MAX_QUERY_BYTES + 1);
        for input in [&nested, &wide, &large] {
            assert!(try_parse_query(input).is_err());
            assert!(parse_query(input).validate().is_err());
        }
        assert!(try_parse_query("ext:rs (foo | bar) top:7").is_ok());
        assert!(try_parse_query("foo | bar top:7").is_ok());
    }

    #[test]
    fn punctuation_and_delimiters_preserve_literal_intent() {
        for input in [
            "foo-bar",
            "foo.bar",
            "foo()",
            "foo(bar)",
            "std::vector",
            "src/foo.rs",
        ] {
            assert!(
                matches!(try_parse_query(input).unwrap().root,QueryNode::Literal(s) if s==input),
                "{input}"
            );
        }
        assert!(
            matches!(try_parse_query(r#""a\"b\\c""#).unwrap().root,QueryNode::Phrase(s) if s=="a\"b\\c")
        );
        for (input, expected) in [
            (r"re:/foo\/bar/", "foo/bar"),
            (r"re:/[/]/", "[/]"),
            (r"re:/[]/]/", "[]/]"),
            (r"re:/[^]/]/", "[^]/]"),
            (r"re:/(?x)[ ]/]/", "(?x)[ ]/]"),
            (r"re:/foo\\/", r"foo\\"),
        ] {
            assert!(
                matches!(try_parse_query(input).unwrap().root,QueryNode::Regex(s) if s==expected),
                "{input}"
            );
        }
        assert_eq!(
            try_parse_query(r#"path:"with space/*.rs" foo"#)
                .unwrap()
                .filters
                .path
                .as_deref(),
            Some("with space/*.rs")
        );
    }

    #[test]
    fn calendar_and_comparison_bounds_are_exact() {
        assert_eq!(QueryParser::parse_date("1970-01-01"), Some(0));
        assert_eq!(QueryParser::parse_date("2000-03-01"), Some(951868800));
        assert_eq!(QueryParser::parse_date("2101-03-01"), Some(4139078400));
        assert_eq!(QueryParser::parse_date("2400-02-29"), Some(13574563200));
        let q = try_parse_query("mtime:2026-09-18 size:>8 size:<10").unwrap();
        assert_eq!(q.filters.size_min, Some(9));
        assert_eq!(q.filters.size_max, Some(9));
        assert_eq!(
            q.filters.mtime_max.unwrap() - q.filters.mtime_min.unwrap(),
            86399
        );
        assert!(try_parse_query("top:0 foo").unwrap().options.explicit_limit);
        assert!(!try_parse_query("foo").unwrap().options.explicit_limit);
    }

    #[test]
    fn word_mode_preserves_literal_case_and_structural_boolean_meaning() {
        let mut query = try_parse_query(r#"(foo-bar | "Exact Case") -re:/skip\/this/"#).unwrap();
        query.apply_word_boundaries().unwrap();
        let QueryNode::And(nodes) = query.root else {
            panic!("expected AND")
        };
        let QueryNode::Or(branches) = &nodes[0] else {
            panic!("expected OR")
        };
        assert!(matches!(&branches[0],QueryNode::Regex(s) if s==r"(?i:\b(?:foo\-bar)\b)"));
        assert!(matches!(&branches[1],QueryNode::Regex(s) if s==r"\b(?:Exact Case)\b"));
        assert!(
            try_parse_query("^2:foo")
                .unwrap()
                .apply_word_boundaries()
                .is_err()
        );
        assert!(
            try_parse_query("near:foo,bar")
                .unwrap()
                .apply_word_boundaries()
                .is_err()
        );
    }

    #[test]
    fn test_simple_query() {
        let q = parse_query("hello");
        assert!(matches!(q.root, QueryNode::Literal(s) if s == "hello"));
    }

    #[test]
    fn test_phrase_query() {
        let q = parse_query("\"hello world\"");
        assert!(matches!(q.root, QueryNode::Phrase(s) if s == "hello world"));
    }

    #[test]
    fn test_and_query() {
        let q = parse_query("foo bar");
        assert!(matches!(q.root, QueryNode::And(_)));
    }

    #[test]
    fn test_or_query() {
        let q = parse_query("foo | bar");
        assert!(matches!(q.root, QueryNode::Or(_)));
    }

    #[test]
    fn test_not_query() {
        let q = parse_query("-test");
        assert!(matches!(q.root, QueryNode::Not(_)));
    }

    #[test]
    fn test_field_filter() {
        let q = parse_query("ext:rs foo");
        assert_eq!(q.filters.ext, Some("rs".to_string()));
    }

    #[test]
    fn test_regex() {
        let q = parse_query("re:/foo.*bar/");
        assert!(matches!(q.root, QueryNode::Regex(_)));
    }

    #[test]
    fn test_mtime_filter_min() {
        let q = parse_query("mtime:>1704067200 test");
        assert_eq!(q.filters.mtime_min, Some(1704067201));
        assert!(q.filters.mtime_max.is_none());
    }

    #[test]
    fn test_mtime_filter_max() {
        let q = parse_query("mtime:<1704067200 test");
        assert_eq!(q.filters.mtime_max, Some(1704067199));
        assert!(q.filters.mtime_min.is_none());
    }

    #[test]
    fn test_mtime_filter_date() {
        let q = parse_query("mtime:2024-01-01 test");
        assert!(q.filters.mtime_min.is_some());
        assert!(q.filters.mtime_max.is_some());
        // Inclusive upper bound ends one second before the next day.
        assert_eq!(
            q.filters.mtime_max.unwrap() - q.filters.mtime_min.unwrap(),
            86399
        );
    }

    #[test]
    fn test_near_query() {
        let q = parse_query("near:function,return,10");
        assert!(
            matches!(q.root, QueryNode::Near { ref terms, distance } if terms.len() == 2 && distance == 10)
        );
    }

    #[test]
    fn test_near_query_default_distance() {
        // If no valid distance is parsed, all elements are terms with default distance 10
        let q = parse_query("near:foo,bar,abc");
        assert!(
            matches!(q.root, QueryNode::Near { ref terms, distance } if terms.len() == 3 && distance == 10)
        );
    }

    #[test]
    fn test_boost_simple() {
        let q = parse_query("^test");
        assert!(
            matches!(q.root, QueryNode::BoostedLiteral { ref text, boost } if text == "test" && boost == 2.0)
        );
    }

    #[test]
    fn test_boost_with_value() {
        let q = parse_query("^3:important");
        assert!(
            matches!(q.root, QueryNode::BoostedLiteral { ref text, boost } if text == "important" && boost == 3.0)
        );
    }

    #[test]
    fn test_boost_float_value() {
        let q = parse_query("^1.5:term");
        assert!(
            matches!(q.root, QueryNode::BoostedLiteral { ref text, boost } if text == "term" && (boost - 1.5).abs() < 0.01)
        );
    }

    #[test]
    fn test_line_filter_single() {
        let q = parse_query("line:100 test");
        assert_eq!(q.filters.line_start, Some(100));
        assert_eq!(q.filters.line_end, Some(100));
    }

    #[test]
    fn test_line_filter_range() {
        let q = parse_query("line:100-200 test");
        assert_eq!(q.filters.line_start, Some(100));
        assert_eq!(q.filters.line_end, Some(200));
    }

    #[test]
    fn test_combined_filters() {
        let q = parse_query("ext:rs path:src mtime:>1704067200 ^important");
        assert_eq!(q.filters.ext, Some("rs".to_string()));
        assert_eq!(q.filters.path, Some("src".to_string()));
        assert_eq!(q.filters.mtime_min, Some(1704067201));
        // The query root contains the boosted term (filters produce Empty nodes that get filtered)
        match &q.root {
            QueryNode::BoostedLiteral { text, boost } => {
                assert_eq!(text, "important");
                assert!((boost - 2.0).abs() < 0.01);
            }
            QueryNode::And(nodes) => {
                // Should contain a BoostedLiteral among the nodes
                let has_boosted = nodes.iter().any(
                    |n| matches!(n, QueryNode::BoostedLiteral { text, .. } if text == "important"),
                );
                assert!(has_boosted, "Expected BoostedLiteral in And nodes");
            }
            _ => panic!("Expected BoostedLiteral or And node"),
        }
    }

    #[test]
    fn test_file_filter_exact() {
        let q = parse_query("file:main.rs");
        assert_eq!(q.filters.filename, Some("main.rs".to_string()));
        assert!(matches!(q.root, QueryNode::Empty));
    }

    #[test]
    fn test_file_filter_glob() {
        let q = parse_query("file:*.rs");
        assert_eq!(q.filters.filename, Some("*.rs".to_string()));
    }

    #[test]
    fn test_file_filter_with_search_term() {
        let q = parse_query("file:main.rs fn main");
        assert_eq!(q.filters.filename, Some("main.rs".to_string()));
        // Should have a search term in root
        assert!(!matches!(q.root, QueryNode::Empty));
    }

    #[test]
    fn test_name_filter_alias() {
        // name: is an alias for file:
        let q = parse_query("name:test.rs");
        assert_eq!(q.filters.filename, Some("test.rs".to_string()));
    }

    #[test]
    fn test_path_filter_simple() {
        let q = parse_query("path:src/lib.rs");
        assert_eq!(q.filters.path, Some("src/lib.rs".to_string()));
    }

    #[test]
    fn test_path_filter_glob() {
        let q = parse_query("path:src/**/*.rs");
        assert_eq!(q.filters.path, Some("src/**/*.rs".to_string()));
    }

    #[test]
    fn test_lang_filter() {
        let q = parse_query("lang:rust test");
        assert_eq!(q.filters.lang, Some("rust".to_string()));
    }

    #[test]
    fn test_lang_filter_aliases() {
        // Test various language aliases
        let q1 = parse_query("lang:rs");
        assert_eq!(q1.filters.lang, Some("rs".to_string()));

        let q2 = parse_query("lang:python");
        assert_eq!(q2.filters.lang, Some("python".to_string()));

        let q3 = parse_query("lang:js");
        assert_eq!(q3.filters.lang, Some("js".to_string()));
    }

    #[test]
    fn test_size_filter_min() {
        let q = parse_query("size:>1000 test");
        assert_eq!(q.filters.size_min, Some(1001));
        assert_eq!(q.filters.size_max, None);
    }

    #[test]
    fn test_size_filter_max() {
        let q = parse_query("size:<5000 test");
        assert_eq!(q.filters.size_min, None);
        assert_eq!(q.filters.size_max, Some(4999));
    }

    #[test]
    fn test_size_filter_both() {
        let q = parse_query("size:>100 size:<10000 test");
        assert_eq!(q.filters.size_min, Some(101));
        assert_eq!(q.filters.size_max, Some(9999));
    }

    #[test]
    fn test_sort_score() {
        let q = parse_query("sort:score test");
        assert_eq!(q.options.sort, SortOrder::Score);
    }

    #[test]
    fn test_sort_recency() {
        let q = parse_query("sort:recency test");
        assert_eq!(q.options.sort, SortOrder::Recency);
    }

    #[test]
    fn test_sort_path() {
        let q = parse_query("sort:path test");
        assert_eq!(q.options.sort, SortOrder::Path);
    }

    #[test]
    fn test_top_limit() {
        let q = parse_query("top:50 test");
        assert_eq!(q.options.limit, 50);
    }

    #[test]
    fn test_top_limit_zero() {
        // top:0 means unlimited
        let q = parse_query("top:0 test");
        assert_eq!(q.options.limit, 0); // 0 = unlimited
    }

    #[test]
    fn test_query_is_empty_no_filters() {
        let q = parse_query("");
        assert!(q.is_empty());
    }

    #[test]
    fn test_query_is_empty_with_search_term() {
        let q = parse_query("test");
        assert!(!q.is_empty());
    }

    #[test]
    fn test_query_is_empty_with_filter_only() {
        // file: filter only - should NOT be empty
        let q = parse_query("file:main.rs");
        assert!(!q.is_empty(), "Query with file filter should not be empty");
    }

    #[test]
    fn test_query_is_empty_with_ext_filter_only() {
        let q = parse_query("ext:rs");
        assert!(!q.is_empty(), "Query with ext filter should not be empty");
    }

    #[test]
    fn test_query_is_empty_with_path_filter_only() {
        let q = parse_query("path:src/*");
        assert!(!q.is_empty(), "Query with path filter should not be empty");
    }

    #[test]
    fn test_filters_has_any_empty() {
        let filters = QueryFilters::default();
        assert!(!filters.has_any());
    }

    #[test]
    fn test_filters_has_any_with_filename() {
        let filters = QueryFilters {
            filename: Some("test.rs".to_string()),
            ..Default::default()
        };
        assert!(filters.has_any());
    }

    #[test]
    fn test_filters_has_any_with_ext() {
        let filters = QueryFilters {
            ext: Some("rs".to_string()),
            ..Default::default()
        };
        assert!(filters.has_any());
    }

    #[test]
    fn test_filters_has_any_with_size() {
        let filters = QueryFilters {
            size_min: Some(100),
            ..Default::default()
        };
        assert!(filters.has_any());
    }

    #[test]
    fn test_complex_query_with_multiple_filters() {
        let q =
            parse_query("file:*.rs ext:rs lang:rust size:>100 path:src/* sort:recency top:20 test");
        assert_eq!(q.filters.filename, Some("*.rs".to_string()));
        assert_eq!(q.filters.ext, Some("rs".to_string()));
        assert_eq!(q.filters.lang, Some("rust".to_string()));
        assert_eq!(q.filters.size_min, Some(101));
        assert_eq!(q.filters.path, Some("src/*".to_string()));
        assert_eq!(q.options.sort, SortOrder::Recency);
        assert_eq!(q.options.limit, 20);
        assert!(!q.is_empty());
    }

    #[test]
    fn test_mtime_filter_both() {
        let q = parse_query("mtime:>1700000000 mtime:<1710000000 test");
        assert_eq!(q.filters.mtime_min, Some(1700000001));
        assert_eq!(q.filters.mtime_max, Some(1709999999));
    }

    // ========================================================================
    // Parenthesized grouping tests
    // ========================================================================

    #[test]
    fn test_paren_simple_or_group() {
        // (foo | bar) baz → And([Or([foo, bar]), baz])
        let q = parse_query("(foo | bar) baz");
        match &q.root {
            QueryNode::And(nodes) => {
                assert_eq!(nodes.len(), 2);
                assert!(matches!(&nodes[0], QueryNode::Or(inner) if inner.len() == 2));
                assert!(matches!(&nodes[1], QueryNode::Literal(s) if s == "baz"));
            }
            _ => panic!("Expected And node, got {:?}", q.root),
        }
    }

    #[test]
    fn test_paren_nested() {
        // ((a | b) | c) → Or([Or([a, b]), c])
        let q = parse_query("((a | b) | c)");
        match &q.root {
            QueryNode::Or(nodes) => {
                assert_eq!(nodes.len(), 2);
                assert!(matches!(&nodes[0], QueryNode::Or(inner) if inner.len() == 2));
                assert!(matches!(&nodes[1], QueryNode::Literal(s) if s == "c"));
            }
            _ => panic!("Expected Or node, got {:?}", q.root),
        }
    }

    #[test]
    fn test_paren_with_not() {
        // (foo | bar) -baz → And([Or([foo, bar]), Not(baz)])
        let q = parse_query("(foo | bar) -baz");
        match &q.root {
            QueryNode::And(nodes) => {
                assert_eq!(nodes.len(), 2);
                assert!(matches!(&nodes[0], QueryNode::Or(_)));
                assert!(matches!(&nodes[1], QueryNode::Not(_)));
            }
            _ => panic!("Expected And node, got {:?}", q.root),
        }
    }

    #[test]
    fn test_paren_empty() {
        // Empty groups are rejected.
        let q = parse_query("()");
        assert!(matches!(q.root, QueryNode::Invalid(_)));
    }

    #[test]
    fn test_paren_unclosed() {
        // Unclosed groups retain a safe error for legacy executor callers.
        let q = parse_query("(foo | bar");
        assert!(matches!(q.root, QueryNode::Invalid(_)));
    }

    #[test]
    fn test_paren_multiple_groups() {
        // (a | b) (c | d) → And([Or([a, b]), Or([c, d])])
        let q = parse_query("(a | b) (c | d)");
        match &q.root {
            QueryNode::And(nodes) => {
                assert_eq!(nodes.len(), 2);
                assert!(matches!(&nodes[0], QueryNode::Or(_)));
                assert!(matches!(&nodes[1], QueryNode::Or(_)));
            }
            _ => panic!("Expected And node, got {:?}", q.root),
        }
    }

    #[test]
    fn test_paren_with_phrase_inside() {
        // ("hello world" | bar) → Or([Phrase, Literal])
        let q = parse_query("(\"hello world\" | bar)");
        match &q.root {
            QueryNode::Or(nodes) => {
                assert_eq!(nodes.len(), 2);
                assert!(matches!(&nodes[0], QueryNode::Phrase(s) if s == "hello world"));
                assert!(matches!(&nodes[1], QueryNode::Literal(s) if s == "bar"));
            }
            _ => panic!("Expected Or node, got {:?}", q.root),
        }
    }

    // ========================================================================
    // NOT operator edge cases
    // ========================================================================

    #[test]
    fn test_not_with_and() {
        // foo -bar → And([Literal(foo), Not(Literal(bar))])
        let q = parse_query("foo -bar");
        match &q.root {
            QueryNode::And(nodes) => {
                assert_eq!(nodes.len(), 2);
                assert!(matches!(&nodes[0], QueryNode::Literal(s) if s == "foo"));
                match &nodes[1] {
                    QueryNode::Not(inner) => {
                        assert!(matches!(inner.as_ref(), QueryNode::Literal(s) if s == "bar"));
                    }
                    _ => panic!("Expected Not node"),
                }
            }
            _ => panic!("Expected And node, got {:?}", q.root),
        }
    }

    #[test]
    fn test_not_multiple() {
        // foo -bar -baz → And([foo, Not(bar), Not(baz)])
        let q = parse_query("foo -bar -baz");
        match &q.root {
            QueryNode::And(nodes) => {
                assert_eq!(nodes.len(), 3);
                assert!(matches!(&nodes[0], QueryNode::Literal(s) if s == "foo"));
                assert!(matches!(&nodes[1], QueryNode::Not(_)));
                assert!(matches!(&nodes[2], QueryNode::Not(_)));
            }
            _ => panic!("Expected And node, got {:?}", q.root),
        }
    }

    // ========================================================================
    // Regex edge cases
    // ========================================================================

    #[test]
    fn test_regex_with_special_chars() {
        let q = parse_query(r"re:/fn\s+\w+/");
        match &q.root {
            QueryNode::Regex(pat) => assert_eq!(pat, r"fn\s+\w+"),
            _ => panic!("Expected Regex node, got {:?}", q.root),
        }
    }

    #[test]
    fn test_regex_empty() {
        let q = parse_query("re://");
        match &q.root {
            QueryNode::Regex(pat) => assert!(pat.is_empty()),
            _ => panic!("Expected Regex node, got {:?}", q.root),
        }
    }

    #[test]
    fn test_regex_with_filter() {
        let q = parse_query("ext:rs re:/TODO.*fix/");
        assert_eq!(q.filters.ext, Some("rs".to_string()));
        // Filter produces Empty node, combined with Regex → And([Empty, Regex])
        match &q.root {
            QueryNode::Regex(pat) => assert_eq!(pat, "TODO.*fix"),
            QueryNode::And(nodes) => {
                let has_regex = nodes
                    .iter()
                    .any(|n| matches!(n, QueryNode::Regex(pat) if pat == "TODO.*fix"));
                assert!(
                    has_regex,
                    "Should contain Regex node in And, got {:?}",
                    nodes
                );
            }
            _ => panic!("Expected Regex or And node, got {:?}", q.root),
        }
    }

    // ========================================================================
    // Near edge cases
    // ========================================================================

    #[test]
    fn test_near_single_term_becomes_empty() {
        // near:foo has no commas, so parts.len() < 2 → Empty
        let q = parse_query("near:foo");
        assert!(
            matches!(&q.root, QueryNode::Invalid(_)),
            "Single-term near (no commas) should be rejected, got {:?}",
            q.root
        );
    }

    #[test]
    fn test_near_two_terms_no_distance_becomes_literal() {
        // near:foo,bar - last element parses as non-numeric, so all are terms with default distance
        let q = parse_query("near:foo,bar");
        assert!(
            matches!(&q.root, QueryNode::Near { terms, distance } if terms.len() == 2 && *distance == 10),
            "Two terms without numeric distance should use default distance 10, got {:?}",
            q.root
        );
    }

    #[test]
    fn test_near_with_other_terms() {
        // near:a,b,3 extra → And([Near{...}, Literal(extra)])
        let q = parse_query("near:a,b,3 extra");
        match &q.root {
            QueryNode::And(nodes) => {
                assert_eq!(nodes.len(), 2);
                assert!(
                    matches!(&nodes[0], QueryNode::Near { terms, distance } if terms.len() == 2 && *distance == 3)
                );
                assert!(matches!(&nodes[1], QueryNode::Literal(s) if s == "extra"));
            }
            _ => panic!("Expected And node, got {:?}", q.root),
        }
    }

    // ========================================================================
    // Boost edge cases
    // ========================================================================

    #[test]
    fn test_boost_zero() {
        let q = parse_query("^0:term");
        assert!(
            matches!(&q.root, QueryNode::BoostedLiteral { text, boost } if text == "term" && *boost == 0.0)
        );
    }

    #[test]
    fn test_boost_on_phrase() {
        let q = parse_query("^3:\"exact phrase\"");
        assert!(
            matches!(&q.root, QueryNode::BoostedPhrase { text, boost } if text == "exact phrase" && *boost == 3.0)
        );
    }

    // ========================================================================
    // Line filter edge cases
    // ========================================================================

    #[test]
    fn test_line_filter_zero() {
        let q = parse_query("line:0 test");
        assert!(matches!(q.root, QueryNode::Invalid(_)));
    }

    #[test]
    fn test_line_filter_reversed_range() {
        // Reversed line ranges are rejected.
        let q = parse_query("line:200-100 test");
        assert!(matches!(q.root, QueryNode::Invalid(_)));
    }

    // ========================================================================
    // Size filter edge cases
    // ========================================================================

    #[test]
    fn test_size_filter_zero() {
        let q = parse_query("size:>0 test");
        assert_eq!(q.filters.size_min, Some(1));
    }

    #[test]
    fn test_size_filter_invalid() {
        // Invalid size value should be ignored
        let q = parse_query("size:>abc test");
        assert_eq!(q.filters.size_min, None);
    }

    // ========================================================================
    // Unknown field edge cases
    // ========================================================================

    #[test]
    fn test_unknown_field_treated_as_literal() {
        let q = parse_query("foo:bar");
        assert!(matches!(&q.root, QueryNode::Literal(s) if s == "foo:bar"));
    }

    // ========================================================================
    // Empty / whitespace edge cases
    // ========================================================================

    #[test]
    fn test_whitespace_only() {
        let q = parse_query("   ");
        assert!(q.is_empty());
    }

    #[test]
    fn test_only_filters_no_search_term() {
        let q = parse_query("ext:rs lang:rust");
        assert_eq!(q.filters.ext, Some("rs".to_string()));
        assert_eq!(q.filters.lang, Some("rust".to_string()));
        // Each filter returns Empty node; two Empties → And([Empty, Empty])
        // The query is NOT considered empty because filters are set
        assert!(!q.is_empty());
    }
}
