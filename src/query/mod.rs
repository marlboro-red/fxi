//! Query parsing, planning, and execution.
//!
//! This module implements the query processing pipeline:
//!
//! ```text
//! Query String → Parser → AST → Planner → Plan → Executor → Results
//! ```
//!
//! ## Query Syntax
//!
//! FXI supports a rich query language:
//!
//! - **Literals**: `foo bar` (AND), `"exact phrase"` (phrase)
//! - **Boolean**: `foo | bar` (OR), `-foo` (NOT), `(expr)` (grouping)
//! - **Regex**: `re:/pattern/`
//! - **Proximity**: `near:foo,bar,5` (within 5 lines)
//! - **Filters**: `ext:rs`, `path:src/*.rs`, `lang:rust`
//! - **Size/Time**: `size:>1000`, `mtime:>2024-01-01`
//! - **Ranking**: `^foo` (boost), `sort:recency`, `top:100`
//!
//! ## Modules
//!
//! - [`parser`] - Tokenization and AST construction
//! - [`planner`] - Query optimization and execution planning
//! - [`executor`] - Parallel query execution with early termination
//! - [`scorer`] - Relevance scoring and ranking
//!
//! ## Example
//!
//! ```ignore
//! use fxi::query::{parse_query, QueryExecutor};
//! use fxi::index::reader::IndexReader;
//! use std::path::PathBuf;
//!
//! let reader = IndexReader::open(&PathBuf::from("/path/to/code")).unwrap();
//! let query = parse_query("ext:rs fn main");
//! let executor = QueryExecutor::new(&reader);
//! let results = executor.execute(&query).unwrap();
//! ```

pub mod executor;
pub mod parser;
pub mod planner;
pub mod scorer;

#[allow(unused_imports)]
pub use executor::ContentMatchResult;
pub use executor::QueryExecutor;
pub use parser::parse_query;
// Re-exports for public API
#[allow(unused_imports)]
pub use parser::{Query, QueryNode};
#[allow(unused_imports)]
pub use planner::QueryPlan;
#[allow(unused_imports)]
pub use scorer::{ScoreContext, Scorer, ScoringWeights};
