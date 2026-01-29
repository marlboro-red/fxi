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
}
