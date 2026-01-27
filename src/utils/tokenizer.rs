use std::collections::HashSet;

/// Extract tokens from source code content
/// Handles: identifiers, snake_case splits, camelCase splits, words
pub fn extract_tokens(content: &str) -> HashSet<String> {
    let mut tokens = HashSet::new();

    // State machine for tokenization
    let mut current_token = String::new();
    let mut prev_char_type = CharType::Other;

    for ch in content.chars() {
        let char_type = classify_char(ch);

        match char_type {
            CharType::Lower | CharType::Digit => {
                current_token.push(ch);
            }
            CharType::Upper => {
                // CamelCase split: if prev was lowercase, start new token
                if prev_char_type == CharType::Lower && !current_token.is_empty() {
                    add_token(&mut tokens, &current_token);
                    current_token.clear();
                }
                current_token.push(ch.to_ascii_lowercase());
            }
            CharType::Underscore => {
                // snake_case split
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

    // Don't forget the last token
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
    // Only add tokens of meaningful length
    if token.len() >= 2 {
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
