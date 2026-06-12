use std::collections::HashSet;

/// Maximum token length to store in the index.
/// Tokens longer than this are likely base64, hex dumps, or other non-searchable content.
const MAX_TOKEN_LENGTH: usize = 128;

/// Extract tokens from source code content
/// Handles: identifiers, snake_case splits, camelCase splits, words
/// Optimized for speed with byte-level processing where possible.
pub fn extract_tokens(content: &str) -> HashSet<String> {
    let bytes = content.as_bytes();

    // For small files, use simple approach
    if bytes.len() < 256 {
        return extract_tokens_simple(content);
    }

    // Pre-allocate with reasonable capacity
    let mut tokens = HashSet::with_capacity(bytes.len() / 8);

    // Process as bytes for speed (ASCII-focused, as most code is ASCII)
    let mut token_start: Option<usize> = None;
    let mut prev_was_lower = false;

    for (i, &byte) in bytes.iter().enumerate() {
        let is_lower = byte.is_ascii_lowercase();
        let is_upper = byte.is_ascii_uppercase();
        let is_digit = byte.is_ascii_digit();
        let is_underscore = byte == b'_';

        if is_lower || is_upper || is_digit {
            // CamelCase split: uppercase after lowercase
            if is_upper && prev_was_lower {
                if let Some(start) = token_start {
                    let slice = &bytes[start..i];
                    if slice.len() >= 2 && slice.len() <= MAX_TOKEN_LENGTH {
                        // SAFETY: we only process ASCII bytes
                        let s = unsafe { std::str::from_utf8_unchecked(slice) };
                        tokens.insert(s.to_ascii_lowercase());
                    }
                }
                token_start = Some(i);
            } else if token_start.is_none() {
                token_start = Some(i);
            }
            prev_was_lower = is_lower;
        } else {
            // End of token (underscore or other char)
            if let Some(start) = token_start {
                let slice = &bytes[start..i];
                if slice.len() >= 2 && slice.len() <= MAX_TOKEN_LENGTH {
                    // SAFETY: we only process ASCII bytes
                    let s = unsafe { std::str::from_utf8_unchecked(slice) };
                    tokens.insert(s.to_ascii_lowercase());
                }
            }
            token_start = None;
            prev_was_lower = false;

            // Skip non-ASCII chars entirely
            if !is_underscore && byte > 127 {
                // Non-ASCII: skip to next ASCII
                continue;
            }
        }
    }

    // Handle last token
    if let Some(start) = token_start {
        let slice = &bytes[start..];
        if slice.len() >= 2 && slice.len() <= MAX_TOKEN_LENGTH {
            // Check if it's valid UTF-8 ASCII
            if slice.iter().all(|&b| b < 128) {
                let s = unsafe { std::str::from_utf8_unchecked(slice) };
                tokens.insert(s.to_ascii_lowercase());
            }
        }
    }

    tokens
}

/// Simple tokenization for small content (original algorithm)
fn extract_tokens_simple(content: &str) -> HashSet<String> {
    let mut tokens = HashSet::new();
    let mut current_token = String::new();
    let mut prev_char_type = CharType::Other;

    for ch in content.chars() {
        let char_type = classify_char(ch);

        match char_type {
            CharType::Lower | CharType::Digit => {
                current_token.push(ch);
            }
            CharType::Upper => {
                if prev_char_type == CharType::Lower && !current_token.is_empty() {
                    add_token(&mut tokens, &current_token);
                    current_token.clear();
                }
                current_token.push(ch.to_ascii_lowercase());
            }
            CharType::Underscore => {
                if !current_token.is_empty() {
                    add_token(&mut tokens, &current_token);
                    current_token.clear();
                }
            }
            CharType::Other => {
                if !current_token.is_empty() {
                    add_token(&mut tokens, &current_token);
                    current_token.clear();
                }
            }
        }

        prev_char_type = char_type;
    }

    if !current_token.is_empty() {
        add_token(&mut tokens, &current_token);
    }

    tokens
}

