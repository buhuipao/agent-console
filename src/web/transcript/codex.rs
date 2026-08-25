//! Codex rollout lines (`~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<id>.jsonl`) to
//! conversation blocks.
//!
//! Codex records every turn twice: once as a `response_item` (the durable history the model
//! is replayed from) and once as an `event_msg` (the live stream the TUI renders). Only
//! `response_item` is read here -- reading both would print the whole conversation twice, and
//! `response_item` is the one that is always present.

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
    let Some(payload) = value.get("payload") else {
        return LineOutcome::Ignore;
    };

    match value.get("type").and_then(Value::as_str) {
        Some("response_item") => {
            item_message(payload, ts, offset, 0).map_or(LineOutcome::Ignore, LineOutcome::Emit)
        }
        Some("compacted") => {
            LineOutcome::ReplacePriorHistory(replacement_history(payload, ts, offset))
        }
        _ => LineOutcome::Ignore,
    }
}

/// A compaction carries the condensed history that replaced everything before it, in the same
/// `response_item` payload shape.
fn replacement_history(payload: &Value, ts: u64, offset: u64) -> Vec<Message> {
    payload
        .get("replacement_history")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, item)| item_message(item, ts, offset, index + 1))
        .collect()
}

fn item_message(payload: &Value, ts: u64, offset: u64, ordinal: usize) -> Option<Message> {
    let (role, blocks) = match payload.get("type").and_then(Value::as_str)? {
        "message" => message_blocks(payload)?,
        "reasoning" => (Role::Assistant, reasoning_blocks(payload)),
        "function_call" | "custom_tool_call" | "local_shell_call" => {
            (Role::Assistant, vec![tool_use_block(payload)])
        }
        "function_call_output" | "custom_tool_call_output" | "local_shell_call_output" => {
            (Role::User, vec![tool_result_block(payload)])
        }
        _ => return None,
    };
    let id = payload
        .get("id")
        .and_then(Value::as_str)
        .map_or_else(|| format!("codex-{offset}-{ordinal}"), str::to_owned);
    Message::new(id, role, ts, blocks)
}

/// `developer` messages are the injected instruction payloads (skills, permissions, AGENTS.md)
/// -- never something the user wrote or read, so the whole message is dropped.
fn message_blocks(payload: &Value) -> Option<(Role, Vec<Block>)> {
    let role = match payload.get("role").and_then(Value::as_str) {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        _ => return None,
    };
    let blocks = payload
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(content_block)
        .collect();
    Some((role, blocks))
}

fn content_block(item: &Value) -> Option<Block> {
    match item.get("type").and_then(Value::as_str)? {
        "input_text" | "output_text" | "text" => {
            let text = item.get("text").and_then(Value::as_str)?;
            conversational_text(text).map(|text| Block::Text {
                text: cap(&text, TEXT_LIMIT),
            })
        }
        "input_image" | "image" => Some(Block::Image),
        // `encrypted_content` and friends are opaque model state, not conversation.
        _ => None,
    }
}

/// Codex usually ships reasoning as opaque `encrypted_content` with an empty summary; only the
/// plain-text summary items are renderable, and a message with none of them is dropped.
fn reasoning_blocks(payload: &Value) -> Vec<Block> {
    payload
        .get("summary")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| Block::Thinking {
            text: cap(text, TEXT_LIMIT),
        })
        .collect()
}

fn tool_use_block(payload: &Value) -> Block {
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_owned();
    let raw = payload
        .get("input")
        .or_else(|| payload.get("arguments"))
        .or_else(|| payload.get("action"))
        .map(|value| {
            value
                .as_str()
                .map_or_else(|| value.to_string(), str::to_owned)
        })
        .unwrap_or_default();
    let headline = shell_command(&raw).unwrap_or_else(|| raw.trim().to_owned());
    Block::ToolUse {
        id: call_id(payload),
        name,
        summary: cap(&headline, SUMMARY_LIMIT),
    }
}

fn tool_result_block(payload: &Value) -> Block {
    let text = match payload.get("output") {
        Some(Value::String(output)) => output.trim().to_owned(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_owned(),
        _ => String::new(),
    };
    Block::ToolResult {
        tool_use_id: call_id(payload),
        ok: is_successful(payload, &text),
        summary: cap(&text, SUMMARY_LIMIT),
    }
}

/// Codex tool output has no error flag. Its `exec` tool prefixes every result with its own
/// verdict line, so that is the signal -- with an explicit `success` field winning when a
/// provider release does start emitting one.
fn is_successful(payload: &Value, text: &str) -> bool {
    if let Some(success) = payload.get("success").and_then(Value::as_bool) {
        return success;
    }
    !["Script failed", "aborted by user", "Script timed out"]
        .iter()
        .any(|marker| text.starts_with(marker))
}

fn call_id(payload: &Value) -> String {
    payload
        .get("call_id")
        .or_else(|| payload.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Codex's `exec` tool takes JavaScript, not a command line: the shell it actually runs is
/// buried in `tools.shell_command({command:"..."})`. Surfacing that string is the difference
/// between a UI showing `cargo test --locked` and showing a paragraph of glue code.
fn shell_command(input: &str) -> Option<String> {
    let start = ["\"command\":", "command:"]
        .iter()
        .find_map(|marker| input.find(marker).map(|index| index + marker.len()))?;
    let rest = input[start..].trim_start();
    if !rest.starts_with('"') {
        return None;
    }
    let command = serde_json::Deserializer::from_str(rest)
        .into_iter::<String>()
        .next()?
        .ok()?;
    let command = command.trim().to_owned();
    (!command.is_empty()).then_some(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shell_behind_a_codex_exec_call_is_what_gets_summarised() {
        let input = "const r = await tools.shell_command({command:\"cargo test --locked\",\"workdir\":\"/repo\"}); text(r);\n";
        assert_eq!(shell_command(input).as_deref(), Some("cargo test --locked"));
        assert_eq!(
            shell_command("{\"command\": \"git status --short\"}").as_deref(),
            Some("git status --short")
        );
        assert_eq!(shell_command("await tools.list_agents()"), None);
    }

    #[test]
    fn a_failed_exec_result_is_reported_as_not_ok() {
        let payload = serde_json::json!({"type": "custom_tool_call_output"});
        assert!(!is_successful(
            &payload,
            "Script failed\nWall time 0.1 seconds"
        ));
        assert!(is_successful(
            &payload,
            "Script completed\nWall time 0.1 seconds"
        ));
        assert!(!is_successful(
            &serde_json::json!({"success": false}),
            "Script completed"
        ));
    }
}
