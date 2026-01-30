#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    // Fuzz the query parser with arbitrary strings
    // This should not panic or cause undefined behavior
    let _ = fxi::query::parse_query(data);
});
