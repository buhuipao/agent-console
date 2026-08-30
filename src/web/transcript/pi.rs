//! pi session lines (`~/.pi/agent/sessions/--<encoded-cwd>--/<stamp>_<uuid>.jsonl`) to
//! conversation blocks.
//!
//! pi wraps every conversational line in a `message` entry and keeps its bookkeeping in
//! sibling entry types (`session`, `model_change`, `thinking_level_change`, `label`,
//! `session_info`, `custom`), none of which a reader of the conversation wants to see.
//!
//! Two shapes differ from the other providers and are the reason this file exists rather than
//! reusing Claude's reader: pi names an assistant's tool calls `toolCall` (not `tool_use`) and
//! puts a tool's answer in its own `toolResult` *message* rather than inside the next user
//! turn, and its timestamps are entry-level ISO strings.

use serde_json::Value;

use super::{
    block::{
        Block, LineOutcome, Message, Role, SUMMARY_LIMIT, TEXT_LIMIT, cap, conversational_text,
    },
    timestamp::parse_rfc3339_seconds,
};

pub(super) fn parse_line(raw: &str, offset: u64) -> LineOutcome {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return LineOutcome::Ignore;
    };
    let ts = value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_seconds)
        .unwrap_or(0);
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .map_or_else(|| format!("pi-{offset}"), str::to_owned);

    match value.get("type").and_then(Value::as_str) {
        Some("message") => message_line(&value, id, ts),
        // An extension's injected context is real context the model saw, so it is shown --
        // but only when the extension asked for it to be.
        Some("custom_message") if value.get("display").and_then(Value::as_bool) != Some(false) => {
            let blocks = content_blocks(value.get("content"));
            Message::new(id, Role::System, ts, blocks)
                .map_or(LineOutcome::Ignore, LineOutcome::Emit)
        }
        // Compaction replaces everything before it. The condensed copy is only worth showing
        // when the window starts after the history it condenses.
        Some("compaction") => summary_replacement(&value, id, ts),
        // A branch summary adds context instead of replacing it: it is what `/tree` carried
        // over from the path the user left, and that path is not in this file's history above.
        Some("branch_summary") => Message::new(id, Role::System, ts, summary_blocks(&value))
            .map_or(LineOutcome::Ignore, LineOutcome::Emit),
        _ => LineOutcome::Ignore,
    }
}

fn message_line(value: &Value, id: String, ts: u64) -> LineOutcome {
    let Some(message) = value.get("message") else {
        return LineOutcome::Ignore;
    };
    let content = message.get("content");
    let (role, blocks) = match message.get("role").and_then(Value::as_str) {
        Some("user") => (Role::User, content_blocks(content)),
        Some("assistant") => (Role::Assistant, content_blocks(content)),
        Some("toolResult") => (Role::User, vec![tool_result_block(message)]),
        Some("bashExecution") => (Role::User, bash_blocks(message)),
        Some("custom") => (Role::System, content_blocks(content)),
        Some("compactionSummary" | "branchSummary") => (Role::System, summary_blocks(message)),
        _ => return LineOutcome::Ignore,
    };
    Message::new(id, role, ts, blocks).map_or(LineOutcome::Ignore, LineOutcome::Emit)
}

fn summary_replacement(value: &Value, id: String, ts: u64) -> LineOutcome {
    let blocks = summary_blocks(value);
    Message::new(id, Role::System, ts, blocks).map_or(LineOutcome::Ignore, |message| {
        LineOutcome::ReplacePriorHistory(vec![message])
    })
}

fn summary_blocks(value: &Value) -> Vec<Block> {
    value
        .get("summary")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| Block::Text {
            text: cap(text, TEXT_LIMIT),
        })
        .into_iter()
        .collect()
}

fn content_blocks(content: Option<&Value>) -> Vec<Block> {
    match content {
        Some(Value::String(text)) => text_block(text).into_iter().collect(),
        Some(Value::Array(items)) => items.iter().filter_map(content_block).collect(),
        _ => Vec::new(),
    }
}

fn content_block(item: &Value) -> Option<Block> {
    match item.get("type").and_then(Value::as_str)? {
        "text" => item
            .get("text")
            .and_then(Value::as_str)
            .and_then(text_block),
        "thinking" => item
            .get("thinking")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(|text| Block::Thinking {
                text: cap(text, TEXT_LIMIT),
            }),
        "toolCall" => Some(Block::ToolUse {
            id: string_or_default(item, "id"),
            name: string_or_default(item, "name"),
            summary: tool_call_summary(item.get("arguments")),
        }),
        // An image block is a base64 payload measured in megabytes. The UI only needs to know
        // one was there; the bytes are deliberately never relayed.
        "image" => Some(Block::Image),
        _ => None,
    }
}

/// pi's tool answers arrive as whole messages of their own, which is what lets one carry the
/// id of the call it answers without the reader having to pair them up.
fn tool_result_block(message: &Value) -> Block {
    Block::ToolResult {
        tool_use_id: string_or_default(message, "toolCallId"),
        ok: message.get("isError").and_then(Value::as_bool) != Some(true),
        summary: cap(&flatten_text(message.get("content")), SUMMARY_LIMIT),
    }
}

/// `!command` and `!!command` run outside the model. They are shown as what they are -- a
/// command and its output -- rather than as a tool the assistant chose to call.
fn bash_blocks(message: &Value) -> Vec<Block> {
    let command = string_or_default(message, "command");
    if command.trim().is_empty() {
        return Vec::new();
    }
    let output = message
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // pi omits `exitCode` entirely when the command was cancelled or killed, so an absent one
    // is a failure rather than a success. `cancelled` carries the same answer and is checked
    // too, because a cancelled command can still have reported a code.
    let ok = message.get("exitCode").and_then(Value::as_i64) == Some(0)
        && message.get("cancelled").and_then(Value::as_bool) != Some(true);
    vec![
        Block::Text {
            text: cap(&format!("!{command}"), TEXT_LIMIT),
        },
        Block::ToolResult {
            tool_use_id: String::new(),
            ok,
            summary: cap(output.trim(), SUMMARY_LIMIT),
        },
    ]
}

fn text_block(text: &str) -> Option<Block> {
    conversational_text(text).map(|text| Block::Text {
        text: cap(&text, TEXT_LIMIT),
    })
}

/// The one field a human recognises a tool call by: the command it ran, the file it touched,
/// the pattern it searched for. The whole argument object is the fallback, not the default.
fn tool_call_summary(arguments: Option<&Value>) -> String {
    let Some(arguments) = arguments else {
        return String::new();
    };
    let headline = ["command", "file_path", "path", "pattern", "url", "prompt"]
        .iter()
        .find_map(|key| arguments.get(*key).and_then(Value::as_str))
        .map(str::to_owned)
        .unwrap_or_else(|| arguments.to_string());
    cap(headline.trim(), SUMMARY_LIMIT)
}

fn flatten_text(content: Option<&Value>) -> String {
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
