//! Canonical session model — the IR (intermediate representation) for casr.
//!
//! Every provider's native format is parsed into these types, and every
//! target format is generated from them. This is the Rosetta Stone of
//! cross-provider session conversion.
//!
//! # CASS heritage
//!
//! These types are adapted from CASS (`coding_agent_session_search/src/model/types.rs`).
//!
//! **Naming difference:** CASS uses `Agent` for the assistant role variant;
//! casr uses `Assistant`, which matches the convention used by Claude, Codex,
//! and most LLM APIs. The [`normalize_role`] helper maps `"agent"` →
//! [`MessageRole::Assistant`] to bridge this.
//!
//! **Deliberately omitted from CASS** (not needed for session conversion):
//! - `approx_tokens` — per-message token data lives in `extra` if present.
//! - `source_id` / `origin_host` — casr works with local files only.
//! - `Snippet` type — code snippet extraction is a CASS indexing feature.
//! - Database `id` fields — casr has no database.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// A provider-agnostic representation of an AI coding agent session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalSession {
    /// Unique session identifier (provider-assigned or generated).
    pub session_id: String,
    /// Provider slug that originally created this session (e.g. `"claude-code"`).
    pub provider_slug: String,
    /// Project root directory, if known.
    pub workspace: Option<PathBuf>,
    /// Human-readable title (first user message or explicit title).
    pub title: Option<String>,
    /// Session start time as epoch milliseconds.
    pub started_at: Option<i64>,
    /// Session end time as epoch milliseconds.
    pub ended_at: Option<i64>,
    /// Ordered conversation messages.
    pub messages: Vec<CanonicalMessage>,
    /// Provider-specific extras that don't map to canonical fields.
    pub metadata: serde_json::Value,
    /// Filesystem path of the original session file.
    pub source_path: PathBuf,
    /// Convenience: most common model name in the session.
    pub model_name: Option<String>,
}

/// A single message in a canonical session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalMessage {
    /// Zero-based sequential index.
    pub idx: usize,
    /// Who sent this message.
    pub role: MessageRole,
    /// The textual content of the message.
    pub content: String,
    /// Message timestamp as epoch milliseconds.
    pub timestamp: Option<i64>,
    /// Model name or `"user"` or `"reasoning"`.
    pub author: Option<String>,
    /// Tool invocations made in this message.
    pub tool_calls: Vec<ToolCall>,
    /// Results returned from tool invocations.
    pub tool_results: Vec<ToolResult>,
    /// Provider-specific fields preserved for round-trip fidelity.
    pub extra: serde_json::Value,
}

/// The role of a message sender.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageRole {
    User,
    Assistant,
    Tool,
    System,
    Other(String),
}

/// A tool invocation within a message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: Option<String>,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// A tool result within a message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: Option<String>,
    pub content: String,
    pub is_error: bool,
}

// ---------------------------------------------------------------------------
// Helpers — ported/adapted from CASS connectors/mod.rs
// ---------------------------------------------------------------------------

