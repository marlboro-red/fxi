// Utility functions
pub fn format_error(msg: &str) -> String {
    format!("ERROR: {}", msg)
}

pub fn format_warning(msg: &str) -> String {
    format!("WARNING: {}", msg)
}

// todo: add more utils
pub fn debug_print(msg: &str) {
    eprintln!("DEBUG: {}", msg);
}
