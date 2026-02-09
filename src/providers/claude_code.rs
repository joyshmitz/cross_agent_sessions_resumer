//! Claude Code provider — reads/writes JSONL sessions under `~/.claude/projects/`.
//!
//! Session files: `<project-key>/<session-id>.jsonl`
//! Resume command: `claude --resume <session-id>`
//!
//! ## JSONL format
//!
//! Each line is a JSON object with a `type` field:
//! - `"user"` / `"assistant"` — conversational messages (extracted).
//! - `"file-history-snapshot"` / `"summary"` — non-conversational (skipped).
//!
//! Conversational entries carry:
//! - `message.role` / `message.content` / `message.model`
//! - Top-level `cwd`, `sessionId`, `version`, `gitBranch`, `timestamp`
//! - `message.content` may be a string or array of content blocks.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Context;
use tracing::{debug, trace, warn};

use crate::discovery::DetectionResult;
use crate::model::{
    CanonicalMessage, CanonicalSession, MessageRole, ToolCall, ToolResult, flatten_content,
    normalize_role, parse_timestamp, reindex_messages, truncate_title,
};
use crate::providers::{Provider, WriteOptions, WrittenSession};

/// Claude Code provider implementation.
pub struct ClaudeCode;

/// Derive Claude Code's project directory key from a workspace path.
///
/// Reverse-engineered from real Claude Code installations: every non-alphanumeric
/// character is replaced by `-` while alphanumeric characters (including case)
/// are preserved.
///
/// Examples:
/// - `/data/projects/cross_agent_sessions_resumer` -> `-data-projects-cross-agent-sessions-resumer`
/// - `/data/projects/jeffreys-skills.md` -> `-data-projects-jeffreys-skills-md`
pub fn project_dir_key(workspace: &Path) -> String {
    workspace
        .to_string_lossy()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

impl ClaudeCode {
    /// Root directory for Claude Code sessions.
    /// Respects `CLAUDE_HOME` env var override.
    fn home_dir() -> Option<PathBuf> {
        if let Ok(home) = std::env::var("CLAUDE_HOME") {
            return Some(PathBuf::from(home));
        }
        dirs::home_dir().map(|h| h.join(".claude"))
    }

    /// Projects directory where session files live.
    fn projects_dir() -> Option<PathBuf> {
        Self::home_dir().map(|h| h.join("projects"))
    }
}

impl Provider for ClaudeCode {
    fn name(&self) -> &str {
        "Claude Code"
    }

    fn slug(&self) -> &str {
        "claude-code"
    }

    fn cli_alias(&self) -> &str {
        "cc"
    }

    fn detect(&self) -> DetectionResult {
        let mut evidence = Vec::new();
        let mut installed = false;

        // Check for binary in PATH.
        if which::which("claude").is_ok() {
            evidence.push("claude binary found in PATH".to_string());
            installed = true;
        }

        // Check for config directory.
        if let Some(home) = Self::home_dir()
            && home.is_dir()
        {
            evidence.push(format!("{} exists", home.display()));
            installed = true;
        }

        trace!(provider = "claude-code", ?evidence, installed, "detection");
        DetectionResult {
            installed,
            version: None,
            evidence,
        }
    }

    fn session_roots(&self) -> Vec<PathBuf> {
        match Self::projects_dir() {
            Some(dir) if dir.is_dir() => vec![dir],
            _ => vec![],
        }
    }

    fn owns_session(&self, session_id: &str) -> Option<PathBuf> {
        let projects_dir = Self::projects_dir()?;
        if !projects_dir.is_dir() {
            return None;
        }
        // Scan project directories for a file matching <session-id>.jsonl
        let target_filename = format!("{session_id}.jsonl");
        for entry in std::fs::read_dir(&projects_dir).ok()?.flatten() {
            if entry.file_type().ok()?.is_dir() {
                let candidate = entry.path().join(&target_filename);
                if candidate.is_file() {
                    debug!(path = %candidate.display(), "found Claude Code session");
                    return Some(candidate);
                }
            }
        }
        None
    }

    fn read_session(&self, path: &Path) -> anyhow::Result<CanonicalSession> {
        debug!(path = %path.display(), "reading Claude Code session");

        let file = std::fs::File::open(path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        let reader = BufReader::new(file);

        // Session-level metadata extracted from the first relevant entry.
        let mut session_id: Option<String> = None;
        let mut workspace: Option<PathBuf> = None;
        let mut git_branch: Option<String> = None;
        let mut version: Option<String> = None;
        let mut started_at: Option<i64> = None;
        let mut ended_at: Option<i64> = None;
        let mut model_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();

        let mut messages: Vec<CanonicalMessage> = Vec::new();
        let mut line_num: usize = 0;
        let mut skipped: usize = 0;

        for line_result in reader.lines() {
            line_num += 1;

            let line = match line_result {
                Ok(l) => l,
                Err(e) => {
                    warn!(line = line_num, error = %e, "skipping unreadable line");
                    skipped += 1;
                    continue;
                }
            };

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let entry: serde_json::Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    warn!(line = line_num, error = %e, "skipping malformed JSON line");
                    skipped += 1;
                    continue;
                }
            };

            // Extract session-level metadata from first entry that has them.
            if session_id.is_none()
                && let Some(sid) = entry.get("sessionId").and_then(|v| v.as_str())
            {
                session_id = Some(sid.to_string());
            }
            if workspace.is_none()
                && let Some(cwd) = entry.get("cwd").and_then(|v| v.as_str())
            {
                workspace = Some(PathBuf::from(cwd));
            }
            if git_branch.is_none()
                && let Some(gb) = entry.get("gitBranch").and_then(|v| v.as_str())
                && gb != "HEAD"
            {
                git_branch = Some(gb.to_string());
            }
            if version.is_none()
                && let Some(v) = entry.get("version").and_then(|v| v.as_str())
            {
                version = Some(v.to_string());
            }

            // Filter: only extract user/assistant conversational messages.
            let entry_type = entry.get("type").and_then(|v| v.as_str());
            let is_conversational = matches!(entry_type, Some("user") | Some("assistant"));
            if !is_conversational {
                trace!(
                    line = line_num,
                    ?entry_type,
                    "skipping non-conversational entry"
                );
                continue;
            }

            // Extract role from message.role → top-level type.
            let role_str = entry
                .pointer("/message/role")
                .and_then(|v| v.as_str())
                .or(entry_type)
                .unwrap_or("user");
            let role = normalize_role(role_str);

            // Extract content from message.content → top-level content.
            let content_value = entry
                .pointer("/message/content")
                .or_else(|| entry.get("content"));
            let content = content_value.map(flatten_content).unwrap_or_default();

            // Skip empty content messages.
            if content.trim().is_empty() {
                trace!(line = line_num, "skipping empty content message");
                continue;
            }

            // Extract timestamp.
            let ts_value = entry
                .get("timestamp")
                .or_else(|| entry.pointer("/message/timestamp"));
            let timestamp = ts_value.and_then(parse_timestamp);

            // Track start/end times.
            if let Some(ts) = timestamp {
                started_at = Some(started_at.map_or(ts, |s: i64| s.min(ts)));
                ended_at = Some(ended_at.map_or(ts, |e: i64| e.max(ts)));
            }

            // Extract model name (author).
            let model = entry
                .pointer("/message/model")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if let Some(ref m) = model {
                *model_counts.entry(m.clone()).or_insert(0) += 1;
            }

            // Extract tool calls from content blocks.
            let tool_calls = extract_tool_calls(content_value);
            let tool_results = extract_tool_results(content_value);

            messages.push(CanonicalMessage {
                idx: 0, // Re-indexed below.
                role,
                content,
                timestamp,
                author: model,
                tool_calls,
                tool_results,
                extra: entry,
            });
        }

        reindex_messages(&mut messages);

        // Derive session ID from filename if not found in content.
        let session_id = session_id.unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string()
        });

        // Derive title from first user message.
        let title = messages
            .iter()
            .find(|m| m.role == MessageRole::User)
            .map(|m| truncate_title(&m.content, 100));

        // Most common model name.
        let model_name = model_counts
            .into_iter()
            .max_by_key(|(_, count)| *count)
            .map(|(name, _)| name);

        // Build metadata.
        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "source".into(),
            serde_json::Value::String("claude_code".to_string()),
        );
        if let Some(ref gb) = git_branch {
            metadata.insert("gitBranch".into(), serde_json::Value::String(gb.clone()));
        }
        if let Some(ref v) = version {
            metadata.insert("claudeVersion".into(), serde_json::Value::String(v.clone()));
        }

        debug!(
            session_id,
            messages = messages.len(),
            skipped,
            "Claude Code session parsed"
        );

        Ok(CanonicalSession {
            session_id,
            provider_slug: "claude-code".to_string(),
            workspace,
            title,
            started_at,
            ended_at,
            messages,
            metadata: serde_json::Value::Object(metadata),
            source_path: path.to_path_buf(),
            model_name,
        })
    }

    fn write_session(
        &self,
        _session: &CanonicalSession,
        _opts: &WriteOptions,
    ) -> anyhow::Result<WrittenSession> {
        todo!("bd-1a2.1: Claude Code writer")
    }

    fn resume_command(&self, session_id: &str) -> String {
        format!("claude --resume {session_id}")
    }
}

