//! Codex provider — reads/writes JSONL sessions under `~/.codex/sessions/`.
//!
//! Session files: `YYYY/MM/DD/rollout-N.jsonl`
//! Resume command: `codex --resume <session-id>`
//!
//! ## JSONL format (modern envelope)
//!
//! Each line: `{ "type": "session_meta|response_item|event_msg", "timestamp": …, "payload": {…} }`
//!
//! - `session_meta` → workspace (`payload.cwd`), session ID (`payload.id`).
//! - `response_item` → main conversational messages (`payload.role`, `payload.content`).
//! - `event_msg` → sub-typed: `user_message`, `agent_reasoning` (conversational);
//!   `token_count`, `turn_aborted` (non-conversational).
//!
//! ## Legacy JSON format
//!
//! Single object: `{ "session": { "id", "cwd" }, "items": [ {role, content, timestamp} ] }`

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Context;
use tracing::{debug, trace, warn};
use walkdir::WalkDir;

use crate::discovery::DetectionResult;
use crate::model::{
    CanonicalMessage, CanonicalSession, MessageRole, ToolCall, ToolResult, flatten_content,
    normalize_role, parse_timestamp, reindex_messages, truncate_title,
};
use crate::providers::{Provider, WriteOptions, WrittenSession};

/// Codex provider implementation.
pub struct Codex;

/// Generate the Codex rollout file path for a new session.
///
/// Convention: `~/.codex/sessions/YYYY/MM/DD/rollout-YYYY-MM-DDThh-mm-ss-<session-id>.jsonl`
///
/// The session ID is a ULID (timestamp-prefixed UUID).
pub fn rollout_path(
    sessions_dir: &Path,
    session_id: &str,
    now: &chrono::DateTime<chrono::Utc>,
) -> PathBuf {
    let date_dir = now.format("%Y/%m/%d").to_string();
    let ts_part = now.format("%Y-%m-%dT%H-%M-%S").to_string();
    let filename = format!("rollout-{ts_part}-{session_id}.jsonl");
    sessions_dir.join(date_dir).join(filename)
}

impl Codex {
    /// Root directory for Codex data.
    /// Respects `CODEX_HOME` env var override.
    fn home_dir() -> Option<PathBuf> {
        if let Ok(home) = std::env::var("CODEX_HOME") {
            return Some(PathBuf::from(home));
        }
        dirs::home_dir().map(|h| h.join(".codex"))
    }

    /// Sessions directory where rollout files live.
    fn sessions_dir() -> Option<PathBuf> {
        Self::home_dir().map(|h| h.join("sessions"))
    }
}

impl Provider for Codex {
    fn name(&self) -> &str {
        "Codex"
    }

    fn slug(&self) -> &str {
        "codex"
    }

    fn cli_alias(&self) -> &str {
        "cod"
    }

    fn detect(&self) -> DetectionResult {
        let mut evidence = Vec::new();
        let mut installed = false;

        if which::which("codex").is_ok() {
            evidence.push("codex binary found in PATH".to_string());
            installed = true;
        }

        if let Some(home) = Self::home_dir()
            && home.is_dir()
        {
            evidence.push(format!("{} exists", home.display()));
            installed = true;
        }

        trace!(provider = "codex", ?evidence, installed, "detection");
        DetectionResult {
            installed,
            version: None,
            evidence,
        }
    }

    fn session_roots(&self) -> Vec<PathBuf> {
        match Self::sessions_dir() {
            Some(dir) if dir.is_dir() => vec![dir],
            _ => vec![],
        }
    }

    fn owns_session(&self, session_id: &str) -> Option<PathBuf> {
        let sessions_dir = Self::sessions_dir()?;
        if !sessions_dir.is_dir() {
            return None;
        }

        // Codex session IDs can be:
        // 1. A UUID embedded in the file content
        // 2. A relative path like "2026/02/06/rollout-1"
        //
        // Strategy: check if session_id is a relative path first,
        // then scan files for matching UUIDs.

        // Try as relative path (with or without extension).
        let as_path = sessions_dir.join(session_id);
        for ext in ["", ".jsonl", ".json"] {
            let candidate = if ext.is_empty() {
                as_path.clone()
            } else {
                as_path.with_extension(&ext[1..])
            };
            if candidate.is_file() {
                debug!(path = %candidate.display(), "found Codex session by path");
                return Some(candidate);
            }
        }

        // Scan rollout files recursively.
        for entry in WalkDir::new(&sessions_dir)
            .max_depth(5)
            .into_iter()
            .filter_map(Result::ok)
        {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && (name.starts_with("rollout-")
                    && (name.ends_with(".jsonl") || name.ends_with(".json")))
                && path.is_file()
            {
                // Check if the relative path (minus extension) matches session_id.
                if let Ok(rel) = path.strip_prefix(&sessions_dir) {
                    let rel_str = rel.with_extension("").to_string_lossy().to_string();
                    if rel_str == session_id {
                        debug!(path = %path.display(), "found Codex session");
                        return Some(path.to_path_buf());
                    }
                }

                // Match by UUID suffix embedded in rollout filename:
                // rollout-YYYY-MM-DDThh-mm-ss-<session-id>.jsonl
                let name_no_ext = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default();
                if name_no_ext.ends_with(session_id) {
                    debug!(path = %path.display(), "found Codex session by filename suffix");
                    return Some(path.to_path_buf());
                }

                // Fallback: inspect `session_meta.payload.id` in file body.
                if session_meta_id(path).as_deref() == Some(session_id) {
                    debug!(path = %path.display(), "found Codex session by session_meta payload.id");
                    return Some(path.to_path_buf());
                }
            }
        }
        None
    }

