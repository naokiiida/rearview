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
use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub struct AgyTranscriptFormat;

pub static AGY_TRANSCRIPT: AgyTranscriptFormat = AgyTranscriptFormat;

pub const AGY_TRANSCRIPT_FILE: &str = "transcript.jsonl";
pub const AGY_TRANSCRIPT_FULL_FILE: &str = "transcript_full.jsonl";

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct AgyRecord {
    step_index: usize,
    #[serde(default)]
    source: Option<String>,
    #[serde(rename = "type")]
    record_type: String,
    #[serde(default)]
    status: Option<String>,
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
        let mut last_tool_call_id = None;

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
                            last_tool_call_id = Some(tool_id.clone());
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
                                    model: Some("gemini".to_string()),
                                    usage,
                                    id: None,
                                },
                                agent: None,
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
                        let tool_use_id = last_tool_call_id
                            .clone()
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
        "invoke_subagent" => (Tool::Agent, args),
        "send_message" => (Tool::AgentMessage, args),
        "search_web" => (Tool::WebSearch, args),
        _ => (Tool::Other, args),
    }
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
}
