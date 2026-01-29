//! Progress bar abstraction that becomes no-op when the `progress` feature is disabled

#[cfg(feature = "progress")]
pub use indicatif::{ProgressBar, ProgressStyle};

#[cfg(not(feature = "progress"))]
pub use self::noop::*;

#[cfg(not(feature = "progress"))]
mod noop {
    use std::time::Duration;

    /// No-op progress bar when `progress` feature is disabled
    #[derive(Clone)]
    pub struct ProgressBar;

    impl ProgressBar {
        pub fn new(_len: u64) -> Self {
            ProgressBar
        }

        pub fn new_spinner() -> Self {
            ProgressBar
        }

        pub fn set_style(&self, _style: ProgressStyle) {}
        pub fn set_message(&self, _msg: impl Into<std::borrow::Cow<'static, str>>) {}
        pub fn enable_steady_tick(&self, _interval: Duration) {}
        pub fn inc(&self, _delta: u64) {}
        pub fn finish_with_message(&self, _msg: impl Into<std::borrow::Cow<'static, str>>) {}
        pub fn finish_and_clear(&self) {}
    }

    /// No-op progress style
    pub struct ProgressStyle;

    impl ProgressStyle {
        pub fn default_spinner() -> Self {
            ProgressStyle
        }

        pub fn default_bar() -> Self {
            ProgressStyle
        }

        pub fn template(self, _template: &str) -> Result<Self, std::convert::Infallible> {
            Ok(self)
        }

        pub fn progress_chars(self, _chars: &str) -> Self {
            self
        }
    }
}
