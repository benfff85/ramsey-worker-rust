/// Logging utilities with timestamp prefixes
use chrono::Utc;
use std::sync::OnceLock;

/// Get formatted timestamp for log messages
#[inline]
pub fn timestamp() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// Whether verbose per-stage/per-batch tracing is enabled (`RAMSEY_LOG_DEBUG=true`).
///
/// Read once and cached: this is checked on paths that run several times per stage, and stages
/// now advance more than once a second across the fleet.
pub fn debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("RAMSEY_LOG_DEBUG")
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false)
    })
}

/// Log an info message with timestamp prefix
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        println!("[{}] {}", $crate::logging::timestamp(), format!($($arg)*))
    };
}

/// Log a verbose tracing message, only when `RAMSEY_LOG_DEBUG=true`.
///
/// Use this for anything that fires per stage or per batch. Fourteen workers each narrating every
/// stage transition costs ~56 lines per stage, which at the post-hoist stage rate swamped the
/// events actually worth reading. The arguments are not formatted when debug is off.
#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {
        if $crate::logging::debug_enabled() {
            println!("[{}] DEBUG {}", $crate::logging::timestamp(), format!($($arg)*))
        }
    };
}

/// Log an error message with timestamp prefix
#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        eprintln!("[{}] {}", $crate::logging::timestamp(), format!($($arg)*))
    };
}