    fn read_session(&self, path: &Path) -> anyhow::Result<CanonicalSession> {
        debug!(path = %path.display(), "reading Codex session");

        // Try JSONL first, fall back to legacy JSON.
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;

        // Detect format: if first non-whitespace char is '{' and the file has
        // multiple JSON lines, it's JSONL. If the top-level parse yields a
        // "session" or "items" key, it's legacy JSON.
        let trimmed = content.trim_start();
        if let Some(first_line) = trimmed.lines().next()
            && let Ok(obj) = serde_json::from_str::<serde_json::Value>(first_line)
            && (obj.get("session").is_some() || obj.get("items").is_some())
        {
            return self.read_legacy_json(path, &content);
        }

        self.read_jsonl(path, &content)
    }

    fn write_session(
        &self,
        _session: &CanonicalSession,
        _opts: &WriteOptions,
    ) -> anyhow::Result<WrittenSession> {
        todo!("bd-1a2.2: Codex writer")
    }

    fn resume_command(&self, session_id: &str) -> String {
        format!("codex --resume {session_id}")
    }
}

// ---------------------------------------------------------------------------
// JSONL / legacy JSON parsing
// ---------------------------------------------------------------------------

impl Codex {
    /// Parse modern JSONL envelope format.
    fn read_jsonl(&self, path: &Path, content: &str) -> anyhow::Result<CanonicalSession> {
        let reader = BufReader::new(content.as_bytes());

        let mut session_id: Option<String> = None;
        let mut workspace: Option<PathBuf> = None;
        let mut started_at: Option<i64> = None;
        let mut ended_at: Option<i64> = None;
        let mut messages: Vec<CanonicalMessage> = Vec::new();
        let mut skipped: usize = 0;
        let mut line_num: usize = 0;

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
            let envelope: serde_json::Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    warn!(line = line_num, error = %e, "skipping malformed JSON line");
                    skipped += 1;
                    continue;
                }
            };

            let event_type = envelope.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let payload = envelope.get("payload");

            // Extract timestamp from envelope level.
            let ts = envelope.get("timestamp").and_then(parse_timestamp);
            if let Some(t) = ts {
                started_at = Some(started_at.map_or(t, |s: i64| s.min(t)));
                ended_at = Some(ended_at.map_or(t, |e: i64| e.max(t)));
            }

            match event_type {
                "session_meta" => {
                    if let Some(p) = payload {
                        if session_id.is_none() {
                            session_id = p.get("id").and_then(|v| v.as_str()).map(String::from);
                        }
                        if workspace.is_none() {
                            workspace = p.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from);
                        }
                    }
                }
                "response_item" => {
                    if let Some(p) = payload {
                        let role_str = p
                            .get("role")
                            .and_then(|v| v.as_str())
                            .unwrap_or("assistant");
                        let role = normalize_role(role_str);

                        let content_val = p.get("content");
                        let text = content_val.map(flatten_content).unwrap_or_default();
                        if text.trim().is_empty() {
                            trace!(line = line_num, "skipping empty response_item");
                            continue;
                        }

                        let tool_calls = codex_extract_tool_calls(content_val);
                        let tool_results = codex_extract_tool_results(content_val);

                        messages.push(CanonicalMessage {
                            idx: 0,
                            role,
                            content: text,
                            timestamp: ts,
                            author: None,
                            tool_calls,
                            tool_results,
                            extra: envelope,
                        });
                    }
                }
                "event_msg" => {
                    if let Some(p) = payload {
                        let sub_type = p.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        match sub_type {
                            "user_message" => {
                                let text = p
                                    .get("message")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                if !text.trim().is_empty() {
                                    messages.push(CanonicalMessage {
                                        idx: 0,
                                        role: MessageRole::User,
                                        content: text,
                                        timestamp: ts,
                                        author: None,
                                        tool_calls: vec![],
                                        tool_results: vec![],
                                        extra: envelope,
                                    });
                                }
                            }
                            "agent_reasoning" => {
                                let text = p
                                    .get("text")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                if !text.trim().is_empty() {
                                    messages.push(CanonicalMessage {
                                        idx: 0,
                                        role: MessageRole::Assistant,
                                        content: text,
                                        timestamp: ts,
                                        author: Some("reasoning".to_string()),
                                        tool_calls: vec![],
                                        tool_results: vec![],
                                        extra: envelope,
                                    });
                                }
                            }
                            _ => {
                                trace!(
                                    line = line_num,
                                    sub_type, "skipping non-conversational event_msg"
                                );
                            }
                        }
                    }
                }
                _ => {
                    trace!(line = line_num, event_type, "skipping unknown event type");
                }
            }
        }

        reindex_messages(&mut messages);
        self.build_session(
            path, session_id, workspace, started_at, ended_at, messages, skipped,
        )
    }

    /// Parse legacy single-JSON format: `{ "session": {…}, "items": […] }`.
    fn read_legacy_json(&self, path: &Path, content: &str) -> anyhow::Result<CanonicalSession> {
        let root: serde_json::Value = serde_json::from_str(content)
            .with_context(|| format!("failed to parse legacy JSON {}", path.display()))?;

        let session_obj = root.get("session");
        let session_id = session_obj
            .and_then(|s| s.get("id"))
            .and_then(|v| v.as_str())
            .map(String::from);
        let workspace = session_obj
            .and_then(|s| s.get("cwd"))
            .and_then(|v| v.as_str())
            .map(PathBuf::from);

        let items = root
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut messages = Vec::new();
        let mut started_at: Option<i64> = None;
        let mut ended_at: Option<i64> = None;

        for item in &items {
            let role_str = item
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("assistant");
            let role = normalize_role(role_str);

            let text = item.get("content").map(flatten_content).unwrap_or_default();
            if text.trim().is_empty() {
                continue;
            }

            let ts = item.get("timestamp").and_then(parse_timestamp);
            if let Some(t) = ts {
                started_at = Some(started_at.map_or(t, |s: i64| s.min(t)));
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
                extra: item.clone(),
            });
        }

        reindex_messages(&mut messages);
        self.build_session(
            path, session_id, workspace, started_at, ended_at, messages, 0,
        )
    }

    /// Assemble the final `CanonicalSession` from parsed data.
    #[expect(
        clippy::too_many_arguments,
        reason = "internal builder; clarity > refactoring"
    )]
    fn build_session(
        &self,
        path: &Path,
        session_id: Option<String>,
        workspace: Option<PathBuf>,
        started_at: Option<i64>,
        ended_at: Option<i64>,
        messages: Vec<CanonicalMessage>,
        skipped: usize,
    ) -> anyhow::Result<CanonicalSession> {
        // Derive session ID from relative path if not in content.
        let session_id = session_id.unwrap_or_else(|| {
            if let Some(sessions_dir) = Self::sessions_dir()
                && let Ok(rel) = path.strip_prefix(&sessions_dir)
            {
                return rel.with_extension("").to_string_lossy().to_string();
            }
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string()
        });

        let title = messages
            .iter()
            .find(|m| m.role == MessageRole::User)
            .map(|m| truncate_title(&m.content, 100));

        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "source".into(),
            serde_json::Value::String("codex".to_string()),
        );

        debug!(
            session_id,
            messages = messages.len(),
            skipped,
            "Codex session parsed"
        );

        Ok(CanonicalSession {
            session_id,
            provider_slug: "codex".to_string(),
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
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract tool calls from Codex content blocks.
fn codex_extract_tool_calls(content: Option<&serde_json::Value>) -> Vec<ToolCall> {
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

/// Extract tool results from Codex content blocks.
fn codex_extract_tool_results(content: Option<&serde_json::Value>) -> Vec<ToolResult> {
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
            Some(ToolResult {
                call_id: obj
                    .get("tool_use_id")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                content: obj
                    .get("content")
                    .and_then(|v| v.as_str())
                    .or_else(|| obj.get("output").and_then(|v| v.as_str()))
                    .unwrap_or("")
                    .to_string(),
                is_error: obj
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            })
        })
        .collect()
}

/// Extract `session_meta.payload.id` from a Codex rollout file.
fn session_meta_id(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    for line in reader.lines().map_while(Result::ok).take(64) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let envelope: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if envelope.get("type").and_then(|v| v.as_str()) == Some("session_meta") {
            return envelope
                .pointer("/payload/id")
                .and_then(|v| v.as_str())
                .map(ToString::to_string);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::rollout_path;
    use chrono::{TimeZone, Utc};
    use std::path::Path;

    #[test]
    fn rollout_path_includes_date_hierarchy_and_uuid_suffix() {
        let now = Utc
            .with_ymd_and_hms(2026, 2, 9, 6, 7, 8)
            .single()
            .expect("valid timestamp");
        let path = rollout_path(
            Path::new("/tmp/codex/sessions"),
            "019c40fd-3c51-7621-a418-68203585f589",
            &now,
        );
        let path_str = path.to_string_lossy();
        assert!(
            path_str.ends_with(
                "2026/02/09/rollout-2026-02-09T06-07-08-019c40fd-3c51-7621-a418-68203585f589.jsonl"
            ),
            "{path_str}"
        );
    }
}
