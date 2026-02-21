/// Logging utilities with timestamp prefixes
use chrono::Utc;

/// Get formatted timestamp for log messages
#[inline]
pub fn timestamp() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// Log an info message with timestamp prefix
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        println!("[{}] {}", $crate::logging::timestamp(), format!($($arg)*))
    };
}

/// Log an error message with timestamp prefix  
#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        eprintln!("[{}] {}", $crate::logging::timestamp(), format!($($arg)*))
    };
}
