//! The Antigravity CLI (agy) transcript format.
//!
//! Antigravity stores conversation sessions under `~/.gemini/antigravity-cli/brain/<uuid>/`.
//! The transcript is recorded as JSONL in:
//! `.system_generated/logs/transcript.jsonl` (and `transcript_full.jsonl`).
//!
//! Each step in the transcript contains:
//! - `step_index`: Sequential step counter (0, 1, 2, ...)
//! - `source`: `"USER_EXPLICIT"`, `"MODEL"`, `"SYSTEM"`
//! - `type`: `"USER_INPUT"`, `"PLANNER_RESPONSE"`, `"GENERIC"`
//! - `status`: `"DONE"`, `"ERROR"`
//! - `created_at`: ISO 8601 timestamp string
//! - `content`: Text payload
//! - `thinking`: Agent reasoning trace
//! - `tool_calls`: Array of tool calls (e.g. `run_command`, `read_file`, `invoke_subagent`)
//! - `input_tokens`, `output_tokens`, `cache_read_tokens`

use super::dropped::DroppedRecords;
use super::{EarlierTurns, SessionFormat, SessionHeader, SessionProjection, at_own_lines};
use crate::error::Result;
use crate::history::Source;
use crate::log_entry::{
    AssistantMessage, ContentBlock, LogEntry, TokenUsage, Tool, UserContent, UserMessage,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub struct AgyTranscriptFormat;

pub static AGY_TRANSCRIPT: AgyTranscriptFormat = AgyTranscriptFormat;

pub const AGY_TRANSCRIPT_FILE: &str = "transcript.jsonl";
pub const AGY_TRANSCRIPT_FULL_FILE: &str = "transcript_full.jsonl";

/// One transcript line. Fields the reader does not use (`source`, `status`)
/// are left to serde to skip.
#[derive(Debug, Deserialize)]
struct AgyRecord {
    step_index: usize,
    #[serde(rename = "type")]
    record_type: String,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<AgyToolCall>>,
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_read_tokens: Option<u64>,
}

#[derive(Debug, Deserialize, Clone)]
struct AgyToolCall {
    name: String,
    #[serde(default)]
    args: Value,
}

impl SessionFormat for AgyTranscriptFormat {
    fn parse_transcript_counting(
        &self,
        path: &Path,
        _dropped: DroppedRecords,
    ) -> Result<Option<SessionProjection>> {
        // If the path points to transcript.jsonl, prefer transcript_full.jsonl if available
        let read_path = if path.file_name() == Some(OsStr::new(AGY_TRANSCRIPT_FILE)) {
            let full_path = path.with_file_name(AGY_TRANSCRIPT_FULL_FILE);
            if full_path.is_file() {
                full_path
            } else {
                path.to_path_buf()
            }
        } else {
            path.to_path_buf()
        };

        let file = match File::open(&read_path) {
            Ok(file) => file,
            Err(_) => return Ok(None),
        };

        let reader = BufReader::new(file);
        let mut entries: Vec<(usize, LogEntry)> = Vec::new();
        let mut first_timestamp = None;
        let mut first_user_prompt = None;
        let mut detected_cwd = None;
        // A step with N tool calls is followed by N result steps, in call
        // order, so each result takes the oldest call still waiting.
        let mut pending_tool_calls: VecDeque<String> = VecDeque::new();
        let model = session_model(path);

        for (line_idx, line_result) in reader.lines().enumerate() {
            let line_num = line_idx + 1;
            let line = match line_result {
                Ok(l) => l,
                Err(_) => continue,
            };

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let record: AgyRecord = match serde_json::from_str(trimmed) {
                Ok(r) => r,
                Err(_) => {
                    if line_idx == 0 {
                        // First line is not valid Agy JSONL
                        return Ok(None);
                    }
                    continue;
                }
            };

            if first_timestamp.is_none() && record.created_at.is_some() {
                first_timestamp = record.created_at.clone();
            }

            match record.record_type.as_str() {
                "USER_INPUT" => {
                    let raw_content = record.content.unwrap_or_default();
                    if detected_cwd.is_none() {
                        detected_cwd = extract_cwd(&raw_content);
                    }
                    let cleaned = clean_user_content(&raw_content);
                    if first_user_prompt.is_none() && !cleaned.is_empty() {
                        first_user_prompt = Some(cleaned.clone());
                    }

                    entries.push((
                        line_num,
                        LogEntry::User {
                            message: UserMessage {
                                role: "user".to_string(),
                                content: UserContent::String(cleaned),
                            },
                            timestamp: record.created_at,
                            uuid: None,
                            cwd: detected_cwd.as_ref().map(|p| p.display().to_string()),
                            parent_tool_use_id: None,
                            source_tool_use_id: None,
                            usage: None,
                        },
                    ));
                }
                "PLANNER_RESPONSE" => {
                    let mut blocks = Vec::new();

                    if let Some(thinking) = record.thinking
                        && !thinking.trim().is_empty()
                    {
                        blocks.push(ContentBlock::Thinking {
                            thinking,
                            signature: String::new(),
                        });
                    }

                    if let Some(tool_calls) = record.tool_calls {
                        for (call_idx, call) in tool_calls.into_iter().enumerate() {
                            let tool_id = format!("call_{}_{}", record.step_index, call_idx);
                            pending_tool_calls.push_back(tool_id.clone());
                            let (canonical_tool, canonical_input) = map_tool(&call.name, call.args);
                            blocks.push(ContentBlock::ToolUse {
                                id: tool_id,
                                name: call.name,
                                tool: canonical_tool,
                                input: canonical_input,
                            });
                        }
                    }

                    if let Some(content) = record.content
                        && !content.trim().is_empty()
                    {
                        blocks.push(ContentBlock::Text { text: content });
                    }

                    if !blocks.is_empty() {
                        let usage =
                            if record.input_tokens.is_some() || record.output_tokens.is_some() {
                                Some(TokenUsage {
                                    input_tokens: record.input_tokens.unwrap_or(0),
                                    output_tokens: record.output_tokens.unwrap_or(0),
                                    cache_read_input_tokens: record.cache_read_tokens.unwrap_or(0),
                                    ..Default::default()
                                })
                            } else {
                                None
                            };

                        entries.push((
                            line_num,
                            LogEntry::Assistant {
                                message: AssistantMessage {
                                    role: "assistant".to_string(),
                                    content: blocks,
                                    model: model.clone(),
                                    usage,
                                    id: None,
                                },
                                agent: Some("Antigravity".to_owned()),
                                timestamp: record.created_at,
                                uuid: None,
                                parent_tool_use_id: None,
                            },
                        ));
                    }
                }
                "GENERIC" => {
                    // Tool output result
                    if let Some(content) = record.content {
                        let tool_use_id = pending_tool_calls
                            .pop_front()
                            .unwrap_or_else(|| format!("call_{}", record.step_index));
                        entries.push((
                            line_num,
                            LogEntry::User {
                                message: UserMessage {
                                    role: "user".to_string(),
                                    content: UserContent::Blocks(vec![ContentBlock::ToolResult {
                                        tool_use_id,
                                        content: Some(Value::String(content)),
                                        standalone_tool_name: None,
                                    }]),
                                },
                                timestamp: record.created_at,
                                uuid: None,
                                cwd: None,
                                parent_tool_use_id: None,
                                source_tool_use_id: None,
                                usage: None,
                            },
                        ));
                    }
                }
                _ => {}
            }
        }

        if entries.is_empty() {
            return Ok(None);
        }

        let session_id = session_id_of(path).unwrap_or_else(|| "unknown".to_string());
        let title = first_user_prompt.as_ref().map(|p| truncate_title(p));

        if let Some(ref t) = title {
            entries.insert(0, (0, LogEntry::Summary { summary: t.clone() }));
        }

        Ok(Some(SessionProjection {
            source: Source::Agy,
            header: SessionHeader {
                version: 1,
                id: session_id,
                timestamp: first_timestamp.unwrap_or_default(),
                cwd: detected_cwd.unwrap_or_default(),
                thread_label: None,
                subagent_identity: Default::default(),
            },
            title,
            entries: at_own_lines(entries),
            leaf_id: None,
            malformed_lines: Vec::new(),
            dropped: DroppedRecords::default(),
            earlier_turns: EarlierTurns::Read,
        }))
    }
}

/// Extract session UUID from transcript path (`.../brain/<uuid>/.system_generated/logs/transcript*.jsonl`).
pub fn session_id_of(path: &Path) -> Option<String> {
    let mut current = path.parent()?; // logs
    if current.file_name() == Some(OsStr::new("logs")) {
        current = current.parent()?; // .system_generated
    }
    if current.file_name() == Some(OsStr::new(".system_generated")) {
        current = current.parent()?; // <uuid>
    }
    current.file_name()?.to_str().map(str::to_owned)
}

/// Strip XML metadata tags from the user request to produce clean display text.
fn clean_user_content(raw: &str) -> String {
    if let Some(start) = raw.find("<USER_REQUEST>") {
        let after = &raw[start + "<USER_REQUEST>".len()..];
        if let Some(end) = after.find("</USER_REQUEST>") {
            return after[..end].trim().to_string();
        }
    }
    raw.trim().to_string()
}

/// Look for working directory in prompt metadata.
fn extract_cwd(raw: &str) -> Option<PathBuf> {
    if let Some(start) = raw.find("Command Working Directory: ") {
        let after = &raw[start + "Command Working Directory: ".len()..];
        let end = after.find('\n').unwrap_or(after.len());
        let path_str = after[..end].trim();
        if !path_str.is_empty() {
            return Some(PathBuf::from(path_str));
        }
    }
    if let Some(start) = raw.find("Active Workspaces:\n- ") {
        let after = &raw[start + "Active Workspaces:\n- ".len()..];
        let end = after.find('\n').unwrap_or(after.len());
        let path_str = after[..end].trim();
        if !path_str.is_empty() {
            return Some(PathBuf::from(path_str));
        }
    }
    None
}

/// Normalize tool call arguments and map tool names to standard Tool enum.
fn map_tool(name: &str, args: Value) -> (Tool, Value) {
    match name {
        "run_command" => {
            let mut input = args.clone();
            if let Some(cmd) = args.get("CommandLine").and_then(|v| v.as_str())
                && let Some(obj) = input.as_object_mut()
            {
                obj.insert("command".to_string(), Value::String(cmd.to_string()));
            }
            (Tool::Shell, input)
        }
        "read_file" => {
            let mut input = args.clone();
            if let Some(p) = args.get("Path").and_then(|v| v.as_str())
                && let Some(obj) = input.as_object_mut()
            {
                obj.insert("file_path".to_string(), Value::String(p.to_string()));
            }
            (Tool::Read, input)
        }
        "write_file" => {
            let mut input = args.clone();
            if let Some(p) = args.get("Path").and_then(|v| v.as_str())
                && let Some(obj) = input.as_object_mut()
            {
                obj.insert("file_path".to_string(), Value::String(p.to_string()));
            }
            (Tool::Write, input)
        }
        "edit_file" => {
            let mut input = args.clone();
            if let Some(p) = args.get("Path").and_then(|v| v.as_str())
                && let Some(obj) = input.as_object_mut()
            {
                obj.insert("file_path".to_string(), Value::String(p.to_string()));
            }
            (Tool::Edit, input)
        }
        "view_file" => (Tool::Read, with_file_path(args, "AbsolutePath")),
        "write_to_file" => (Tool::Write, with_file_path(args, "TargetFile")),
        "replace_file_content" | "multi_replace_file_content" => {
            (Tool::Edit, with_file_path(args, "TargetFile"))
        }
        "read_url_content" => {
            let mut input = args.clone();
            if let Some(url) = args.get("Url").and_then(|v| v.as_str())
                && let Some(obj) = input.as_object_mut()
            {
                obj.insert("url".to_string(), Value::String(url.to_string()));
            }
            (Tool::WebFetch, input)
        }
        "schedule" => (Tool::Wait, args),
        "invoke_subagent" => (Tool::Agent, args),
        "send_message" => (Tool::AgentMessage, args),
        "search_web" => (Tool::WebSearch, args),
        _ => (Tool::Other, args),
    }
}

/// Copy the path argument `key` to the canonical `file_path`.
fn with_file_path(args: Value, key: &str) -> Value {
    let mut input = args.clone();
    if let Some(path) = args.get(key).and_then(|v| v.as_str())
        && let Some(obj) = input.as_object_mut()
    {
        obj.insert("file_path".to_string(), Value::String(path.to_string()));
    }
    input
}

/// The model a session ran on. The transcript never names it; agy keeps it in
/// the per-conversation database, `<data>/conversations/<id>.db`, inside the
/// protobuf blobs of `gen_metadata`. Read the newest generation's blob and
/// take the first model-shaped word in it. `None` when the database or a
/// model name is absent, rather than a guess.
pub fn session_model(transcript: &Path) -> Option<String> {
    let id = session_id_of(transcript)?;
    let brain = transcript.ancestors().find(|dir| {
        dir.file_name()
            .is_some_and(|name| name.to_str() == Some(&id))
    })?;
    let data_dir = brain.parent()?.parent()?;
    let database = data_dir.join("conversations").join(format!("{id}.db"));
    if !database.is_file() {
        return None;
    }
    let connection = rusqlite::Connection::open_with_flags(
        database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let mut statement = connection
        .prepare("SELECT data FROM gen_metadata ORDER BY idx DESC")
        .ok()?;
    let blobs = statement
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .ok()?;
    blobs.flatten().find_map(|blob| model_name_in(&blob))
}

/// The first word in `blob` shaped like a model id, such as
/// `claude-sonnet-5-5-medium` or `gemini-3-pro`.
fn model_name_in(blob: &[u8]) -> Option<String> {
    const PREFIXES: [&str; 3] = ["claude-", "gemini-", "gpt-"];
    blob.split(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_')))
        .filter_map(|word| std::str::from_utf8(word).ok())
        .find(|word| PREFIXES.iter().any(|prefix| word.starts_with(prefix)))
        .map(str::to_owned)
}

fn truncate_title(prompt: &str) -> String {
    let first_line = prompt.lines().next().unwrap_or(prompt).trim();
    if first_line.chars().count() > 80 {
        let prefix: String = first_line.chars().take(77).collect();
        format!("{prefix}...")
    } else {
        first_line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_user_content_extracts_request() {
        let raw = "<USER_REQUEST>\nfix the bug\n</USER_REQUEST>\n<ADDITIONAL_METADATA>";
        assert_eq!(clean_user_content(raw), "fix the bug");
    }

    #[test]
    fn extract_cwd_finds_working_directory() {
        let raw = "Some preamble\nCommand Working Directory: /path/to/project\nother text";
        assert_eq!(extract_cwd(raw), Some(PathBuf::from("/path/to/project")));
    }

    #[test]
    fn session_id_of_extracts_uuid() {
        let path = Path::new(
            "/Users/user/.gemini/antigravity-cli/brain/5c6625aa-d86f-481f-bff8-f2d3327e80df/.system_generated/logs/transcript.jsonl",
        );
        assert_eq!(
            session_id_of(path),
            Some("5c6625aa-d86f-481f-bff8-f2d3327e80df".to_string())
        );
    }

    #[test]
    fn truncate_title_handles_multibyte_utf8() {
        let long_japanese = "これはテストのための非常に長いプロンプト文字列です。日本語のマルチバイト文字が含まれていますが、境界でパニックを起こしてはなりません。さらにテキストを続けます。さらに長くします。";
        let truncated = truncate_title(long_japanese);
        assert!(truncated.ends_with("..."));
        assert_eq!(truncated.chars().count(), 80);
    }

    fn transcript_with(lines: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir
            .path()
            .join("brain/00000000-0000-4000-8000-000000000001/.system_generated/logs");
        std::fs::create_dir_all(&logs).unwrap();
        let path = logs.join(AGY_TRANSCRIPT_FILE);
        std::fs::write(&path, lines.join("\n")).unwrap();
        (dir, path)
    }

    fn parse(path: &Path) -> SessionProjection {
        AGY_TRANSCRIPT
            .parse_transcript_counting(path, DroppedRecords::default())
            .unwrap()
            .unwrap()
    }

    /// A step with several tool calls is followed by one result per call, in
    /// call order; each result belongs to its own call.
    #[test]
    fn each_result_pairs_with_its_own_call_in_a_multi_call_step() {
        let (_dir, path) = transcript_with(&[
            r#"{"step_index":0,"type":"USER_INPUT","content":"go"}"#,
            r#"{"step_index":1,"type":"PLANNER_RESPONSE","tool_calls":[{"name":"run_command","args":{"CommandLine":"a"}},{"name":"run_command","args":{"CommandLine":"b"}}]}"#,
            r#"{"step_index":2,"type":"GENERIC","content":"result-a"}"#,
            r#"{"step_index":3,"type":"GENERIC","content":"result-b"}"#,
        ]);
        let projection = parse(&path);
        let results: Vec<(String, String)> = projection
            .entries
            .iter()
            .filter_map(|(_, entry)| match entry {
                LogEntry::User {
                    message:
                        UserMessage {
                            content: UserContent::Blocks(blocks),
                            ..
                        },
                    ..
                } => blocks.iter().find_map(|block| match block {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content: Some(Value::String(text)),
                        ..
                    } => Some((tool_use_id.clone(), text.clone())),
                    _ => None,
                }),
                _ => None,
            })
            .collect();
        assert_eq!(
            results,
            vec![
                ("call_1_0".to_string(), "result-a".to_string()),
                ("call_1_1".to_string(), "result-b".to_string()),
            ]
        );
    }

    #[test]
    fn map_tool_covers_the_tools_agy_has_shipped() {
        let cases = [
            ("view_file", Tool::Read),
            ("read_file", Tool::Read),
            ("write_to_file", Tool::Write),
            ("replace_file_content", Tool::Edit),
            ("read_url_content", Tool::WebFetch),
            ("schedule", Tool::Wait),
            ("invoke_subagent", Tool::Agent),
        ];
        for (name, tool) in cases {
            assert_eq!(map_tool(name, Value::Null).0, tool, "{name}");
        }
        let (_, input) = map_tool(
            "view_file",
            serde_json::json!({"AbsolutePath": "/tmp/a.rs"}),
        );
        assert_eq!(input["file_path"], "/tmp/a.rs");
        let (_, input) = map_tool(
            "read_url_content",
            serde_json::json!({"Url": "https://x.y"}),
        );
        assert_eq!(input["url"], "https://x.y");
    }

    #[test]
    fn model_name_in_finds_a_model_id_inside_binary_noise() {
        let blob = b"\x08\x01\x12\x18claude-sonnet-5-5-medium\x1a\x02ok";
        assert_eq!(
            model_name_in(blob).as_deref(),
            Some("claude-sonnet-5-5-medium")
        );
        assert_eq!(model_name_in(b"\x00\x01 nothing here"), None);
    }

    #[test]
    fn model_is_read_from_the_conversation_database_and_absent_without_one() {
        let (dir, path) =
            transcript_with(&[r#"{"step_index":0,"type":"USER_INPUT","content":"go"}"#]);
        assert_eq!(session_model(&path), None);

        let conversations = dir.path().join("conversations");
        std::fs::create_dir_all(&conversations).unwrap();
        let connection = rusqlite::Connection::open(
            conversations.join("00000000-0000-4000-8000-000000000001.db"),
        )
        .unwrap();
        connection
            .execute("CREATE TABLE gen_metadata (idx integer, data blob)", [])
            .unwrap();
        connection
            .execute(
                "INSERT INTO gen_metadata VALUES (0, ?1)",
                [b"\x0agemini-3-pro\x00".to_vec()],
            )
            .unwrap();
        drop(connection);
        assert_eq!(session_model(&path).as_deref(), Some("gemini-3-pro"));
    }

    #[test]
    fn token_counts_come_from_the_planner_step() {
        let (_dir, path) = transcript_with(&[
            r#"{"step_index":0,"type":"USER_INPUT","content":"go"}"#,
            r#"{"step_index":1,"type":"PLANNER_RESPONSE","content":"ok","input_tokens":10,"output_tokens":4,"cache_read_tokens":7}"#,
        ]);
        let projection = parse(&path);
        let usage = projection
            .entries
            .iter()
            .find_map(|(_, entry)| match entry {
                LogEntry::Assistant { message, .. } => message.usage.clone(),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            (
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_input_tokens
            ),
            (10, 4, 7)
        );
    }
}
