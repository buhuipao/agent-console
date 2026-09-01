//! Claude Code transcript lines (`~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`) to
//! conversation blocks.
//!
//! Only `user`, `assistant` and `system` lines carry conversation; the rest of the line types
//! (`last-prompt`, `custom-title`, `agent-name`, `mode`, `permission-mode`, `atis-latch`,
//! `attachment`, `file-history-snapshot`, `queue-operation`, ...) are editor bookkeeping the
//! user never typed and never read.

use serde_json::Value;

use super::{
    block::{
        Block, LineOutcome, Message, Role, SUMMARY_LIMIT, TEXT_LIMIT, cap, conversational_text,
        image_data_uri,
    },
    timestamp::parse_rfc3339_seconds,
};

pub(super) fn parse_line(raw: &str, offset: u64) -> LineOutcome {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return LineOutcome::Ignore;
    };
    // Subagent turns run their own thread against their own prompt. They are not part of
    // the conversation the user is having, so they never join the main transcript view.
    if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return LineOutcome::Ignore;
    }
    let ts = value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_seconds)
        .unwrap_or(0);
    let id = value
        .get("uuid")
        .and_then(Value::as_str)
        .map_or_else(|| format!("claude-{offset}"), str::to_owned);

    let (role, blocks) = match value.get("type").and_then(Value::as_str) {
        Some("user") => (Role::User, message_blocks(&value)),
        Some("assistant") => (Role::Assistant, message_blocks(&value)),
        Some("system") => (Role::System, system_blocks(&value)),
        _ => return LineOutcome::Ignore,
    };
    Message::new(id, role, ts, blocks).map_or(LineOutcome::Ignore, LineOutcome::Emit)
}

fn message_blocks(value: &Value) -> Vec<Block> {
    let content = value
        .get("message")
        .and_then(|message| message.get("content"));
    match content {
        Some(Value::String(text)) => text_block(text).into_iter().collect(),
        Some(Value::Array(items)) => items.iter().filter_map(content_block).collect(),
        _ => Vec::new(),
    }
}

/// `system` lines put their text at the top level rather than under `message`. Most of them
/// (`stop_hook_summary`, `turn_duration`) carry no text at all and drop out here.
fn system_blocks(value: &Value) -> Vec<Block> {
    value
        .get("content")
        .and_then(Value::as_str)
        .and_then(text_block)
        .into_iter()
        .collect()
}

fn content_block(item: &Value) -> Option<Block> {
    match item.get("type").and_then(Value::as_str)? {
        "text" => item
            .get("text")
            .and_then(Value::as_str)
            .and_then(text_block),
        "thinking" => item
            .get("thinking")
            .or_else(|| item.get("text"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(|text| Block::Thinking {
                text: cap(text, TEXT_LIMIT),
            }),
        "tool_use" => Some(Block::ToolUse {
            id: string_or_default(item, "id"),
            name: string_or_default(item, "name"),
            summary: tool_use_summary(item.get("input")),
        }),
        "tool_result" => Some(Block::ToolResult {
            tool_use_id: string_or_default(item, "tool_use_id"),
            ok: item.get("is_error").and_then(Value::as_bool) != Some(true),
            summary: cap(&tool_result_text(item.get("content")), SUMMARY_LIMIT),
        }),
        // An image block is a base64 payload measured in megabytes; only a capped copy is
        // relayed as a preview (see `image_data_uri`), never the raw field.
        "image" => Some(Block::Image {
            data: image_data_uri(item),
        }),
        _ => None,
    }
}

fn text_block(text: &str) -> Option<Block> {
    conversational_text(text).map(|text| Block::Text {
        text: cap(&text, TEXT_LIMIT),
    })
}

/// The one field a human recognises a tool call by: the command it ran, the file it touched,
/// the pattern it searched for. The whole input object is the fallback, not the default.
fn tool_use_summary(input: Option<&Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    let headline = ["command", "file_path", "path", "pattern", "url", "prompt"]
        .iter()
        .find_map(|key| input.get(*key).and_then(Value::as_str))
        .map(str::to_owned)
        .unwrap_or_else(|| input.to_string());
    cap(headline.trim(), SUMMARY_LIMIT)
}

fn tool_result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.trim().to_owned(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_owned(),
        _ => String::new(),
    }
}

fn string_or_default(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}
