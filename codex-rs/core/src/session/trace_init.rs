// Modified by RenCrow Switch Core, 2026-11-27: declare constructor-owned trace initialization.
use codex_rollout_trace::ThreadTraceContext;
#[cfg(test)]
use std::path::PathBuf;

/// Distinguish inherited child traces from an explicit root test dependency.
pub(crate) enum SessionTraceInit {
    /// Preserve root environment configuration and spawned-child inheritance.
    Automatic { parent: ThreadTraceContext },
    /// Create the real root sink using the constructor's resolved metadata.
    #[cfg(test)]
    RootInDirectory(PathBuf),
}