/// Extract tokens suitable for query matching
#[allow(dead_code)]
pub fn tokenize_query(query: &str) -> Vec<String> {
    let tokens = extract_tokens(query);
    let mut result: Vec<_> = tokens.into_iter().collect();
    result.sort();
    result
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CharType {
    Upper,
    Lower,
    Digit,
    Underscore,
    Other,
}

fn classify_char(ch: char) -> CharType {
    if ch.is_ascii_uppercase() {
        CharType::Upper
    } else if ch.is_ascii_lowercase() {
        CharType::Lower
    } else if ch.is_ascii_digit() {
        CharType::Digit
    } else if ch == '_' {
        CharType::Underscore
    } else {
        CharType::Other
    }
}

fn add_token(tokens: &mut HashSet<String>, token: &str) {
    // Only add tokens of meaningful length, skip overly long tokens
    if token.len() >= 2 && token.len() <= MAX_TOKEN_LENGTH {
        tokens.insert(token.to_lowercase());
    }
}

/// Extract tokens with their word positions from source code content.
/// Returns Vec<(token, position)> where position is the sequential word index.
/// The position counter increments for every token boundary (including sub-2-char
/// tokens that are filtered from the index) to maintain gap consistency between
/// index-time and query-time tokenization.
pub fn extract_tokens_with_positions(content: &str) -> Vec<(String, u32)> {
    let bytes = content.as_bytes();

    if bytes.len() < 256 {
        return extract_tokens_with_positions_simple(content);
    }

    let mut result = Vec::with_capacity(bytes.len() / 8);
    let mut token_start: Option<usize> = None;
    let mut prev_was_lower = false;
    let mut word_pos: u32 = 0;

    for (i, &byte) in bytes.iter().enumerate() {
        let is_lower = byte.is_ascii_lowercase();
        let is_upper = byte.is_ascii_uppercase();
        let is_digit = byte.is_ascii_digit();

        if is_lower || is_upper || is_digit {
            // CamelCase split: uppercase after lowercase
            if is_upper && prev_was_lower {
                if let Some(start) = token_start {
                    let slice = &bytes[start..i];
                    if slice.len() >= 2
                        && slice.len() <= MAX_TOKEN_LENGTH
                        && slice.iter().all(|&b| b < 128)
                    {
                        let s = unsafe { std::str::from_utf8_unchecked(slice) };
                        result.push((s.to_ascii_lowercase(), word_pos));
                    }
                    word_pos += 1;
                }
                token_start = Some(i);
            } else if token_start.is_none() {
                token_start = Some(i);
            }
            prev_was_lower = is_lower;
        } else {
            if let Some(start) = token_start {
                let slice = &bytes[start..i];
                if slice.len() >= 2
                    && slice.len() <= MAX_TOKEN_LENGTH
                    && slice.iter().all(|&b| b < 128)
                {
                    let s = unsafe { std::str::from_utf8_unchecked(slice) };
                    result.push((s.to_ascii_lowercase(), word_pos));
                }
                word_pos += 1;
            }
            token_start = None;
            prev_was_lower = false;
        }
    }

    // Handle last token
    if let Some(start) = token_start {
        let slice = &bytes[start..];
        if slice.len() >= 2 && slice.len() <= MAX_TOKEN_LENGTH && slice.iter().all(|&b| b < 128) {
            let s = unsafe { std::str::from_utf8_unchecked(slice) };
            result.push((s.to_ascii_lowercase(), word_pos));
        }
    }

    result
}

/// Simple position-aware tokenization for small content
fn extract_tokens_with_positions_simple(content: &str) -> Vec<(String, u32)> {
    let mut result = Vec::new();
    let mut current_token = String::new();
    let mut prev_char_type = CharType::Other;
    let mut word_pos: u32 = 0;

    for ch in content.chars() {
        let char_type = classify_char(ch);

        match char_type {
            CharType::Lower | CharType::Digit => {
                current_token.push(ch);
            }
            CharType::Upper => {
                if prev_char_type == CharType::Lower && !current_token.is_empty() {
                    if current_token.len() >= 2 && current_token.len() <= MAX_TOKEN_LENGTH {
                        result.push((current_token.to_lowercase(), word_pos));
                    }
                    word_pos += 1;
                    current_token.clear();
                }
                current_token.push(ch.to_ascii_lowercase());
            }
            CharType::Underscore => {
                if !current_token.is_empty() {
                    if current_token.len() >= 2 && current_token.len() <= MAX_TOKEN_LENGTH {
                        result.push((current_token.to_lowercase(), word_pos));
                    }
                    word_pos += 1;
                    current_token.clear();
                }
            }
            CharType::Other => {
                if !current_token.is_empty() {
                    if current_token.len() >= 2 && current_token.len() <= MAX_TOKEN_LENGTH {
                        result.push((current_token.to_lowercase(), word_pos));
                    }
                    word_pos += 1;
                    current_token.clear();
                }
            }
        }

        prev_char_type = char_type;
    }

    if !current_token.is_empty()
        && current_token.len() >= 2
        && current_token.len() <= MAX_TOKEN_LENGTH
    {
        result.push((current_token.to_lowercase(), word_pos));
    }

    result
}

/// Tokenize a query phrase with positions.
/// Uses the same tokenization logic as extract_tokens_with_positions
/// to ensure positions match between index-time and query-time.
pub fn tokenize_query_with_positions(query: &str) -> Vec<(String, u32)> {
    extract_tokens_with_positions_simple(query)
}

/// Tokenize content in a single scan, returning both the unique token set
/// and the full position list. The unique set is derived from the position
/// list instead of re-tokenizing the content (extract_tokens +
/// extract_tokens_with_positions each scan the whole content).
pub fn extract_tokens_and_positions(text: &str) -> (Vec<String>, Vec<(u32, u32)>) {
    let tok_pos = extract_tokens_with_positions(text);
    // Positions reference the unique-token list by index instead of carrying
    // an owned String per occurrence: a 30KB source file has thousands of
    // token occurrences but only hundreds of unique tokens, and the
    // per-occurrence Strings dominated indexing peak memory
    let mut ids: ahash::AHashMap<String, u32> =
        ahash::AHashMap::with_capacity(tok_pos.len() / 2 + 1);
    let mut tokens: Vec<String> = Vec::new();
    let mut positions = Vec::with_capacity(tok_pos.len());
    for (t, pos) in tok_pos {
        let id = match ids.get(t.as_str()) {
            Some(&id) => id,
            None => {
                let id = tokens.len() as u32;
                tokens.push(t.clone());
                ids.insert(t, id);
                id
            }
        };
        positions.push((id, pos));
    }
    (tokens, positions)
}

/// Extract identifiers (complete symbols) from code
#[allow(dead_code)]
pub fn extract_identifiers(content: &str) -> HashSet<String> {
    let mut identifiers = HashSet::new();
    let mut current = String::new();
    let mut in_identifier = false;

    for ch in content.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            if !in_identifier && ch.is_ascii_digit() {
                // Can't start identifier with digit
                continue;
            }
            in_identifier = true;
            current.push(ch);
        } else {
            if in_identifier && current.len() >= 2 {
                identifiers.insert(current.clone());
            }
            current.clear();
            in_identifier = false;
        }
    }

    if in_identifier && current.len() >= 2 {
        identifiers.insert(current);
    }

    identifiers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_tokens() {
        let content = "getUserById";
        let tokens = extract_tokens(content);
        assert!(tokens.contains("get"));
        assert!(tokens.contains("user"));
        assert!(tokens.contains("by"));
        assert!(tokens.contains("id"));
    }

    #[test]
    fn test_snake_case() {
        let content = "get_user_by_id";
        let tokens = extract_tokens(content);
        assert!(tokens.contains("get"));
        assert!(tokens.contains("user"));
        assert!(tokens.contains("by"));
        assert!(tokens.contains("id"));
    }

    #[test]
    fn test_extract_identifiers() {
        let content = "fn getUserById(id: u32) -> User";
        let ids = extract_identifiers(content);
        assert!(ids.contains("fn"));
        assert!(ids.contains("getUserById"));
        assert!(ids.contains("id"));
        assert!(ids.contains("u32"));
        assert!(ids.contains("User"));
    }

    #[test]
    fn test_extract_tokens_with_positions_basic() {
        let content = "struct device";
        let tokens = extract_tokens_with_positions(content);
        // "struct" at pos 0, "device" at pos 1
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0], ("struct".to_string(), 0));
        assert_eq!(tokens[1], ("device".to_string(), 1));
    }

    #[test]
    fn test_extract_tokens_with_positions_camel_case() {
        let content = "getUserById";
        let tokens = extract_tokens_with_positions(content);
        // "get" at pos 0, "user" at pos 1, "by" at pos 2, "id" at pos 3
        assert_eq!(tokens.len(), 4);
        assert_eq!(tokens[0].0, "get");
        assert_eq!(tokens[1].0, "user");
        assert_eq!(tokens[2].0, "by");
        assert_eq!(tokens[3].0, "id");
        // Positions should be sequential
        assert_eq!(tokens[0].1, 0);
        assert_eq!(tokens[1].1, 1);
        assert_eq!(tokens[2].1, 2);
        assert_eq!(tokens[3].1, 3);
    }

    #[test]
    fn test_extract_tokens_with_positions_snake_case() {
        let content = "get_user_by_id";
        let tokens = extract_tokens_with_positions(content);
        assert_eq!(tokens.len(), 4);
        assert_eq!(tokens[0].0, "get");
        assert_eq!(tokens[1].0, "user");
        assert_eq!(tokens[2].0, "by");
        assert_eq!(tokens[3].0, "id");
    }

    #[test]
    fn test_tokenize_query_matches_index() {
        // Query-time tokenization should produce the same positions as index-time
        let phrase = "struct device";
        let query_tokens = tokenize_query_with_positions(phrase);
        let index_tokens = extract_tokens_with_positions(phrase);
        assert_eq!(query_tokens, index_tokens);
    }

    #[test]
    fn test_tokenize_query_with_positions() {
        let tokens = tokenize_query_with_positions("static void");
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0], ("static".to_string(), 0));
        assert_eq!(tokens[1], ("void".to_string(), 1));
    }

    #[test]
    fn test_positions_with_short_tokens_gap() {
        // Short tokens (< 2 chars) should still increment position counter
        let content = "a big x dog";
        let tokens = extract_tokens_with_positions(content);
        // "a"(skipped, pos 0), "big"(pos 1), "x"(skipped, pos 2), "dog"(pos 3)
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0], ("big".to_string(), 1));
        assert_eq!(tokens[1], ("dog".to_string(), 3));
    }
}
