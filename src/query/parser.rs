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
    /// Exact phrase search (quoted)
    Phrase(String),
    /// Regex pattern
    Regex(String),
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
}

/// Query options
#[derive(Debug, Clone)]
pub struct QueryOptions {
    /// Sort order
    pub sort: SortOrder,
    /// Maximum results
    pub limit: usize,
    /// Case sensitive
    pub case_sensitive: bool,
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            sort: SortOrder::Score,
            limit: 1000,
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

impl Query {
    /// Get the raw text for simple literal/phrase queries
    pub fn get_search_text(&self) -> Option<&str> {
        match &self.root {
            QueryNode::Literal(s) | QueryNode::Phrase(s) => Some(s),
            _ => None,
        }
    }

    /// Check if query is empty
    pub fn is_empty(&self) -> bool {
        matches!(self.root, QueryNode::Empty)
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
}
