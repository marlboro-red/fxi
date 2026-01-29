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
    /// Simple literal search
    Literal(String),
    /// Simple literal search with boost
    BoostedLiteral { text: String, boost: f32 },
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
    /// Case sensitive
    #[allow(dead_code)]
    pub case_sensitive: bool,
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            sort: SortOrder::Score,
            limit: 100,
            case_sensitive: false,
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

/// Parse a query string into a Query structure
pub fn parse_query(input: &str) -> Query {
    let mut parser = QueryParser::new(input);
    parser.parse()
}

/// Query parser
struct QueryParser<'a> {
    input: &'a str,
    pos: usize,
    filters: QueryFilters,
    options: QueryOptions,
}

impl<'a> QueryParser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input,
            pos: 0,
            filters: QueryFilters::default(),
            options: QueryOptions::default(),
        }
    }

    fn parse(&mut self) -> Query {
        let root = self.parse_or();
        Query {
            root,
            filters: self.filters.clone(),
            options: self.options.clone(),
        }
    }

    fn parse_or(&mut self) -> QueryNode {
        let mut nodes = vec![self.parse_and()];

        self.skip_whitespace();
        while self.consume_char('|') {
            self.skip_whitespace();
            nodes.push(self.parse_and());
            self.skip_whitespace();
        }

        if nodes.len() == 1 {
            nodes.pop().unwrap()
        } else {
            QueryNode::Or(nodes)
        }
    }

    fn parse_and(&mut self) -> QueryNode {
        let mut nodes = Vec::new();

        loop {
            self.skip_whitespace();

            if self.is_eof() || self.peek_char() == Some(')') || self.peek_char() == Some('|') {
                break;
            }

            nodes.push(self.parse_unary());
        }

        match nodes.len() {
            0 => QueryNode::Empty,
            1 => nodes.pop().unwrap(),
            _ => QueryNode::And(nodes),
        }
    }

    fn parse_unary(&mut self) -> QueryNode {
        self.skip_whitespace();

        if self.consume_char('-') {
            let inner = self.parse_primary();
            return QueryNode::Not(Box::new(inner));
        }

        // Handle boost prefix ^term or ^N:term (e.g., ^foo, ^2:foo, ^1.5:term)
        if self.consume_char('^') {
            // Try to parse optional boost value
            let mut boost = 2.0_f32; // Default boost value
            let boost_start = self.pos;

            // Check for explicit boost value like ^2:term or ^1.5:term
            while !self.is_eof() {
                let ch = self.peek_char().unwrap();
                if ch.is_ascii_digit() || ch == '.' {
                    self.advance();
                } else if ch == ':' {
                    let boost_str = &self.input[boost_start..self.pos];
                    if let Ok(b) = boost_str.parse::<f32>() {
                        boost = b;
                    }
                    self.advance(); // consume ':'
                    break;
                } else {
                    // No explicit boost value, reset position
                    self.pos = boost_start;
                    break;
                }
            }

            let inner = self.parse_primary();
            return match inner {
                QueryNode::Literal(text) => QueryNode::BoostedLiteral { text, boost },
                QueryNode::Phrase(text) => QueryNode::BoostedLiteral { text, boost },
                other => other, // Can't boost complex nodes, return as-is
            };
        }

        self.parse_primary()
    }

    fn parse_primary(&mut self) -> QueryNode {
        self.skip_whitespace();

        // Parenthesized expression
        if self.consume_char('(') {
            let node = self.parse_or();
            self.consume_char(')');
            return node;
        }

        // Quoted phrase
        if self.peek_char() == Some('"') {
            return self.parse_phrase();
        }

        // Regex
        if self.remaining().starts_with("re:/") {
            return self.parse_regex();
        }

        // Field filter or literal
        self.parse_term()
    }

    fn parse_phrase(&mut self) -> QueryNode {
        self.consume_char('"');
        let start = self.pos;

        while !self.is_eof() && self.peek_char() != Some('"') {
            self.advance();
        }

        let phrase = self.input[start..self.pos].to_string();
        self.consume_char('"');

        QueryNode::Phrase(phrase)
    }

    fn parse_regex(&mut self) -> QueryNode {
        // Skip "re:/"
        self.pos += 4;
        let start = self.pos;

        // Find closing /
        while !self.is_eof() && self.peek_char() != Some('/') {
            self.advance();
        }

        let pattern = self.input[start..self.pos].to_string();
        self.consume_char('/');

        QueryNode::Regex(pattern)
    }

    fn parse_term(&mut self) -> QueryNode {
        let start = self.pos;

        // Check for field prefix
        while !self.is_eof() {
            let ch = self.peek_char().unwrap();
            if ch.is_alphanumeric() || ch == '_' || ch == ':' {
                self.advance();
                if ch == ':' {
                    let field = &self.input[start..self.pos - 1];
                    return self.parse_field(field);
                }
            } else {
                break;
            }
        }

        // Regular word
        let word = self.input[start..self.pos].to_string();
        if word.is_empty() {
            // Try to consume any non-whitespace
            while !self.is_eof() {
                let ch = self.peek_char().unwrap();
                if ch.is_whitespace() || ch == '|' || ch == ')' || ch == '(' {
                    break;
                }
                self.advance();
            }
            let word = self.input[start..self.pos].to_string();
            if word.is_empty() {
                return QueryNode::Empty;
            }
            return QueryNode::Literal(word);
        }

        QueryNode::Literal(word)
    }

    fn parse_field(&mut self, field: &str) -> QueryNode {
        let value_start = self.pos;

        // Read value until whitespace or special char
        while !self.is_eof() {
            let ch = self.peek_char().unwrap();
            if ch.is_whitespace() || ch == '|' || ch == ')' {
                break;
            }
            self.advance();
        }

        let value = self.input[value_start..self.pos].to_string();

        match field.to_lowercase().as_str() {
            "path" => {
                self.filters.path = Some(value);
                QueryNode::Empty
            }
            "file" | "name" => {
                self.filters.filename = Some(value);
                QueryNode::Empty
            }
            "ext" => {
                self.filters.ext = Some(value);
                QueryNode::Empty
            }
            "lang" => {
                self.filters.lang = Some(value);
                QueryNode::Empty
            }
            "size" => {
                self.parse_size_filter(&value);
                QueryNode::Empty
            }
            "line" => {
                self.parse_line_filter(&value);
                QueryNode::Empty
            }
            "mtime" => {
                self.parse_mtime_filter(&value);
                QueryNode::Empty
            }
            "near" => {
                // Parse near:term1,term2,distance
                return self.parse_near_query(&value);
            }
            "sort" => {
                self.parse_sort(&value);
                QueryNode::Empty
            }
            "top" => {
                if let Ok(n) = value.parse() {
                    self.options.limit = n;
                }
                QueryNode::Empty
            }
            _ => {
                // Unknown field, treat as literal
                QueryNode::Literal(format!("{}:{}", field, value))
            }
        }
    }

    fn parse_size_filter(&mut self, value: &str) {
        if let Some(rest) = value.strip_prefix('>') {
            if let Ok(n) = rest.parse() {
                self.filters.size_min = Some(n);
            }
        } else if let Some(rest) = value.strip_prefix('<') {
            if let Ok(n) = rest.parse() {
                self.filters.size_max = Some(n);
            }
        }
    }

    fn parse_line_filter(&mut self, value: &str) {
        if let Some((start, end)) = value.split_once('-') {
            self.filters.line_start = start.parse().ok();
            self.filters.line_end = end.parse().ok();
        } else if let Ok(n) = value.parse() {
            self.filters.line_start = Some(n);
            self.filters.line_end = Some(n);
        }
    }

    fn parse_sort(&mut self, value: &str) {
        self.options.sort = match value.to_lowercase().as_str() {
            "recency" | "recent" | "mtime" => SortOrder::Recency,
            "path" | "name" => SortOrder::Path,
            _ => SortOrder::Score,
        };
    }

    fn parse_mtime_filter(&mut self, value: &str) {
        // Parse mtime:>timestamp, mtime:<timestamp, or mtime:YYYY-MM-DD
        if let Some(rest) = value.strip_prefix('>') {
            self.filters.mtime_min = Self::parse_timestamp(rest);
        } else if let Some(rest) = value.strip_prefix('<') {
            self.filters.mtime_max = Self::parse_timestamp(rest);
        } else if value.contains('-') && value.len() >= 10 {
            // Parse as date: YYYY-MM-DD (start of day)
            if let Some(ts) = Self::parse_date(value) {
                // Set both min and max for a specific day
                self.filters.mtime_min = Some(ts);
                self.filters.mtime_max = Some(ts + 86400); // Next day
            }
        } else if let Ok(n) = value.parse::<u64>() {
            // Direct timestamp
            self.filters.mtime_min = Some(n);
            self.filters.mtime_max = Some(n + 86400);
        }
    }

    fn parse_timestamp(s: &str) -> Option<u64> {
        // First try parsing as a direct timestamp
        if let Ok(n) = s.parse::<u64>() {
            return Some(n);
        }
        // Try parsing as YYYY-MM-DD date
        Self::parse_date(s)
    }

    fn parse_date(s: &str) -> Option<u64> {
        // Parse YYYY-MM-DD format
        let parts: Vec<&str> = s.split('-').collect();
        if parts.len() >= 3 {
            let year: i32 = parts[0].parse().ok()?;
            let month: u32 = parts[1].parse().ok()?;
            let day: u32 = parts[2].parse().ok()?;

            // Simple calculation: days since Unix epoch
            // Note: This is approximate, ignoring leap seconds
            if year >= 1970 && month >= 1 && month <= 12 && day >= 1 && day <= 31 {
                let days_since_epoch = (year - 1970) as u64 * 365
                    + ((year - 1969) / 4) as u64  // Leap years
                    + days_before_month(month, is_leap_year(year))
                    + (day - 1) as u64;
                return Some(days_since_epoch * 86400);
            }
        }
        None
    }

    fn parse_near_query(&self, value: &str) -> QueryNode {
        // Parse near:term1,term2,distance format
        let parts: Vec<&str> = value.split(',').collect();
        if parts.len() >= 2 {
            let terms: Vec<String> = parts[..parts.len().saturating_sub(1)]
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();

            // Last part should be the distance
            let distance = parts.last()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(10); // Default distance of 10 lines

            if terms.len() >= 2 {
                return QueryNode::Near { terms, distance };
            } else if terms.len() == 1 {
                // If only one term, treat as literal
                return QueryNode::Literal(terms.into_iter().next().unwrap());
            }
        }
        QueryNode::Empty
    }

    fn skip_whitespace(&mut self) {
        while !self.is_eof() && self.peek_char().map(|c| c.is_whitespace()).unwrap_or(false) {
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
    let days = DAYS.get(month.saturating_sub(1) as usize).copied().unwrap_or(0);
    if leap && month > 2 {
        days + 1
    } else {
        days
    }
}

impl Query {
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
        matches!(self.root, QueryNode::Empty) && !self.filters.has_any()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(q.filters.mtime_min, Some(1704067200));
        assert!(q.filters.mtime_max.is_none());
    }

    #[test]
    fn test_mtime_filter_max() {
        let q = parse_query("mtime:<1704067200 test");
        assert_eq!(q.filters.mtime_max, Some(1704067200));
        assert!(q.filters.mtime_min.is_none());
    }

    #[test]
    fn test_mtime_filter_date() {
        let q = parse_query("mtime:2024-01-01 test");
        assert!(q.filters.mtime_min.is_some());
        assert!(q.filters.mtime_max.is_some());
        // The mtime_max should be 86400 seconds (1 day) after mtime_min
        assert_eq!(
            q.filters.mtime_max.unwrap() - q.filters.mtime_min.unwrap(),
            86400
        );
    }

    #[test]
    fn test_near_query() {
        let q = parse_query("near:function,return,10");
        assert!(matches!(q.root, QueryNode::Near { ref terms, distance } if terms.len() == 2 && distance == 10));
    }

    #[test]
    fn test_near_query_default_distance() {
        // If no valid distance is parsed, should use default of 10
        let q = parse_query("near:foo,bar,abc");
        assert!(
            matches!(q.root, QueryNode::Near { ref terms, distance } if terms.len() == 2 && distance == 10)
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
        assert_eq!(q.filters.mtime_min, Some(1704067200));
        // The query root contains the boosted term (filters produce Empty nodes that get filtered)
        match &q.root {
            QueryNode::BoostedLiteral { text, boost } => {
                assert_eq!(text, "important");
                assert!((boost - 2.0).abs() < 0.01);
            }
            QueryNode::And(nodes) => {
                // Should contain a BoostedLiteral among the nodes
                let has_boosted = nodes.iter().any(|n| {
                    matches!(n, QueryNode::BoostedLiteral { text, .. } if text == "important")
                });
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
        assert_eq!(q.filters.size_min, Some(1000));
        assert_eq!(q.filters.size_max, None);
    }

    #[test]
    fn test_size_filter_max() {
        let q = parse_query("size:<5000 test");
        assert_eq!(q.filters.size_min, None);
        assert_eq!(q.filters.size_max, Some(5000));
    }

    #[test]
    fn test_size_filter_both() {
        let q = parse_query("size:>100 size:<10000 test");
        assert_eq!(q.filters.size_min, Some(100));
        assert_eq!(q.filters.size_max, Some(10000));
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
        let mut filters = QueryFilters::default();
        filters.filename = Some("test.rs".to_string());
        assert!(filters.has_any());
    }

    #[test]
    fn test_filters_has_any_with_ext() {
        let mut filters = QueryFilters::default();
        filters.ext = Some("rs".to_string());
        assert!(filters.has_any());
    }

    #[test]
    fn test_filters_has_any_with_size() {
        let mut filters = QueryFilters::default();
        filters.size_min = Some(100);
        assert!(filters.has_any());
    }

    #[test]
    fn test_complex_query_with_multiple_filters() {
        let q = parse_query("file:*.rs ext:rs lang:rust size:>100 path:src/* sort:recency top:20 test");
        assert_eq!(q.filters.filename, Some("*.rs".to_string()));
        assert_eq!(q.filters.ext, Some("rs".to_string()));
        assert_eq!(q.filters.lang, Some("rust".to_string()));
        assert_eq!(q.filters.size_min, Some(100));
        assert_eq!(q.filters.path, Some("src/*".to_string()));
        assert_eq!(q.options.sort, SortOrder::Recency);
        assert_eq!(q.options.limit, 20);
        assert!(!q.is_empty());
    }

    #[test]
    fn test_mtime_filter_both() {
        let q = parse_query("mtime:>1700000000 mtime:<1710000000 test");
        assert_eq!(q.filters.mtime_min, Some(1700000000));
        assert_eq!(q.filters.mtime_max, Some(1710000000));
    }
}
