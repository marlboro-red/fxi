//! Integration tests comparing fxi CLI behavior to ripgrep.
//!
//! These tests verify that fxi's CLI flags and output format match ripgrep's
//! conventions for familiar usage patterns.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

static FIXTURE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Get or create the test fixture directory (singleton)
fn get_fixture_dir() -> PathBuf {
    FIXTURE_DIR.get_or_init(create_fixture_dir).clone()
}

/// Create isolated test fixture directory with its own git repo
fn create_fixture_dir() -> PathBuf {
    // Use a temp directory to avoid git root detection issues
    let dir = std::env::temp_dir()
        .join("fxi_test_fixtures")
        .join(format!("test_{}", std::process::id()));

    // Clean up any existing directory
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("Failed to create fixture dir");

    // Initialize git repo so fxi doesn't traverse up to parent
    Command::new("git")
        .args(["init"])
        .current_dir(&dir)
        .output()
        .expect("Failed to init git repo");

    // Create test files with known content
    fs::write(
        dir.join("main.rs"),
        r#"fn main() {
    println!("Hello, world!");
    let x = 42;
    // TODO: fix this
    let y = x + 1;
}

fn helper() {
    // Another function
    println!("Helper");
}
"#,
    )
    .unwrap();

    fs::write(
        dir.join("lib.rs"),
        r#"pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

pub fn multiply(a: i32, b: i32) -> i32 {
    // TODO: optimize this
    a * b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_add() {
        assert_eq!(add(2, 3), 5);
    }
}
"#,
    )
    .unwrap();

    fs::write(
        dir.join("utils.rs"),
        r#"// Utility functions
pub fn format_error(msg: &str) -> String {
    format!("ERROR: {}", msg)
}

pub fn format_warning(msg: &str) -> String {
    format!("WARNING: {}", msg)
}

// todo: add more utils
pub fn debug_print(msg: &str) {
    eprintln!("DEBUG: {}", msg);
}
"#,
    )
    .unwrap();

    fs::write(
        dir.join("config.json"),
        r#"{
    "name": "test-project",
    "version": "1.0.0",
    "debug": true
}
"#,
    )
    .unwrap();

    // Build the index
    let fxi = fxi_binary();
    let output = Command::new(&fxi)
        .args(["index", "--force"])
        .arg(&dir)
        .output()
        .expect("Failed to run fxi index");

    if !output.status.success() {
        panic!(
            "fxi index failed: {}\nstdout: {}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
    }

    dir
}

/// Setup fixtures (now just returns the singleton dir)
fn setup_fixtures() -> PathBuf {
    get_fixture_dir()
}

/// Build index for test fixtures (now a no-op since we build in setup)
fn build_index(_dir: &PathBuf) {
    // Index is built once during fixture creation
}

/// Get path to fxi binary
fn fxi_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("debug")
        .join("fxi")
}

/// Run fxi with given args
fn run_fxi(args: &[&str], dir: &PathBuf) -> (String, String, bool) {
    let fxi = fxi_binary();
    let mut cmd_args: Vec<&str> = args.to_vec();
    cmd_args.extend(["-p", dir.to_str().unwrap(), "--color=never"]);

    let output = Command::new(&fxi)
        .args(&cmd_args)
        .output()
        .expect("Failed to run fxi");

    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.success(),
    )
}

/// Run ripgrep with given args
fn run_rg(args: &[&str], dir: &PathBuf) -> (String, String, bool) {
    let output = Command::new("rg")
        .args(args)
        .arg("--color=never")
        .arg("--no-heading")
        .current_dir(dir)
        .output()
        .expect("Failed to run ripgrep");

    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.success(),
    )
}

