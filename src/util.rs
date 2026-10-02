//! Small shared helpers.

/// Map any error with a Debug representation into anyhow. IROH 1.x uses the
/// `n0-error` error type rather than std errors, so `?` conversion into
/// anyhow is not guaranteed; this keeps call sites terse and safe.
pub fn de<E: std::fmt::Debug>(e: E) -> anyhow::Error {
    anyhow::anyhow!("{e:?}")
}

/// Current time as unix epoch seconds.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
