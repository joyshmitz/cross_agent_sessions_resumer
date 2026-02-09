//! Actionable typed errors for casr.
//!
//! Each error variant includes enough context for the user to understand
//! what went wrong and what to do next. Internal propagation uses `anyhow`;
//! the public API exposes these `thiserror` types.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A candidate match returned when a session ID is ambiguous.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    /// Provider slug (e.g. `"claude-code"`).
    pub provider: String,
    /// Resolved path to the session file.
    pub path: PathBuf,
}

/// Errors that casr surfaces to the user.
///
/// Every variant carries enough context to render an actionable message
/// *and* to serialize as a stable JSON `error_type` string.
#[derive(Debug, thiserror::Error)]
pub enum CasrError {
    /// Session ID not found in any installed provider.
    #[error(
        "Session '{session_id}' not found. Checked: {providers_checked:?} ({sessions_scanned} sessions scanned). Run 'casr list' to see all sessions."
    )]
    SessionNotFound {
        session_id: String,
        providers_checked: Vec<String>,
        sessions_scanned: usize,
    },

    /// Session ID matched in multiple providers — user must disambiguate.
    #[error(
        "Session '{session_id}' found in multiple providers: {}. Use --source <alias> to choose.",
        candidates.iter().map(|c| c.provider.as_str()).collect::<Vec<_>>().join(", ")
    )]
    AmbiguousSessionId {
        session_id: String,
        candidates: Vec<Candidate>,
    },

    /// Unknown provider alias in CLI input.
    #[error("Unknown provider alias '{alias}'. Known aliases: {}", known_aliases.join(", "))]
    UnknownProviderAlias {
        alias: String,
        known_aliases: Vec<String>,
    },

    /// Provider cannot perform the requested operation.
    ///
    /// Reasons distinguish: binary missing, no readable roots, no writable
    /// roots, permission denied. A missing binary is only a hard error for
    /// `resume` (the target must be launchable); reads/writes may still work
    /// if roots exist.
    #[error("{provider}: {reason}")]
    ProviderUnavailable {
        provider: String,
        reason: String,
        evidence: Vec<String>,
    },

    /// Failed to parse a session from its native format.
    #[error("Failed to read {provider} session at {}: {detail}", path.display())]
    SessionReadError {
        path: PathBuf,
        provider: String,
        detail: String,
    },

    /// Failed to write a converted session to disk.
    #[error("Failed to write {provider} session to {}: {detail}", path.display())]
    SessionWriteError {
        path: PathBuf,
        provider: String,
        detail: String,
    },

    /// Target session file already exists and `--force` was not supplied.
    #[error(
        "Session already exists at {}. Use --force to overwrite (creates .bak backup).",
        existing_path.display()
    )]
    SessionConflict {
        session_id: String,
        existing_path: PathBuf,
    },

    /// Canonical session failed validation checks.
    ///
    /// `errors` are fatal (pipeline stops); `warnings` and `info` are
    /// surfaced in UX/JSON output but don't block conversion.
    #[error("Session validation failed: {}", errors.join("; "))]
    ValidationError {
        errors: Vec<String>,
        warnings: Vec<String>,
        info: Vec<String>,
    },

    /// Read-back verification failed after writing — this is a casr bug.
    #[error(
        "Written file(s) could not be read back ({provider}). This is a bug in casr. Detail: {detail}"
    )]
    VerifyFailed {
        provider: String,
        written_paths: Vec<PathBuf>,
        detail: String,
    },
}
