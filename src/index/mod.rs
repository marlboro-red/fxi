pub mod build;
pub mod compact;
pub mod reader;
pub mod stats;
pub mod suffix_array;
pub mod types;
pub mod writer;

// Re-exports for public API
#[allow(unused_imports)]
pub use reader::IndexReader;
#[allow(unused_imports)]
pub use types::*;
#[allow(unused_imports)]
pub use writer::IndexWriter;
