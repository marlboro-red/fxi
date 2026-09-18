#![no_main]
use libfuzzer_sys::fuzz_target;
// The deterministic test and coverage-guided target exercise exactly the same
// tiny index mutation logic and caps, with no daemon or external services.
#[path = "../../tests/index_generated.rs"]
mod index_generated;
thread_local! {
    static FIXTURE: index_generated::TinyIndex = index_generated::TinyIndex::new().expect("create tiny fixture");
}
fuzz_target!(|data: &[u8]| {
    if !(3..=256).contains(&data.len()) {
        return;
    }
    FIXTURE.with(|fixture| {
        fixture
            .exercise(data)
            .expect("fixture filesystem operation");
    });
});