/// Flatten heterogeneous content representations into a single string.
///
/// Handles all content shapes encountered across providers:
/// - String → returned as-is
/// - Array of `{type:"text", text:"…"}` blocks → concatenated
/// - Array of `{type:"input_text", text:"…"}` blocks (Codex) → concatenated
/// - Array of `{type:"tool_use", name:"…", input:{…}}` → rendered as `[Tool: name]`
/// - Array of plain strings → joined with newlines
/// - Object with `text` field (no `type`) → returns the text
/// - null / number / bool → empty string
pub fn flatten_content(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => {
            let mut parts = Vec::new();
            for item in arr {
                match item {
                    serde_json::Value::String(s) => parts.push(s.clone()),
                    serde_json::Value::Object(obj) => {
                        let type_field = obj.get("type").and_then(|v| v.as_str());
                        match type_field {
                            Some("text") | Some("input_text") => {
                                if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
                                    parts.push(text.to_string());
                                }
                            }
                            Some("tool_use") => {
                                let name = obj
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown");
                                let desc =
                                    obj.get("input")
                                        .and_then(|v| v.as_object())
                                        .and_then(|inp| {
                                            inp.get("description")
                                                .or_else(|| inp.get("file_path"))
                                                .and_then(|v| v.as_str())
                                        });
                                match desc {
                                    Some(d) => parts.push(format!("[Tool: {name} - {d}]")),
                                    None => parts.push(format!("[Tool: {name}]")),
                                }
                            }
                            _ => {
                                // Object without recognized type but with text field.
                                if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
                                    parts.push(text.to_string());
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            parts.join("\n")
        }
        serde_json::Value::Object(obj) => {
            // Single object without type but with text field.
            obj.get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        }
        _ => String::new(),
    }
}

/// Parse a timestamp value into epoch milliseconds.
///
/// Accepts:
/// - Integer: < 100 billion → seconds (× 1000); ≥ 100 billion → millis
/// - Float: treated as seconds → millis
/// - String of digits: same integer heuristic
/// - Float string (e.g. `"1700000000.123"`): seconds → millis
/// - ISO-8601 / RFC 3339 with timezone or Z suffix
///
/// Returns `None` for null, objects, arrays, or unparseable strings.
pub fn parse_timestamp(value: &serde_json::Value) -> Option<i64> {
    /// Threshold: values below this are seconds, at or above are milliseconds.
    const MILLIS_THRESHOLD: i64 = 100_000_000_000;

    match value {
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(if i < MILLIS_THRESHOLD { i * 1000 } else { i })
            } else {
                n.as_f64().map(|f| (f * 1000.0) as i64)
            }
        }
        serde_json::Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            // Try integer parse first.
            if let Ok(i) = s.parse::<i64>() {
                return Some(if i < MILLIS_THRESHOLD { i * 1000 } else { i });
            }
            // Try float parse.
            if let Ok(f) = s.parse::<f64>()
                && f.is_finite()
            {
                return Some((f * 1000.0) as i64);
            }
            // Try RFC 3339 / ISO-8601 with timezone.
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
                return Some(dt.timestamp_millis());
            }
            // Try common ISO-8601 variants.
            if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.fZ") {
                return Some(dt.and_utc().timestamp_millis());
            }
            if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%SZ") {
                return Some(dt.and_utc().timestamp_millis());
            }
            if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
                return Some(dt.and_utc().timestamp_millis());
            }
            if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
                return Some(dt.and_utc().timestamp_millis());
            }
            None
        }
        _ => None,
    }
}

/// Re-assign sequential idx values (0, 1, 2, …) after filtering/sorting.
pub fn reindex_messages(messages: &mut [CanonicalMessage]) {
    for (i, msg) in messages.iter_mut().enumerate() {
        msg.idx = i;
    }
}

/// Extract a title from message content: first line, truncated to `max_len`.
///
/// Returns an empty string for empty or whitespace-only input.
pub fn truncate_title(text: &str, max_len: usize) -> String {
    let first_line = text.lines().next().unwrap_or("").trim();
    if first_line.is_empty() {
        return String::new();
    }
    if first_line.len() <= max_len {
        first_line.to_string()
    } else {
        // Truncate at char boundary.
        let mut end = max_len;
        while !first_line.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}...", &first_line[..end])
    }
}

/// Map provider-specific role strings to canonical [`MessageRole`].
///
/// Case-insensitive matching. CASS uses `"agent"` for assistant; most
/// providers use `"assistant"` or `"model"`. Gemini CLI emits `"gemini"`
/// for assistant/model responses in current builds.
pub fn normalize_role(role_str: &str) -> MessageRole {
    match role_str.to_ascii_lowercase().as_str() {
        "user" => MessageRole::User,
        "assistant" | "model" | "agent" | "gemini" => MessageRole::Assistant,
        "tool" => MessageRole::Tool,
        "system" => MessageRole::System,
        other => MessageRole::Other(other.to_string()),
    }
}