// ---------------------------------------------------------------------------
// Helpers — tool call/result extraction from content blocks
// ---------------------------------------------------------------------------

/// Extract tool invocations from a content value (array of content blocks).
fn extract_tool_calls(content: Option<&serde_json::Value>) -> Vec<ToolCall> {
    let Some(serde_json::Value::Array(blocks)) = content else {
        return vec![];
    };
    blocks
        .iter()
        .filter_map(|block| {
            let obj = block.as_object()?;
            if obj.get("type")?.as_str()? != "tool_use" {
                return None;
            }
            Some(ToolCall {
                id: obj.get("id").and_then(|v| v.as_str()).map(String::from),
                name: obj
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                arguments: obj.get("input").cloned().unwrap_or(serde_json::Value::Null),
            })
        })
        .collect()
}

/// Extract tool results from a content value (array of content blocks).
fn extract_tool_results(content: Option<&serde_json::Value>) -> Vec<ToolResult> {
    let Some(serde_json::Value::Array(blocks)) = content else {
        return vec![];
    };
    blocks
        .iter()
        .filter_map(|block| {
            let obj = block.as_object()?;
            if obj.get("type")?.as_str()? != "tool_result" {
                return None;
            }
            let text = obj
                .get("content")
                .and_then(|v| v.as_str())
                .or_else(|| obj.get("output").and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_string();
            Some(ToolResult {
                call_id: obj
                    .get("tool_use_id")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                content: text,
                is_error: obj
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::project_dir_key;
    use std::path::Path;

    #[test]
    fn project_dir_key_matches_observed_workspace_mapping() {
        let got = project_dir_key(Path::new("/data/projects/cross_agent_sessions_resumer"));
        assert_eq!(got, "-data-projects-cross-agent-sessions-resumer");
    }

    #[test]
    fn project_dir_key_replaces_dots_underscores_and_slashes() {
        let got = project_dir_key(Path::new("/data/projects/jeffreys-skills.md"));
        assert_eq!(got, "-data-projects-jeffreys-skills-md");
    }

    #[test]
    fn project_dir_key_handles_simple_home_paths() {
        let got = project_dir_key(Path::new("/home/ubuntu"));
        assert_eq!(got, "-home-ubuntu");
    }
}
