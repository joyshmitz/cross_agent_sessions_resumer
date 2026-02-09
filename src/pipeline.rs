//! Conversion pipeline orchestrator.
//!
//! Ties detection, reading, validation, writing, and verification into a
//! single `convert()` call. Generic over the [`Provider`](crate::providers::Provider)
//! trait — concrete providers are wired in via the registry.

use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use crate::discovery::ProviderRegistry;
use crate::error::CasrError;
use crate::model::CanonicalSession;
use crate::providers::WrittenSession;

/// Top-level orchestrator for session conversion.
pub struct ConversionPipeline {
    pub registry: ProviderRegistry,
}

/// Options passed through the pipeline from CLI flags.
#[derive(Debug, Clone)]
pub struct ConvertOptions {
    pub dry_run: bool,
    pub force: bool,
    pub verbose: bool,
    pub source_hint: Option<String>,
}

/// Outcome of a successful (or dry-run) conversion.
#[derive(Debug)]
pub struct ConversionResult {
    pub source_provider: String,
    pub target_provider: String,
    pub canonical_session: CanonicalSession,
    pub written: Option<WrittenSession>,
    pub warnings: Vec<String>,
}

impl ConversionPipeline {
    /// Run the full detect → read → validate → write → verify pipeline.
    pub fn convert(
        &self,
        _target_alias: &str,
        _session_id: &str,
        _opts: ConvertOptions,
    ) -> anyhow::Result<ConversionResult> {
        todo!("bd-ikb.1: conversion pipeline")
    }
}

// ---------------------------------------------------------------------------
// Atomic file writing
// ---------------------------------------------------------------------------

/// Outcome of a successful atomic write operation.
#[derive(Debug, Clone)]
pub struct AtomicWriteOutcome {
    /// Final destination path.
    pub target_path: PathBuf,
    /// Temp file used during write (already renamed away).
    pub temp_path: PathBuf,
    /// Path to the `.bak` backup of a pre-existing file (if `--force` was used).
    pub backup_path: Option<PathBuf>,
}

/// Write `content` atomically to `target_path` using temp-then-rename.
///
/// Guarantees: either the old target remains intact, or the new target is
/// fully written and fsynced. Never leaves partial writes.
///
/// Returns `AtomicWriteOutcome` on success, or:
/// - [`CasrError::SessionConflict`] if target exists and `force` is false.
/// - [`CasrError::SessionWriteError`] on I/O failures.
pub fn atomic_write(
    target_path: &Path,
    content: &[u8],
    force: bool,
) -> Result<AtomicWriteOutcome, CasrError> {
    use std::io::Write;

    // 1. Create parent directories.
    if let Some(parent) = target_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| CasrError::SessionWriteError {
            path: target_path.to_path_buf(),
            provider: String::new(),
            detail: format!("failed to create parent directories: {e}"),
        })?;
    }

    // 2. Check for existing target.
    let backup_path = if target_path.exists() {
        if !force {
            return Err(CasrError::SessionConflict {
                session_id: String::new(),
                existing_path: target_path.to_path_buf(),
            });
        }
        // Create backup with deterministic de-dupe.
        let bak = find_backup_path(target_path);
        debug!(
            target = %target_path.display(),
            backup = %bak.display(),
            "backing up existing file"
        );
        std::fs::rename(target_path, &bak).map_err(|e| CasrError::SessionWriteError {
            path: target_path.to_path_buf(),
            provider: String::new(),
            detail: format!("failed to create backup: {e}"),
        })?;
        Some(bak)
    } else {
        None
    };

    // 3. Write to temp file in the same directory.
    let temp_name = format!(
        ".casr-tmp-{}",
        uuid::Uuid::new_v4().as_hyphenated()
    );
    let temp_path = target_path
        .parent()
        .unwrap_or(Path::new("."))
        .join(&temp_name);

    let write_result = (|| -> Result<(), std::io::Error> {
        let mut file = std::fs::File::create(&temp_path)?;
        file.write_all(content)?;
        file.flush()?;
        file.sync_all()?;
        Ok(())
    })();

    if let Err(e) = write_result {
        // Cleanup temp file on write failure.
        let _ = std::fs::remove_file(&temp_path);
        // Restore backup if we made one.
        if let Some(ref bak) = backup_path {
            warn!(
                backup = %bak.display(),
                target = %target_path.display(),
                "restoring backup after write failure"
            );
            let _ = std::fs::rename(bak, target_path);
        }
        return Err(CasrError::SessionWriteError {
            path: target_path.to_path_buf(),
            provider: String::new(),
            detail: format!("failed to write temp file: {e}"),
        });
    }

    // 4. Atomic rename temp -> target.
    if let Err(e) = std::fs::rename(&temp_path, target_path) {
        let _ = std::fs::remove_file(&temp_path);
        if let Some(ref bak) = backup_path {
            warn!(
                backup = %bak.display(),
                target = %target_path.display(),
                "restoring backup after rename failure"
            );
            let _ = std::fs::rename(bak, target_path);
        }
        return Err(CasrError::SessionWriteError {
            path: target_path.to_path_buf(),
            provider: String::new(),
            detail: format!("failed to rename temp file to target: {e}"),
        });
    }

    info!(target = %target_path.display(), "atomic write complete");

    Ok(AtomicWriteOutcome {
        target_path: target_path.to_path_buf(),
        temp_path,
        backup_path,
    })
}

/// Restore a backup after a verification failure.
///
/// Removes the broken target and renames the backup back into place.
pub fn restore_backup(outcome: &AtomicWriteOutcome) -> Result<(), CasrError> {
    if let Some(ref bak) = outcome.backup_path {
        warn!(
            backup = %bak.display(),
            target = %outcome.target_path.display(),
            "restoring backup after verification failure"
        );
        let _ = std::fs::remove_file(&outcome.target_path);
        std::fs::rename(bak, &outcome.target_path).map_err(|e| CasrError::SessionWriteError {
            path: outcome.target_path.clone(),
            provider: String::new(),
            detail: format!("failed to restore backup: {e}"),
        })?;
    } else {
        // No backup: just remove the broken target.
        let _ = std::fs::remove_file(&outcome.target_path);
    }
    Ok(())
}

/// Find an available backup path, deduplicating with `.bak`, `.bak.1`, `.bak.2`, etc.
fn find_backup_path(target: &Path) -> PathBuf {
    let base = target.as_os_str().to_string_lossy();
    let bak = PathBuf::from(format!("{base}.bak"));
    if !bak.exists() {
        return bak;
    }
    for i in 1..100 {
        let numbered = PathBuf::from(format!("{base}.bak.{i}"));
        if !numbered.exists() {
            return numbered;
        }
    }
    // Fallback: use random suffix.
    PathBuf::from(format!(
        "{base}.bak.{}",
        uuid::Uuid::new_v4().as_hyphenated()
    ))
}
