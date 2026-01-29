use ahash::AHashSet;

/// Extract tokens from source code content
/// Handles: identifiers, snake_case splits, camelCase splits, words
/// Optimized for speed with byte-level processing where possible.
pub fn extract_tokens(content: &str) -> Vec<String> {
    let bytes = content.as_bytes();

    // For small files, use simple approach
    if bytes.len() < 256 {
        return extract_tokens_simple(content);
    }

    // Pre-allocate with reasonable capacity - use AHashSet of hashes for speed
    let mut seen: AHashSet<u64> = AHashSet::with_capacity(bytes.len() / 64);
    let mut tokens: Vec<String> = Vec::with_capacity(bytes.len() / 64);

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
                    if slice.len() >= 3 {
                        // Hash the lowercase bytes directly
                        let hash = hash_lowercase_slice(slice);
                        if seen.insert(hash) {
                            // Only allocate string if new token
                            let mut s = String::with_capacity(slice.len());
                            for &b in slice {
                                s.push((b | 0x20) as char);
                            }
                            tokens.push(s);
                        }
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
                if slice.len() >= 3 {
                    let hash = hash_lowercase_slice(slice);
                    if seen.insert(hash) {
                        let mut s = String::with_capacity(slice.len());
                        for &b in slice {
                            s.push((b | 0x20) as char);
                        }
                        tokens.push(s);
                    }
                }
            }
            token_start = None;
            prev_was_lower = false;
        }
    }

    // Handle last token
    if let Some(start) = token_start {
        let slice = &bytes[start..];
        if slice.len() >= 3 && slice.iter().all(|&b| b < 128) {
            let hash = hash_lowercase_slice(slice);
            if seen.insert(hash) {
                let mut s = String::with_capacity(slice.len());
                for &b in slice {
                    s.push((b | 0x20) as char);
                }
                tokens.push(s);
            }
        }
    }

    tokens
}

/// Fast hash of lowercase ASCII slice using FNV-1a
#[inline]
fn hash_lowercase_slice(slice: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325; // FNV offset basis
    for &b in slice {
        hash ^= (b | 0x20) as u64; // lowercase
        hash = hash.wrapping_mul(0x100000001b3); // FNV prime
    }
    hash
}

/// Simple tokenization for small content (original algorithm)
fn extract_tokens_simple(content: &str) -> Vec<String> {
    let mut tokens: AHashSet<String> = AHashSet::new();
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

    tokens.into_iter().collect()
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

fn add_token(tokens: &mut AHashSet<String>, token: &str) {
    // Only add tokens of meaningful length
    if token.len() >= 2 {
        tokens.insert(token.to_lowercase());
    }
}

/// Extract identifiers (complete symbols) from code
#[allow(dead_code)]
pub fn extract_identifiers(content: &str) -> AHashSet<String> {
    let mut identifiers = AHashSet::new();
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
        assert!(tokens.iter().any(|t| t == "get"));
        assert!(tokens.iter().any(|t| t == "user"));
        assert!(tokens.iter().any(|t| t == "by"));
        assert!(tokens.iter().any(|t| t == "id"));
    }

    #[test]
    fn test_snake_case() {
        let content = "get_user_by_id";
        let tokens = extract_tokens(content);
        assert!(tokens.iter().any(|t| t == "get"));
        assert!(tokens.iter().any(|t| t == "user"));
        assert!(tokens.iter().any(|t| t == "by"));
        assert!(tokens.iter().any(|t| t == "id"));
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