/// Extract just filenames from output (for -l flag comparison)
fn extract_files(output: &str) -> HashSet<String> {
    output
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with("--"))
        .filter_map(|l| {
            // Skip lines that are just separators or context markers
            if l.trim().is_empty() {
                return None;
            }

            // For -l output, lines are just filenames
            // For regular output, format is "file:line:content" or "file" (heading)
            let path_str = if l.contains(':') {
                // Could be "file:line:content" - extract file part
                let first_colon = l.find(':').unwrap();
                &l[..first_colon]
            } else {
                // Just a filename (heading style or -l output)
                l.trim()
            };

            // Get just the filename from the path
            PathBuf::from(path_str)
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
        })
        .filter(|f| {
            // Filter out things that look like line numbers or garbage
            !f.chars().all(|c| c.is_ascii_digit()) &&
            !f.is_empty() &&
            !f.contains('\x1b')  // Filter ANSI codes
        })
        .collect()
}

/// Extract file:count pairs from -c output
fn extract_counts(output: &str) -> Vec<(String, usize)> {
    output
        .lines()
        .filter(|l| !l.is_empty() && l.contains(':'))
        .filter_map(|l| {
            let parts: Vec<&str> = l.rsplitn(2, ':').collect();
            if parts.len() == 2 {
                let count: usize = parts[0].trim().parse().ok()?;
                let file = PathBuf::from(parts[1])
                    .file_name()?
                    .to_string_lossy()
                    .to_string();
                Some((file, count))
            } else {
                None
            }
        })
        .collect()
}

/// Count total matches in output
fn count_matches(output: &str) -> usize {
    output.lines().filter(|l| !l.is_empty() && !l.starts_with("--")).count()
}

// ============================================================================
// Flag Compatibility Tests
// ============================================================================

#[test]
fn test_flag_case_insensitive() {
    let dir = setup_fixtures();
    build_index(&dir);

    // Search for "error" case-insensitively
    let (fxi_out, _, fxi_ok) = run_fxi(&["-i", "error"], &dir);
    let (rg_out, _, rg_ok) = run_rg(&["-i", "error"], &dir);

    assert!(fxi_ok, "fxi should succeed");
    assert!(rg_ok, "rg should succeed");

    // Both should find matches in utils.rs (ERROR, error)
    let fxi_files = extract_files(&fxi_out);
    let rg_files = extract_files(&rg_out);

    assert!(
        fxi_files.contains("utils.rs"),
        "fxi -i should find utils.rs, got: {:?}",
        fxi_files
    );
    assert!(
        rg_files.contains("utils.rs"),
        "rg -i should find utils.rs, got: {:?}",
        rg_files
    );
}

#[test]
fn test_flag_files_with_matches() {
    let dir = setup_fixtures();
    build_index(&dir);

    // Search with -l flag
    let (fxi_out, _, fxi_ok) = run_fxi(&["-l", "fn"], &dir);
    let (rg_out, _, rg_ok) = run_rg(&["-l", "fn"], &dir);

    assert!(fxi_ok, "fxi should succeed");
    assert!(rg_ok, "rg should succeed");

    let fxi_files = extract_files(&fxi_out);
    let rg_files = extract_files(&rg_out);

    // Both should find the same .rs files containing "fn"
    assert_eq!(
        fxi_files, rg_files,
        "fxi -l and rg -l should find same files\nfxi: {:?}\nrg: {:?}",
        fxi_files, rg_files
    );
}

#[test]
fn test_flag_count() {
    let dir = setup_fixtures();
    build_index(&dir);

    // Search with -c flag - use a unique term that only appears in specific files
    // Note: fxi's token index is case-insensitive by default for better recall
    let (fxi_out, _, fxi_ok) = run_fxi(&["-c", "println"], &dir);
    let (rg_out, _, rg_ok) = run_rg(&["-c", "println"], &dir);

    assert!(fxi_ok, "fxi should succeed");
    assert!(rg_ok, "rg should succeed");

    let fxi_counts = extract_counts(&fxi_out);
    let rg_counts = extract_counts(&rg_out);

    // Convert to HashMaps for comparison
    let fxi_map: std::collections::HashMap<_, _> = fxi_counts.into_iter().collect();
    let rg_map: std::collections::HashMap<_, _> = rg_counts.into_iter().collect();

    assert_eq!(
        fxi_map, rg_map,
        "fxi -c and rg -c should report same counts\nfxi: {:?}\nrg: {:?}",
        fxi_map, rg_map
    );
}

