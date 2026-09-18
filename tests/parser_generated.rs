//! Deterministic parser reliability checks; also reused by the parser fuzz target.
use fxi::query::parser::{MAX_QUERY_BYTES, MAX_QUERY_DEPTH, MAX_QUERY_NODES};
use fxi::query::{QueryNode, QueryPlan, parse_query, try_parse_query};

/// Check parser agreement and independently inspect the accepted AST's bounds.
/// This tests safety and API contracts, not the meaning of search results.
pub fn assert_parser_contract(input: &str) {
    let compatibility = parse_query(input);
    match try_parse_query(input) {
        Ok(query) => {
            assert!(input.len() <= MAX_QUERY_BYTES);
            assert!(query.validate().is_ok());
            assert_eq!(format!("{query:?}"), format!("{compatibility:?}"));
            let mut stack = vec![(&query.root, 0usize)];
            let mut nodes = 0;
            let mut bytes = 0;
            while let Some((node, depth)) = stack.pop() {
                nodes += 1;
                assert!(nodes <= MAX_QUERY_NODES * 3);
                assert!(depth <= MAX_QUERY_DEPTH * 3);
                match node {
                    QueryNode::Invalid(error) => panic!("Accepted invalid node: {error}"),
                    QueryNode::And(children) | QueryNode::Or(children) => {
                        assert!(children.len() <= MAX_QUERY_NODES);
                        stack.extend(children.iter().map(|child| (child, depth + 1)));
                    }
                    QueryNode::Not(child) => stack.push((child, depth + 1)),
                    QueryNode::BoostedLiteral { text, boost }
                    | QueryNode::BoostedPhrase { text, boost } => {
                        assert!(boost.is_finite() && *boost >= 0.0);
                        bytes += text.len();
                    }
                    QueryNode::Literal(text) | QueryNode::Phrase(text) | QueryNode::Regex(text) => {
                        bytes += text.len();
                    }
                    QueryNode::Near { terms, .. } => {
                        assert!(terms.len() <= MAX_QUERY_NODES);
                        bytes += terms.iter().map(String::len).sum::<usize>();
                    }
                    QueryNode::Empty => {}
                }
            }
            assert!(bytes <= MAX_QUERY_BYTES * 4);
            assert!(QueryPlan::try_from_query(&query).is_ok());
            let mut words = query.clone();
            if words.apply_word_boundaries().is_ok() {
                assert!(words.validate().is_ok());
                assert!(QueryPlan::try_from_query(&words).is_ok());
            }
        }
        Err(error) => {
            assert!(!error.message.is_empty());
            assert!(error.offset <= input.len());
            assert!(input.is_char_boundary(error.offset));
            assert_eq!(compatibility.validate().unwrap_err(), error);
            assert_eq!(
                QueryPlan::try_from_query(&compatibility).unwrap_err(),
                error
            );
        }
    }
}

#[cfg(test)]
mod generated {
    use super::*;

    #[test]
    fn replay_parser_regressions_and_boundary_inputs() {
        let seeds = [
            "",
            " ",
            "λ",
            "Kelvin",
            "東京",
            "🦀",
            "e\u{301}",
            "\0",
            "\r\n",
            "foo-bar",
            "foo::bar",
            "https://example.test/a",
            "foo^inf",
            "foo^-NaN",
            "foo^1e999",
            "\"a\\\"b\"",
            "re:/[/",
            "re:/[()/]/",
            "re:/a\\/b/",
            "NOT",
            "foo OR",
            "(foo",
            "foo)",
            "NOT (foo OR bar)",
            "ext:rs (foo OR bar)",
            "foo OR ext:rs bar",
            "NOT ext:rs",
            "line:0-3",
            "line:3-2",
            "size:>18446744073709551615",
            "size:<0",
            "mtime:2024-02-29",
            "mtime:2023-02-29",
            "foo NEAR/2 bar",
            "top:0",
            "top:18446744073709551616",
            "\"\"",
            "re:/(?:)/",
        ];
        for input in seeds {
            assert_parser_contract(input);
        }
        for count in [31, 32, 33, 96, 1024, 10_000] {
            assert_parser_contract(&format!("{}λ{}", "(".repeat(count), ")".repeat(count)));
            assert_parser_contract(&format!("{}foo", "NOT ".repeat(count)));
        }
        for count in [1023, 1024, 1025] {
            assert_parser_contract(&vec!["λ"; count].join(" OR "));
        }
        for count in [MAX_QUERY_BYTES - 1, MAX_QUERY_BYTES, MAX_QUERY_BYTES + 1] {
            assert_parser_contract(&"x".repeat(count));
        }
    }

    #[test]
    fn generated_unicode_and_query_syntax_combinations() {
        let atoms = [
            "foo", "bar", "東京", "Kelvin", "é", "e\u{301}", "🦀", "a.b", "x-y",
        ];
        let pieces = [
            "(", ")", "NOT ", " OR ", " AND ", " ", "\"", "\\", ":", "^", "\t", "\n", "re:/", "/",
            "ext:rs ", "top:5 ",
        ];
        // Fixed seeds keep failures reproducible on every platform, without RNG dependencies.
        for seed in 0..512u64 {
            let mut state = seed + 1;
            let mut next = || {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as usize
            };
            let a = atoms[next() % atoms.len()];
            let b = atoms[next() % atoms.len()];
            for input in [
                format!("({a} OR {b}) AND NOT absent"),
                format!("ext:rs ({a} OR \"{b}\") top:5"),
                format!("\"{a} {b}\""),
                format!("re:/(?:{a}|{b})/"),
                format!("{a} NEAR/{} {b}", next() % 8),
                format!("{a}^{}", next() % 20),
            ] {
                assert_parser_contract(&input);
            }
            let mut mutated = String::new();
            for _ in 0..32 {
                if next() % 3 == 0 {
                    mutated.push_str(atoms[next() % atoms.len()]);
                } else {
                    mutated.push_str(pieces[next() % pieces.len()]);
                }
            }
            assert_parser_contract(&mutated);
        }
    }
}
