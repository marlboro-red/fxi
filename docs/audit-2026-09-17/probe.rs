use fxi::{
    index::{
        build::{build_index_with_progress, update_index},
        reader::IndexReader,
    },
    query::{QueryExecutor, parse_query},
    utils::app_data::{get_index_dir, remove_index},
};
use std::fs;
#[test]
fn audit_probe() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    for (p, c) in [
        ("a.txt", "x\nPREFIXNEEDLESUFFIX\nfoobar bazqux\nK foo\n"),
        ("b.txt", "needle\n"),
        ("c.txt", "something different\n"),
        ("needle.md", "nothing\n"),
    ] {
        fs::write(root.join(p), c).unwrap();
    }
    build_index_with_progress(root, true, true).unwrap();
    let r = IndexReader::open(root).unwrap();
    let ex = QueryExecutor::new(&r);
    for q in [
        "needle | x",
        "need",
        "oo",
        "\"bar baz\"",
        "re:/^needle$/",
        "needle line:20-30",
        "ext:rs needle",
        "re:/[/",
        "foo",
    ] {
        let query = parse_query(q);
        let m = ex.execute_with_content(&query, 0, 0).unwrap();
        let f = ex.execute_files_only(&query, 0).unwrap();
        let ranked = ex.execute(&query).unwrap();
        println!(
            "PROBE {q:?}: lines={:?}; files={f:?}; ranked={:?}",
            m.iter()
                .map(|m| (&m.path, m.line_number, m.match_start, m.match_end))
                .collect::<Vec<_>>(),
            ranked.iter().map(|m| &m.path).collect::<Vec<_>>()
        );
    }
    let stamp = std::fs::File::open(root.join("b.txt"))
        .unwrap()
        .metadata()
        .unwrap()
        .modified()
        .unwrap();
    fs::write(root.join("b.txt"), "gone now\n").unwrap();
    println!(
        "PROBE stale cached: {:?}",
        ex.execute_files_only(&parse_query("needle"), 0).unwrap()
    );
    fs::write(root.join("b.txt"), "brandnewword\n").unwrap();
    std::fs::File::options()
        .write(true)
        .open(root.join("b.txt"))
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(stamp))
        .unwrap();
    println!("PROBE update same second: {}", update_index(root).unwrap());
    let r2 = IndexReader::open(root).unwrap();
    println!(
        "PROBE new word: {:?}",
        QueryExecutor::new(&r2)
            .execute_files_only(&parse_query("brandnewword"), 0)
            .unwrap()
    );
    let index = get_index_dir(root).unwrap();
    fs::write(index.join("docs.bin.tmp"), "active writer").unwrap();
    let _ = IndexReader::open(root).unwrap();
    println!(
        "PROBE reader deleted writer temp: {}",
        !index.join("docs.bin.tmp").exists()
    );
    remove_index(root).unwrap();
}
#[test]
fn repeated_compaction() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    for i in 0..4 {
        fs::write(
            root.join(format!("f{i}.txt")),
            if i < 3 {
                "vector::start\n"
            } else {
                "unrelated filler\n"
            },
        )
        .unwrap();
    }
    fxi::index::build::build_index_with_options(root, true, true, Some(2)).unwrap();
    for stage in 0..3 {
        if stage > 0 {
            fxi::index::compact::merge_segments(root).unwrap();
        }
        let r = IndexReader::open(root).unwrap();
        let found = QueryExecutor::new(&r)
            .execute_files_only(&parse_query("\"r::st\""), 0)
            .unwrap();
        println!("COMPACTION {stage}: {found:?}");
        drop(r);
        if stage == 1 {
            fs::write(root.join("new.txt"), "different unrelated file\n").unwrap();
            update_index(root).unwrap();
        }
    }
    remove_index(root).unwrap();
}
