use fxi::index::{
    build::{build_index_with_options, build_index_with_profile, update_index},
    compact::merge_segments,
    reader::IndexReader,
    types::IndexProfile,
};
use fxi::query::{QueryExecutor, parse_query};
use std::{fs, path::Path};

fn assert_lean(root: &Path) {
    let reader = IndexReader::open(root).unwrap();
    assert_eq!(reader.meta.profile, IndexProfile::Lean);
    assert!(!reader.meta.has_positions);
    assert_eq!(reader.meta.version, 3);
    assert!(
        reader
            .get_token_docs("needle")
            .unwrap_err()
            .to_string()
            .contains("--profile full")
    );
    assert!(reader.get_token_docs_containing("need").is_err());
    assert!(
        reader
            .resolve_phrase_positional(&[("needle".into(), 0), ("alpha".into(), 1)], None)
            .is_err()
    );
    assert!(reader.get_line_map(1).unwrap().is_none());
    for segment in fs::read_dir(fxi::utils::get_index_dir(root).unwrap().join("segments")).unwrap()
    {
        for file in [
            "tokens.dict",
            "tokens.postings",
            "tokens.positions",
            "linemap.bin",
        ] {
            assert!(
                !segment.as_ref().unwrap().path().join(file).exists(),
                "unexpected {file}"
            );
        }
    }
    let actual = QueryExecutor::new(&reader)
        .execute_files_only(&parse_query("needle"), 0)
        .unwrap();
    let mut expected: Vec<std::path::PathBuf> = fs::read_dir(root)
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            path.is_file()
                .then(|| {
                    fs::read_to_string(&path)
                        .unwrap()
                        .contains("needle")
                        .then(|| path.file_name().unwrap().into())
                })
                .flatten()
        })
        .collect();
    expected.sort();
    assert_eq!(actual, expected);
}

#[test]
fn lean_profile_survives_updates_compaction_rebuild_and_explicit_conversion() {
    fxi::utils::app_data::isolate_test_storage().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join(".git")).unwrap();
    for id in 0..40 {
        fs::write(root.join(format!("{id}.txt")), "needle alpha\n").unwrap();
    }
    build_index_with_profile(root, true, true, Some(10), IndexProfile::Lean).unwrap();
    assert_lean(root);
    fs::write(root.join("0.txt"), "changed content\n").unwrap();
    fs::write(root.join("new.txt"), "needle beta\n").unwrap();
    fs::remove_file(root.join("1.txt")).unwrap();
    update_index(root).unwrap();
    assert_lean(root);
    merge_segments(root).unwrap();
    assert_lean(root);
    build_index_with_options(root, true, true, Some(10)).unwrap();
    assert_lean(root);
    build_index_with_profile(root, true, true, Some(10), IndexProfile::Full).unwrap();
    let reader = IndexReader::open(root).unwrap();
    assert_eq!(reader.meta.profile, IndexProfile::Full);
    assert_eq!(reader.get_token_docs("needle").unwrap().len(), 39);
    drop(reader);
    fxi::utils::remove_index(root).unwrap();
}

#[test]
fn legacy_metadata_requires_tokens_and_unknown_profiles_are_rejected() {
    fxi::utils::app_data::isolate_test_storage().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::write(root.join("a.txt"), "needle alpha\n").unwrap();
    build_index_with_options(root, true, true, None).unwrap();
    let index = fxi::utils::get_index_dir(root).unwrap();
    let path = index.join("meta.json");
    let mut meta: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    meta.as_object_mut().unwrap().remove("profile");
    fs::write(&path, serde_json::to_vec(&meta).unwrap()).unwrap();
    let reader = IndexReader::open(root).unwrap();
    assert_eq!(reader.meta.profile, IndexProfile::Full);
    assert_eq!(reader.get_token_docs("needle").unwrap().len(), 1);
    drop(reader);
    let segment = fs::read_dir(index.join("segments"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::remove_file(segment.join("tokens.dict")).unwrap();
    assert!(IndexReader::open(root).is_err());
    meta["profile"] = "unrecognized".into();
    fs::write(path, serde_json::to_vec(&meta).unwrap()).unwrap();
    assert!(IndexReader::open(root).is_err());
    fxi::utils::remove_index(root).unwrap();
}

#[test]
fn lean_profile_still_requires_gram_evidence() {
    fxi::utils::app_data::isolate_test_storage().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::write(root.join("a.txt"), "needle alpha\n").unwrap();
    build_index_with_profile(root, true, true, None, IndexProfile::Lean).unwrap();
    let index = fxi::utils::get_index_dir(root).unwrap();
    let segment = fs::read_dir(index.join("segments"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::remove_file(segment.join("grams.postings")).unwrap();
    assert!(IndexReader::open(root).is_err());
    fxi::utils::remove_index(root).unwrap();
}

#[test]
fn profile_and_format_version_must_agree() {
    fxi::utils::app_data::isolate_test_storage().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::write(root.join("a.txt"), "needle alpha\n").unwrap();
    build_index_with_profile(root, true, true, None, IndexProfile::Lean).unwrap();
    let index = fxi::utils::get_index_dir(root).unwrap();
    let path = index.join("meta.json");
    let original: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for (version, profile, positions) in [(2, "lean", false), (3, "full", false), (3, "lean", true)]
    {
        let mut meta = original.clone();
        meta["version"] = version.into();
        meta["profile"] = profile.into();
        meta["has_positions"] = positions.into();
        fs::write(&path, serde_json::to_vec(&meta).unwrap()).unwrap();
        assert!(IndexReader::open(root).is_err());
    }
    fxi::utils::remove_index(root).unwrap();
}