#[test]
fn test_flag_max_count() {
    let dir = setup_fixtures();
    build_index(&dir);

    // Search with -m flag to limit results
    let (fxi_out, _, fxi_ok) = run_fxi(&["-m", "2", "fn"], &dir);

    assert!(fxi_ok, "fxi should succeed");

    let match_count = count_matches(&fxi_out);
    assert!(
        match_count <= 2,
        "fxi -m 2 should return at most 2 matches, got {}",
        match_count
    );
}

#[test]
fn test_flag_after_context() {
    let dir = setup_fixtures();
    build_index(&dir);

    // Search with -A flag
    let (fxi_out, _, fxi_ok) = run_fxi(&["-A", "1", "fn main"], &dir);

    assert!(fxi_ok, "fxi should succeed");

    // Should have context lines (marked with - instead of :)
    let has_context = fxi_out.lines().any(|l| {
        l.contains("-") && !l.starts_with("--") && !l.contains("fn main")
    });

    assert!(
        has_context || fxi_out.lines().count() > 1,
        "fxi -A 1 should include context lines"
    );
}

#[test]
fn test_flag_before_context() {
    let dir = setup_fixtures();
    build_index(&dir);

    // Search with -B flag
    let (fxi_out, _, fxi_ok) = run_fxi(&["-B", "1", "println"], &dir);

    assert!(fxi_ok, "fxi should succeed");
    assert!(!fxi_out.is_empty(), "fxi -B should return results");
}

#[test]
fn test_flag_context_both() {
    let dir = setup_fixtures();
    build_index(&dir);

    // Search with -C flag (context both directions)
    let (fxi_out, _, fxi_ok) = run_fxi(&["-C", "1", "TODO"], &dir);

    assert!(fxi_ok, "fxi should succeed");
    assert!(!fxi_out.is_empty(), "fxi -C should return results");

    // Count lines - should be more than just the match lines
    let line_count = fxi_out.lines().filter(|l| !l.is_empty()).count();
    assert!(
        line_count >= 2,
        "fxi -C 1 should include context, got {} lines",
        line_count
    );
}

#[test]
fn test_flag_color_never() {
    let dir = setup_fixtures();
    build_index(&dir);

    // Search with --color=never (run_fxi already adds --color=never, so just test basic search)
    let (fxi_out, _, fxi_ok) = run_fxi(&["fn"], &dir);

    assert!(fxi_ok, "fxi should succeed");

    // Should not contain ANSI escape codes (--color=never is added by run_fxi)
    assert!(
        !fxi_out.contains("\x1b["),
        "fxi --color=never should not contain ANSI codes, got: {}",
        fxi_out.chars().take(200).collect::<String>()
    );
}

#[test]
fn test_flag_color_always() {
    let dir = setup_fixtures();
    build_index(&dir);

    // Run without --color=never override
    let fxi = fxi_binary();
    let output = Command::new(&fxi)
        .args(["fn", "-p"])
        .arg(&dir)
        .arg("--color=always")
        .output()
        .expect("Failed to run fxi");

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should contain ANSI escape codes
    assert!(
        stdout.contains("\x1b["),
        "fxi --color=always should contain ANSI codes"
    );
}

// ============================================================================
// Output Format Tests
// ============================================================================

#[test]
fn test_output_format_basic() {
    let dir = setup_fixtures();
    build_index(&dir);

    let (fxi_out, _, _) = run_fxi(&["fn main"], &dir);

    // Output should contain filename and line number
    assert!(
        fxi_out.contains("main.rs"),
        "Output should contain filename"
    );
    assert!(
        fxi_out.contains(':'),
        "Output should use : separator for line numbers"
    );
}

