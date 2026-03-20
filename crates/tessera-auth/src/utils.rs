// Copyright 2026 BelowZero Security OU. All rights reserved.

/// Return the current time as seconds since the Unix epoch.
///
/// # Panics
///
/// Panics if the system clock is set before the Unix epoch (1970-01-01).
pub fn unix_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is before epoch")
        .as_secs()
}
