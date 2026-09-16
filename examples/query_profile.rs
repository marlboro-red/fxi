//! Stage measurements on an already-built index; no daemon or CLI startup.
//! Usage: FXI_INDEXES=... cargo run --release --example query_profile -- ROOT
use fxi::index::reader::IndexReader;
use fxi::query::planner::PlanStep;
use fxi::query::{QueryExecutor, QueryPlan, parse_query};
use roaring::RoaringBitmap;
use std::time::Instant;

fn candidates(reader: &IndexReader, plan: &QueryPlan) -> RoaringBitmap {
    let mut result = reader.valid_doc_ids().clone();
    for step in &plan.steps {
        let next = match step {
            PlanStep::TrigramIntersect(grams) => {
                let grams: Vec<_> = grams
                    .iter()
                    .copied()
                    .filter(|g| !reader.is_stop_gram(*g))
                    .collect();
                reader.get_trigram_docs_with_bloom(&grams)
            }
            PlanStep::Union(plans) => plans
                .iter()
                .map(|p| candidates(reader, p))
                .fold(RoaringBitmap::new(), |a, b| a | b),
            _ => panic!("This profiling probe accepts positive regex constraints only"),
        };
        result &= next;
    }
    result
}

fn main() -> anyhow::Result<()> {
    let root = std::env::args().nth(1).expect("ROOT");
    let reader = IndexReader::open(std::path::Path::new(&root))?;
    let executor = QueryExecutor::new(&reader);
    let mut rows = Vec::new();
    for pattern in [
        "raxFind",
        "return",
        "static void",
        "raxFind|dictRehash",
        ".*raxFind",
        "(?i)serverassert",
    ] {
        let query = parse_query(&format!("re:/{pattern}/"));
        let plan = QueryPlan::from_query(&query);
        let docs = candidates(&reader, &plan);
        let bytes: u64 = docs
            .iter()
            .filter_map(|id| reader.get_document(id))
            .map(|d| d.size)
            .sum();
        let expected = executor.execute_files_only(&query, 0)?;
        let mut plans_us = Vec::new();
        let mut lookup_us = Vec::new();
        let mut total_us = Vec::new();
        for _ in 0..21 {
            let start = Instant::now();
            let plan = QueryPlan::from_query(&query);
            plans_us.push(start.elapsed().as_micros());
            let start = Instant::now();
            assert_eq!(candidates(&reader, &plan), docs);
            lookup_us.push(start.elapsed().as_micros());
            let start = Instant::now();
            assert_eq!(executor.execute_files_only(&query, 0)?, expected);
            total_us.push(start.elapsed().as_micros());
        }
        plans_us.sort();
        lookup_us.sort();
        total_us.sort();
        rows.push(serde_json::json!({"pattern":pattern,"candidate_files":docs.len(),"candidate_bytes":bytes,
            "matching_files":expected.len(),"plan_median_us":plans_us[10],"lookup_median_us":lookup_us[10],
            "engine_median_us":total_us[10],"engine_samples_us":total_us}));
    }
    println!("{}", serde_json::to_string_pretty(&rows)?);
    Ok(())
}
