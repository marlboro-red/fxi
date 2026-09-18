//! Deterministic executable differential tests. The oracle scans every source line;
//! it never calls FXI's parser, planner, index reader, verifier, or formatter.
//! Regex cases deliberately share only the public `regex` language implementation.
//! Replay: FXI_CLI_SEED=123 FXI_CLI_CASE=17 cargo test --test cli_generated -- --nocapture
use regex::RegexBuilder;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

struct Corpus {
    dir: tempfile::TempDir,
    root: PathBuf,
    files: BTreeMap<PathBuf, Vec<String>>,
}
impl Corpus {
    fn new(seed: u64, profile: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(root.join(".git")).unwrap();
        let vocabulary = [
            "alpha",
            "Alpha",
            "ALPHA",
            "alphabet",
            "_alpha",
            "beta",
            "foo/bar",
            "foo.bar",
            "café",
            "🦀 alpha",
            "neutral",
            "alpha beta",
            "",
            "βalpha",
            "alpha-alpha",
        ];
        let mut state = seed;
        let mut files = BTreeMap::new();
        #[cfg(unix)]
        let unusual_path = "odd\nname.txt";
        #[cfg(not(unix))]
        let unusual_path = "odd name.txt";
        for name in [
            "a.txt",
            "src/a space.txt",
            "src/deep/é.txt",
            "src-other/b.txt",
            "z.txt",
            unusual_path,
        ] {
            let mut lines = vec!["prefix".to_owned()];
            for _ in 0..24 {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                lines.push(vocabulary[(state >> 32) as usize % vocabulary.len()].to_owned());
            }
            // Guaranteed overlaps, Unicode byte offsets, word boundaries and absence.
            lines.extend(
                [
                    "🦀 alpha beta",
                    "alphabet _alpha βalpha",
                    "foo/bar foo.bar",
                    "suffix",
                ]
                .map(str::to_owned),
            );
            // PathBuf::from preserves '/' in its backing string on Windows,
            // whereas filesystem discovery produces native '\\' separators.
            // Build native paths so JSON/text/NUL expectations test the actual
            // platform representation without normalizing the observed output.
            let path: PathBuf = Path::new(name).components().collect();
            fs::create_dir_all(root.join(&path).parent().unwrap()).unwrap();
            let separator = if name == "z.txt" { "\r\n" } else { "\n" };
            // Include a source without a final newline.
            let mut text = lines.join(separator);
            if name != "a.txt" {
                text.push_str(separator);
            }
            fs::write(root.join(&path), text).unwrap();
            files.insert(path, lines);
        }
        let corpus = Self { dir, root, files };
        corpus.run(&[
            "index".into(),
            "--force".into(),
            "--profile".into(),
            profile.into(),
        ]);
        corpus
    }
    fn run(&self, args: &[String]) -> Vec<u8> {
        let output = Command::new(env!("CARGO_BIN_EXE_fxi"))
            .current_dir(&self.root)
            .env("FXI_APP_DATA", self.dir.path().join("app-data"))
            .env("FXI_INDEXES", self.dir.path().join("indexes"))
            .env("FXI_SOCKET", self.dir.path().join("isolated.sock"))
            .env("NO_COLOR", "1")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "args={args:?}\nstderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }
    fn json(&self, args: &[String], extra: &[&str]) -> Value {
        let mut args = args.to_vec();
        args.extend(extra.iter().map(|s| s.to_string()));
        args.push("--json".into());
        serde_json::from_slice(&self.run(&args)).unwrap()
    }
}

#[test]
fn generated_cli_modes_agree_with_exhaustive_source_scan() {
    generated_modes("full");
}

#[test]
fn lean_cli_modes_agree_with_exhaustive_source_scan() {
    generated_modes("lean");
}

fn generated_modes(profile: &str) {
    let seed = std::env::var("FXI_CLI_SEED")
        .map(|s| s.parse().expect("FXI_CLI_SEED must be u64"))
        .unwrap_or(0x5eed_fa17);
    let replay = std::env::var("FXI_CLI_CASE")
        .ok()
        .map(|s| s.parse::<usize>().expect("FXI_CLI_CASE must be 0..127"));
    assert!(replay.is_none_or(|n| n < 128));
    let corpus = Corpus::new(seed, profile);
    for case in 0..128 {
        if replay.is_some_and(|n| n != case) {
            continue;
        }
        let regex_mode = case & 1 != 0;
        let insensitive = case & 2 != 0;
        let word = case & 4 != 0;
        let multiple = case & 8 != 0;
        let scopes = [".", "src", "src/deep", "src/a space.txt"];
        let scope = scopes[(case >> 4) & 3];
        let primary = if case % 13 == 0 {
            "never_present"
        } else if case & 64 == 0 {
            "alpha"
        } else if regex_mode {
            "foo[/.]bar|café"
        } else {
            "foo/bar"
        };
        let patterns = if multiple {
            vec![primary, "beta", "never_present"]
        } else {
            vec![primary]
        };
        let limit = [0, 1, 7][(case / 3) % 3];
        let mut before = (case / 5) % 3;
        let mut after = (case / 7) % 3;
        let mut args = vec![if regex_mode { "--regex" } else { "-F" }.to_owned()];
        if insensitive {
            args.push("-i".into());
        }
        if word {
            args.push("-w".into());
        }
        for pattern in &patterns {
            args.extend(["-e".into(), pattern.to_string()]);
        }
        // Exercise the explicit-pattern positional scope, including spaces.
        args.extend([
            scope.into(),
            "-m".into(),
            limit.to_string(),
            "-B".into(),
            before.to_string(),
            "-A".into(),
            after.to_string(),
        ]);
        if case % 11 == 0 {
            // Explicit -C overrides -A/-B, including -C0.
            before = (case / 11) % 3;
            after = before;
            args.extend(["-C".into(), before.to_string()]);
        }
        let matchers: Vec<_> = patterns
            .iter()
            .map(|p| {
                let p = if regex_mode {
                    p.to_string()
                } else {
                    regex::escape(p)
                };
                let p = if word { format!(r"\b(?:{p})\b") } else { p };
                RegexBuilder::new(&p)
                    .case_insensitive(insensitive)
                    .build()
                    .unwrap()
            })
            .collect();
        let mut rows = Vec::new();
        let mut matching_files = Vec::new();
        for (path, lines) in &corpus.files {
            if scope != "." && !path.starts_with(Path::new(scope)) {
                continue;
            }
            let initial_len = rows.len();
            for (line, text) in lines.iter().enumerate() {
                if let Some((start, end)) = matchers
                    .iter()
                    .enumerate()
                    .filter_map(|(index, r)| {
                        if !regex_mode && !insensitive && !word {
                            // Plain literals use an independent substring oracle.
                            text.find(patterns[index])
                                .map(|start| (start, start + patterns[index].len()))
                        } else {
                            r.find(text).map(|m| (m.start(), m.end()))
                        }
                    })
                    .min()
                {
                    let context = |from: usize, to: usize| -> Vec<Value> {
                        (from..to).map(|n| json!([n + 1, lines[n]])).collect()
                    };
                    rows.push(json!({"path":path,"line_number":line+1,"line_content":text,"match_start":start,"match_end":end,
                        "context_before":context(line.saturating_sub(before),line),"context_after":context(line+1,(line+1+after).min(lines.len()))}));
                }
            }
            if rows.len() != initial_len {
                matching_files.push(path.clone());
            }
        }
        if limit > 0 {
            rows.truncate(limit);
            matching_files.truncate(limit);
        }
        let mut counts = BTreeMap::<PathBuf, usize>::new();
        for row in &rows {
            *counts
                .entry(PathBuf::from(row["path"].as_str().unwrap()))
                .or_default() += 1;
        }
        let expected_counts: Vec<_> = counts.iter().map(|(p, n)| json!([p, n])).collect();
        let description = format!(
            "seed={seed} case={case} args={args:?}; replay with FXI_CLI_SEED={seed} FXI_CLI_CASE={case}; source={:?}",
            corpus.files
        );
        let actual = corpus.json(&args, &[]);
        assert_eq!(actual["matches"], json!(rows), "content {description}");
        assert_eq!(
            actual["files_with_matches"],
            counts.len(),
            "file count {description}"
        );
        assert_eq!(
            corpus.json(&args, &["-c"])["file_counts"],
            json!(expected_counts),
            "counts {description}"
        );
        assert_eq!(
            corpus.json(&args, &["-l"])["file_paths"],
            json!(matching_files),
            "files {description}"
        );
        let mut nul_args = args.clone();
        nul_args.extend(["-l".into(), "-0".into()]);
        let expected_nul: Vec<_> = matching_files
            .iter()
            .flat_map(|p| p.to_str().unwrap().bytes().chain(std::iter::once(0)))
            .collect();
        assert_eq!(corpus.run(&nul_args), expected_nul, "NUL {description}");
        // Independently union source line intervals, promoting hits over context.
        let mut displayed = BTreeMap::<PathBuf, BTreeMap<usize, (bool, String)>>::new();
        for row in &rows {
            let path = PathBuf::from(row["path"].as_str().unwrap());
            let lines = &corpus.files[&path];
            let line = row["line_number"].as_u64().unwrap() as usize - 1;
            let file_rows = displayed.entry(path).or_default();
            for (n, text) in lines
                .iter()
                .enumerate()
                .take((line + after + 1).min(lines.len()))
                .skip(line.saturating_sub(before))
            {
                file_rows
                    .entry(n)
                    .and_modify(|r| r.0 |= n == line)
                    .or_insert((n == line, text.clone()));
            }
        }
        let mut expected_text = String::new();
        for (path, lines) in displayed {
            let mut previous = None;
            for (line, (hit, text)) in lines {
                if before + after > 0 && previous.is_some_and(|p| line > p + 1) {
                    expected_text.push_str("--\n");
                }
                let delimiter = if hit { ':' } else { '-' };
                expected_text.push_str(&format!(
                    "{}{delimiter}{}{delimiter}{text}\n",
                    path.display(),
                    line + 1
                ));
                previous = Some(line);
            }
        }
        assert_eq!(
            String::from_utf8(corpus.run(&args)).unwrap(),
            expected_text,
            "text {description}"
        );
        if case % 4 == 0 {
            let mut count_args = args.clone();
            count_args.push("-c".into());
            let expected = counts
                .iter()
                .map(|(path, n)| format!("{}:{n}\n", path.display()))
                .collect::<String>();
            assert_eq!(
                String::from_utf8(corpus.run(&count_args)).unwrap(),
                expected,
                "text counts {description}"
            );
        }
    }
}
