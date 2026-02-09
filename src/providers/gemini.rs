//! Gemini CLI provider — reads/writes JSON sessions under `~/.gemini/tmp/`.
//!
//! Session files: `<hash>/chats/session-<id>.json`
//! Resume command: `gemini` (in the project directory)
//!
//! ## JSON format
//!
//! Single JSON object per file:
//! ```json
//! {
//!   "sessionId": "…",
//!   "startTime": "…",
//!   "lastUpdated": "…",
//!   "messages": [
//!     { "type": "user"|"gemini"|"model", "content": "…"|[…], "timestamp": "…" }
//!   ]
//! }
//! ```
//!
//! Note: Gemini may use `"gemini"` or `"model"` for assistant responses.

use std::path::{Path, PathBuf};

use anyhow::Context;
use tracing::{debug, trace};
use walkdir::WalkDir;

use crate::discovery::DetectionResult;
use crate::model::{
    CanonicalMessage, CanonicalSession, MessageRole, flatten_content, normalize_role,
    parse_timestamp, reindex_messages, truncate_title,
};
use crate::providers::{Provider, WriteOptions, WrittenSession};

/// Gemini CLI provider implementation.
pub struct Gemini;

/// Compute the Gemini project hash directory name from a workspace path.
///
/// Algorithm: `SHA256(absolute_workspace_path)` as lowercase hex.
///
/// Example: `/data/projects/foo` → `sha256(b"/data/projects/foo")` (64 hex chars)
pub fn project_hash(workspace: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(workspace.to_string_lossy().as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Generate a Gemini session filename from a session ID and timestamp.
///
/// Convention: `session-YYYY-MM-DDThh-mm-<uuid-prefix>.json`
/// where `<uuid-prefix>` is the first 8 chars of the session UUID.
pub fn session_filename(session_id: &str, now: &chrono::DateTime<chrono::Utc>) -> String {
    let ts = now.format("%Y-%m-%dT%H-%M").to_string();
    let prefix = &session_id[..session_id.len().min(8)];
    format!("session-{ts}-{prefix}.json")
}

impl Gemini {
    /// Root directory for Gemini data.
    /// Respects `GEMINI_HOME` env var override.
    fn home_dir() -> Option<PathBuf> {
        if let Ok(home) = std::env::var("GEMINI_HOME") {
            return Some(PathBuf::from(home));
        }
        dirs::home_dir().map(|h| h.join(".gemini"))
    }

    /// Tmp directory where session hashes live.
    fn tmp_dir() -> Option<PathBuf> {
        Self::home_dir().map(|h| h.join("tmp"))
    }
}

impl Provider for Gemini {
    fn name(&self) -> &str {
        "Gemini CLI"
    }

    fn slug(&self) -> &str {
        "gemini"
    }

    fn cli_alias(&self) -> &str {
        "gmi"
    }

    fn detect(&self) -> DetectionResult {
        let mut evidence = Vec::new();
        let mut installed = false;

        if which::which("gemini").is_ok() {
            evidence.push("gemini binary found in PATH".to_string());
            installed = true;
        }

        if let Some(home) = Self::home_dir()
            && home.is_dir()
        {
            evidence.push(format!("{} exists", home.display()));
            installed = true;
        }

        trace!(provider = "gemini", ?evidence, installed, "detection");
        DetectionResult {
            installed,
            version: None,
            evidence,
        }
    }

    fn session_roots(&self) -> Vec<PathBuf> {
        let Some(tmp) = Self::tmp_dir() else {
            return vec![];
        };
        if !tmp.is_dir() {
            return vec![];
        }
        // Each hash directory under tmp/ that has a chats/ subdirectory is a root.
        std::fs::read_dir(&tmp)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let chats = entry.path().join("chats");
                chats.is_dir().then_some(chats)
            })
            .collect()
    }

    fn owns_session(&self, session_id: &str) -> Option<PathBuf> {
        let tmp = Self::tmp_dir()?;
        if !tmp.is_dir() {
            return None;
        }

        // Gemini sessions are at <hash>/chats/session-*.json.
        //
        // Real filename convention: session-YYYY-MM-DDThh-mm-<uuid_prefix8>.json
        // so we cannot rely on exact filename == session_id.
        let exact_name = format!("session-{session_id}.json");
        let id_prefix = session_id
            .chars()
            .take(8)
            .collect::<String>()
            .to_ascii_lowercase();

        for entry in WalkDir::new(&tmp)
            .max_depth(3)
            .into_iter()
            .filter_map(Result::ok)
        {
            let path = entry.path();
            // Files must be in a chats/ directory.
            if let Some(parent) = path.parent()
                && parent.file_name().and_then(|n| n.to_str()) == Some("chats")
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
            {
                // Legacy-style exact filename.
                if name == exact_name {
                    debug!(path = %path.display(), "found Gemini session by exact filename");
                    return Some(path.to_path_buf());
                }

                // Prefix-based lookup for modern filenames.
                if !id_prefix.is_empty() {
                    let name_lc = name.to_ascii_lowercase();
                    if name_lc.ends_with(&format!("-{id_prefix}.json"))
                        && session_id_from_file(path).as_deref() == Some(session_id)
                    {
                        debug!(path = %path.display(), "found Gemini session by UUID prefix + sessionId body match");
                        return Some(path.to_path_buf());
                    }
                }
            }
        }
        None
    }

    fn read_session(&self, path: &Path) -> anyhow::Result<CanonicalSession> {
        debug!(path = %path.display(), "reading Gemini session");

        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let root: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!("failed to parse JSON {}", path.display()))?;

        // Session-level fields.
        let session_id = root
            .get("sessionId")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| {
                // Derive from filename: session-<uuid>.json → <uuid>
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.strip_prefix("session-"))
                    .unwrap_or("unknown")
                    .to_string()
            });

        let project_hash = root
            .get("projectHash")
            .and_then(|v| v.as_str())
            .map(String::from);

        let started_at = root.get("startTime").and_then(parse_timestamp);
        let mut ended_at = root.get("lastUpdated").and_then(parse_timestamp);

        // Parse messages array.
        let msg_array = root
            .get("messages")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut messages: Vec<CanonicalMessage> = Vec::new();

        for (i, msg) in msg_array.iter().enumerate() {
            // Role: Gemini uses "type" field with "user" or "model".
            let role_str = msg
                .get("type")
                .or_else(|| msg.get("role"))
                .and_then(|v| v.as_str())
                .unwrap_or("user");
            let role = normalize_role(role_str);

            // Content: string or array of content parts.
            let content_val = msg.get("content");
            let text = content_val.map(flatten_content).unwrap_or_default();
            if text.trim().is_empty() {
                trace!(index = i, "skipping empty Gemini message");
                continue;
            }

            // Timestamp.
            let ts = msg.get("timestamp").and_then(parse_timestamp);
            if let Some(t) = ts {
                ended_at = Some(ended_at.map_or(t, |e: i64| e.max(t)));
            }

            messages.push(CanonicalMessage {
                idx: 0,
                role,
                content: text,
                timestamp: ts,
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: msg.clone(),
            });
        }

        reindex_messages(&mut messages);

        // Title from first user message.
        let title = messages
            .iter()
            .find(|m| m.role == MessageRole::User)
            .map(|m| truncate_title(&m.content, 100));

        // Workspace: try to extract from message content (project paths).
        let workspace = extract_workspace_from_messages(&messages);

        // Metadata.
        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "source".into(),
            serde_json::Value::String("gemini".to_string()),
        );
        if let Some(ref ph) = project_hash {
            metadata.insert("project_hash".into(), serde_json::Value::String(ph.clone()));
        }

        debug!(
            session_id,
            messages = messages.len(),
            "Gemini session parsed"
        );

        Ok(CanonicalSession {
            session_id,
            provider_slug: "gemini".to_string(),
            workspace,
            title,
            started_at,
            ended_at,
            messages,
            metadata: serde_json::Value::Object(metadata),
            source_path: path.to_path_buf(),
            model_name: None,
        })
    }

    fn write_session(
        &self,
        _session: &CanonicalSession,
        _opts: &WriteOptions,
    ) -> anyhow::Result<WrittenSession> {
        todo!("bd-1a2.3: Gemini writer")
    }

    fn resume_command(&self, _session_id: &str) -> String {
        "gemini".to_string()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Try to extract a workspace path from message content.
///
/// Scans the first N messages for common path patterns:
/// - `"# AGENTS.md instructions for /data/projects/foo"`
/// - `"Working directory: /path/to/project"`
/// - Any `/data/projects/X` reference
fn extract_workspace_from_messages(messages: &[CanonicalMessage]) -> Option<PathBuf> {
    let scan_limit = messages.len().min(50);
    for msg in &messages[..scan_limit] {
        // Look for /data/projects/ patterns (common convention).
        if let Some(idx) = msg.content.find("/data/projects/") {
            let rest = &msg.content[idx..];
            // Extract project name (next path segment after /data/projects/).
            let project_path: String = rest
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != '"' && *c != '\'' && *c != ')')
                .collect();
            // Normalize to just /data/projects/<name>
            let parts: Vec<&str> = project_path.split('/').collect();
            if parts.len() >= 4 {
                let normalized = format!("/{}/{}/{}", parts[1], parts[2], parts[3]);
                return Some(PathBuf::from(normalized));
            }
        }
        // Look for absolute paths on common prefixes.
        for prefix in ["/home/", "/Users/", "/root/"] {
            if let Some(idx) = msg.content.find(prefix) {
                let rest = &msg.content[idx..];
                let path: String = rest
                    .chars()
                    .take_while(|c| !c.is_whitespace() && *c != '"' && *c != '\'')
                    .collect();
                if path.len() > prefix.len() + 3 {
                    return Some(PathBuf::from(path));
                }
            }
        }
    }
    None
}

fn session_id_from_file(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    json.get("sessionId")
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::{project_hash, session_filename};
    use chrono::{TimeZone, Utc};
    use std::path::Path;

    #[test]
    fn project_hash_matches_observed_sha256_mapping() {
        let workspace = Path::new("/data/projects/flywheel_gateway");
        let hash = project_hash(workspace);
        assert_eq!(
            hash,
            "b7da685261f0fff76430fd68dd709a693a8abac1c72c19c49f2fd1c7424c6d4e"
        );
    }

    #[test]
    fn session_filename_uses_timestamp_and_uuid_prefix() {
        let now = Utc
            .with_ymd_and_hms(2026, 1, 10, 2, 6, 44)
            .single()
            .expect("valid timestamp");
        let filename = session_filename("8c1890a5-eb39-4c5c-acff-93790d35dd3f", &now);
        assert_eq!(filename, "session-2026-01-10T02-06-8c1890a5.json");
    }
}
