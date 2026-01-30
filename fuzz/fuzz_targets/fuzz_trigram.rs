#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Fuzz trigram extraction with arbitrary bytes
    // This tests binary detection and trigram generation
    let _ = fxi::utils::extract_trigrams(data);
    let _ = fxi::utils::is_binary(data);
    let _ = fxi::utils::is_minified(data);
});
