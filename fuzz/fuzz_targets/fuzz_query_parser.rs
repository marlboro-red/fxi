#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../../tests/parser_generated.rs"]
mod parser_contract;

fuzz_target!(|data: &str| {
    // Includes strict/compatibility agreement, bounded AST inspection, fallible
    // planning and whole-word transformation. Oversize input tests early rejection.
    parser_contract::assert_parser_contract(data);
});