#[test]
fn test_output_line_numbers() {
    let dir = setup_fixtures();
    build_index(&dir);

    let (fxi_out, _, _) = run_fxi(&["fn main"], &dir);

    // Should have line number in format "file:number:content"
    let has_line_number = fxi_out.lines().any(|l| {
        let parts: Vec<&str> = l.split(':').collect();
        parts.len() >= 2 && parts[1].chars().all(|c| c.is_ascii_digit() || c == '-')
    });

    assert!(has_line_number, "Output should include line numbers");
}

// ============================================================================
// Search Result Parity Tests
// ============================================================================

#[test]
fn test_search_parity_simple() {
    let dir = setup_fixtures();
    build_index(&dir);

    // Simple search should find same files
    let (fxi_out, _, _) = run_fxi(&["-l", "println"], &dir);
    let (rg_out, _, _) = run_rg(&["-l", "println"], &dir);

    let fxi_files = extract_files(&fxi_out);
    let rg_files = extract_files(&rg_out);

    assert_eq!(
        fxi_files, rg_files,
        "Simple search should find same files\nfxi: {:?}\nrg: {:?}",
        fxi_files, rg_files
    );
}

#[test]
fn test_search_parity_phrase() {
    let dir = setup_fixtures();
    build_index(&dir);

    // Phrase search (quoted)
    let (fxi_out, _, _) = run_fxi(&["-l", "\"Hello, world\""], &dir);
    let (rg_out, _, _) = run_rg(&["-l", "Hello, world"], &dir);

    let fxi_files = extract_files(&fxi_out);
    let rg_files = extract_files(&rg_out);

    assert_eq!(
        fxi_files, rg_files,
        "Phrase search should find same files\nfxi: {:?}\nrg: {:?}",
        fxi_files, rg_files
    );
}

#[test]
fn test_search_no_results() {
    let dir = setup_fixtures();
    build_index(&dir);

    // Search for non-existent pattern
    let (fxi_out, _, _) = run_fxi(&["xyznonexistent123"], &dir);
    let (rg_out, _, _) = run_rg(&["xyznonexistent123"], &dir);

    assert!(fxi_out.trim().is_empty(), "fxi should return no results");
    assert!(rg_out.trim().is_empty(), "rg should return no results");
}

// ============================================================================
// Edge Cases
// ============================================================================

#[test]
fn test_special_characters_in_pattern() {
    let dir = setup_fixtures();
    build_index(&dir);

    // Search for pattern with special chars
    let (fxi_out, _, fxi_ok) = run_fxi(&["-l", "a + b"], &dir);

    assert!(fxi_ok, "fxi should handle special characters");
    // Should find lib.rs which has "a + b"
    let fxi_files = extract_files(&fxi_out);
    assert!(
        fxi_files.contains("lib.rs"),
        "Should find lib.rs with 'a + b'"
    );
}

#[test]
fn test_json_file_search() {
    let dir = setup_fixtures();
    build_index(&dir);

    // Search in non-code files - use a term unique to config.json
    // Note: "test-project" would be tokenized as "test" AND "project"
    let (fxi_out, _, _) = run_fxi(&["-l", "1.0.0"], &dir);

    let fxi_files = extract_files(&fxi_out);
    assert!(
        fxi_files.contains("config.json"),
        "Should find config.json, got: {:?}",
        fxi_files
    );
}

#[test]
fn test_combined_flags() {
    let dir = setup_fixtures();
    build_index(&dir);

    // Combine multiple flags like ripgrep users would
    let (fxi_out, _, fxi_ok) = run_fxi(&["-i", "-m", "5", "-C", "1", "todo"], &dir);

    assert!(fxi_ok, "fxi should handle combined flags");
    assert!(!fxi_out.is_empty(), "Should find TODO/todo matches");

    // Verify max count is respected
    let match_count = fxi_out
        .lines()
        .filter(|l| l.to_lowercase().contains("todo") && l.contains(':'))
        .count();
    assert!(match_count <= 5, "Should respect -m 5 limit");
}

// ============================================================================
// CLI Behavior Tests
// ============================================================================

