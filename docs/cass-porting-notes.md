# CASS Porting Notes

casr adapts connector and model logic from [coding_agent_session_search](https://github.com/Dicklesworthstone/coding_agent_session_search) (CASS). This document tracks what was ported, what changed, and why casr carries no runtime dependency on CASS.

## No-Runtime-Dependency Architecture

casr copies and adapts relevant CASS source code ("vendoring") rather than depending on the CASS crate at build time. Rationale:

1. **CASS is a search/indexing tool; casr is a conversion CLI.** Different optimization targets (throughput vs latency), different dependency trees.
2. **casr adds writers.** CASS only reads sessions; casr reads *and writes*. The writer code is entirely new.
3. **Decoupled release cycles.** CASS may make breaking changes to its model types that casr shouldn't be forced to absorb immediately.
4. **Smaller binary.** casr ships as a single static binary with no transitive CASS deps (SQLite, tantivy, etc.).

## Source Files Adapted

| CASS source | casr destination | What changed |
|-------------|-----------------|--------------|
| `src/model/types.rs` | `src/model.rs` | Subset of fields; `Agent` → `Assistant` role; dropped `approx_tokens`, `source_id`, `Snippet` |
| `src/connectors/claude_code.rs` | `src/providers/claude_code.rs` | Added writer; unified role/content cascades |
| `src/connectors/codex.rs` | `src/providers/codex.rs` | Added writer; retroactive `token_count` attachment |
| `src/connectors/gemini.rs` | `src/providers/gemini.rs` | Added writer; 3-strategy workspace extraction |
| `src/connectors/mod.rs` | `src/model.rs` (helpers) | `flatten_content`, `parse_timestamp`, `reindex_messages` ported as shared helpers |
| `src/sources/probe.rs` | `src/discovery.rs` | Adapted provider detection probes with env var overrides |

## Behavioral Deltas

- **Role naming:** CASS uses `Agent`; casr uses `Assistant`. `normalize_role()` maps `"agent"` → `Assistant`.
- **Timestamp heuristic:** Same 100-billion threshold for seconds-vs-millis detection.
- **Workspace extraction (Gemini):** Same 3-strategy cascade; casr adds writer that reproduces the directory hash.
- **External ID (Claude Code):** Same filename-based derivation (not `sessionId` field).

## Resume/Storage Reverse-Engineering Notes (2026-02-09)

These findings were validated against real local provider state in `~/.claude`, `~/.codex`, and
`~/.gemini`, plus connector code in CASS.

### Claude Code

- Projects root: `~/.claude/projects`.
- Project directory key is a sanitized workspace path:
  - non-alphanumeric characters -> `-`
  - case preserved
  - example: `/data/projects/cross_agent_sessions_resumer` ->
    `-data-projects-cross-agent-sessions-resumer`.
- Session file: `<project-key>/<session-id>.jsonl`.
- `sessionId` in JSONL entries matches the UUID filename used by `claude --resume`.

### Codex

- Sessions root: `~/.codex/sessions/YYYY/MM/DD/`.
- Filename convention:
  `rollout-YYYY-MM-DDThh-mm-ss-<session-uuid>.jsonl`.
- `session_meta.payload.id` matches the UUID suffix in the filename.
- `codex resume --help` confirms `SESSION_ID` accepts UUIDs (or thread names), with UUID precedence.
- casr lookup strategy should accept:
  - explicit relative rollout paths,
  - UUID filename suffix matches,
  - `session_meta.payload.id` body matches.

### Gemini CLI

- Sessions root: `~/.gemini/tmp/<projectHash>/chats/session-*.json`.
- `projectHash` equals `SHA256(<absolute workspace path>)` (lowercase hex).
- Filename convention:
  `session-YYYY-MM-DDThh-mm-<sessionId-prefix8>.json`.
- `gemini --resume` is index/latest-oriented for the current workspace, but files still contain stable
  full `sessionId` UUIDs used by casr conversion.
- Assistant messages are emitted as `type: "gemini"` in current real sessions (legacy examples may use
  `model`), so both must map to canonical assistant role.

## License

CASS is MIT-licensed. Adapted code retains MIT license compliance per the terms of the original license.