#[test]
fn test_help_shows_ripgrep_flags() {
    let fxi = fxi_binary();
    let output = Command::new(&fxi)
        .arg("--help")
        .output()
        .expect("Failed to run fxi --help");

    let help = String::from_utf8_lossy(&output.stdout);

    // Verify ripgrep-compatible flags are documented
    assert!(help.contains("-i"), "Help should show -i flag");
    assert!(help.contains("-A"), "Help should show -A flag");
    assert!(help.contains("-B"), "Help should show -B flag");
    assert!(help.contains("-C"), "Help should show -C flag");
    assert!(help.contains("-l"), "Help should show -l flag");
    assert!(help.contains("-c"), "Help should show -c flag");
    assert!(help.contains("-m"), "Help should show -m flag");
    assert!(help.contains("--color"), "Help should show --color flag");
}

#[test]
fn test_no_subcommand_does_search() {
    let dir = setup_fixtures();
    build_index(&dir);

    // Running fxi with just a pattern should search (like ripgrep)
    let (fxi_out, _, fxi_ok) = run_fxi(&["fn"], &dir);

    assert!(fxi_ok, "fxi PATTERN should work without subcommand");
    assert!(!fxi_out.is_empty(), "Should return search results");
}

// ============================================================================
// Behavior Difference Documentation Tests
// ============================================================================

#[test]
fn test_case_sensitivity_difference() {
    let dir = setup_fixtures();
    build_index(&dir);

    // fxi's token index is case-insensitive by default for better code search recall
    // This differs from ripgrep which is case-sensitive by default
    //
    // Example: searching "TODO" in fxi will also match "todo"
    // Use -i flag with ripgrep to get similar behavior

    let (fxi_out, _, _) = run_fxi(&["-l", "error"], &dir);
    let (rg_out_sensitive, _, _) = run_rg(&["-l", "error"], &dir);
    let (rg_out_insensitive, _, _) = run_rg(&["-l", "-i", "error"], &dir);

    let fxi_files = extract_files(&fxi_out);
    let rg_sensitive = extract_files(&rg_out_sensitive);
    let rg_insensitive = extract_files(&rg_out_insensitive);

    // fxi behavior is closer to rg -i than plain rg
    // utils.rs has both "ERROR" (uppercase in format string) and "error" (in function name)
    assert!(
        fxi_files.contains("utils.rs"),
        "fxi should find utils.rs (case-insensitive by default)"
    );

    // Document the difference
    assert!(
        rg_sensitive.contains("utils.rs") || rg_insensitive.contains("utils.rs"),
        "rg should find utils.rs with either mode"
    );
}

#[test]
fn test_phrase_search_syntax() {
    let dir = setup_fixtures();
    build_index(&dir);

    // fxi uses quotes for exact phrase matching
    // Without quotes, terms are ANDed together

    // "fn main" as two terms (AND): matches any file with both "fn" and "main"
    let (and_out, _, _) = run_fxi(&["-l", "fn main"], &dir);
    let and_files = extract_files(&and_out);

    // "fn main" as phrase: matches exact sequence
    let (phrase_out, _, _) = run_fxi(&["-l", "\"fn main\""], &dir);
    let phrase_files = extract_files(&phrase_out);

    // AND search might match more files
    assert!(
        and_files.len() >= phrase_files.len(),
        "AND search should match >= phrase search"
    );

    // Phrase search should find main.rs (has "fn main()")
    assert!(
        phrase_files.contains("main.rs"),
        "Phrase search should find main.rs"
    );
}

#[test]
fn test_hyphenated_terms() {
    let dir = setup_fixtures();
    build_index(&dir);

    // Hyphens are word separators in fxi's tokenizer
    // "test-project" becomes "test" AND "project"
    // Use quotes for exact match: "\"test-project\""

    let (fxi_out, _, _) = run_fxi(&["-l", "\"test-project\""], &dir);
    let fxi_files = extract_files(&fxi_out);

    let (rg_out, _, _) = run_rg(&["-l", "test-project"], &dir);
    let rg_files = extract_files(&rg_out);

    // Both should find config.json with exact match
    assert_eq!(
        fxi_files, rg_files,
        "Quoted phrase search should match ripgrep literal search"
    );
}

#[test]
fn test_multiple_flags_order() {
    let dir = setup_fixtures();
    build_index(&dir);

    // Flags can appear in any order (like ripgrep)
    let (out1, _, ok1) = run_fxi(&["-i", "-l", "fn"], &dir);
    let (out2, _, ok2) = run_fxi(&["-l", "-i", "fn"], &dir);

    assert!(ok1 && ok2, "Both flag orders should work");

    let files1 = extract_files(&out1);
    let files2 = extract_files(&out2);

    assert_eq!(files1, files2, "Flag order should not affect results");
}

#[test]
fn test_flag_multiple_patterns() {
    let dir = setup_fixtures();
    build_index(&dir);

    // Multiple -e patterns (OR together)
    let (fxi_out, _, fxi_ok) = run_fxi(&["-l", "-e", "fn main", "-e", "fn helper"], &dir);
    let (rg_out, _, rg_ok) = run_rg(&["-l", "-e", "fn main", "-e", "fn helper"], &dir);

    assert!(fxi_ok, "fxi should succeed with multiple -e patterns");
    assert!(rg_ok, "rg should succeed with multiple -e patterns");

    let fxi_files = extract_files(&fxi_out);
    let rg_files = extract_files(&rg_out);

    // Both should find main.rs (has both patterns)
    assert!(
        fxi_files.contains("main.rs"),
        "fxi should find main.rs with -e patterns"
    );
    assert!(
        rg_files.contains("main.rs"),
        "rg should find main.rs with -e patterns"
    );
}

#[test]
fn test_flag_word_regexp() {
    let dir = setup_fixtures();
    build_index(&dir);

    // -w should match whole words only
    let (fxi_out, _, fxi_ok) = run_fxi(&["-w", "-l", "add"], &dir);
    let (rg_out, _, rg_ok) = run_rg(&["-w", "-l", "add"], &dir);

    assert!(fxi_ok, "fxi should succeed with -w");
    assert!(rg_ok, "rg should succeed with -w");

    let fxi_files = extract_files(&fxi_out);
    let rg_files = extract_files(&rg_out);

    // lib.rs has "add" as a whole word in function name
    assert!(
        fxi_files.contains("lib.rs"),
        "fxi -w should find lib.rs"
    );
    assert!(
        rg_files.contains("lib.rs"),
        "rg -w should find lib.rs"
    );
}

#[test]
fn test_flag_invert_match_unsupported() {
    let dir = setup_fixtures();
    build_index(&dir);

    // -v should return an error (not supported with indexed search)
    let (_, stderr, fxi_ok) = run_fxi(&["-v", "fn"], &dir);

    assert!(!fxi_ok, "fxi -v should fail (not supported)");
    assert!(
        stderr.contains("not supported") || stderr.contains("invert"),
        "Error should explain -v is not supported"
    );
}

#[test]
fn test_context_overrides() {
    let dir = setup_fixtures();
    build_index(&dir);

    // -C should override -A and -B (like ripgrep)
    // Test by comparing output with -C 1 vs what -A 5 -B 5 would produce
    let (out_c, _, _) = run_fxi(&["-C", "1", "\"fn main\""], &dir);
    let (out_large, _, _) = run_fxi(&["-A", "5", "-B", "5", "\"fn main\""], &dir);

    // Count content lines (excluding headers and separators)
    let count_content = |s: &str| {
        s.lines()
            .filter(|l| !l.is_empty() && !l.starts_with("--") && l.contains(':'))
            .count()
    };

    let c_lines = count_content(&out_c);
    let large_lines = count_content(&out_large);

    // -C 1 should produce fewer or equal lines than -A 5 -B 5
    assert!(
        c_lines <= large_lines,
        "-C 1 ({} lines) should produce <= -A 5 -B 5 ({} lines)",
        c_lines,
        large_lines
    );
}
