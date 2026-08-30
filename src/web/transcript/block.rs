//! The wire shape of a conversation as the web UI renders it, plus the text hygiene every
//! provider parser shares. Provider transcripts carry megabyte tool results, base64 images and
//! injected instruction blocks; nothing reaches a block until it has been filtered and capped
//! here, so the browser never has to defend itself against a 50 MB field.

use serde::Serialize;

use crate::{discovery, model::AgentKind};

/// Cap for `summary` strings (tool commands and tool output). Tool results are routinely
/// megabytes; the UI only shows a preview line, so the rest is dead weight on every poll.
pub(crate) const SUMMARY_LIMIT: usize = 2_000;

/// Cap for conversation prose. Far more generous than `SUMMARY_LIMIT` because this is the
/// content the user actually reads, but still bounded so one pathological message cannot
/// blow up a response.
pub(crate) const TEXT_LIMIT: usize = 20_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Role {
    User,
    Assistant,
    System,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Block {
    Text {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        summary: String,
    },
    ToolResult {
        tool_use_id: String,
        ok: bool,
        summary: String,
    },
    Image,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct Message {
    pub id: String,
    pub role: Role,
    pub ts: u64,
    pub blocks: Vec<Block>,
}

impl Message {
    pub(crate) fn new(id: String, role: Role, ts: u64, blocks: Vec<Block>) -> Option<Self> {
        (!blocks.is_empty()).then_some(Self {
            id,
            role,
            ts,
            blocks,
        })
    }
}

/// What one transcript line contributed to the conversation.
pub(crate) enum LineOutcome {
    /// Plumbing, noise, or a line this provider has no rendering for.
    Ignore,
    /// Ordinary conversation content, appended in file order.
    Emit(Message),
    /// A compaction: the provider condensed everything before this point. The condensed copy
    /// duplicates history the transcript already spelled out above, so it is only rendered
    /// when the read window starts after that history and would otherwise show nothing.
    ReplacePriorHistory(Vec<Message>),
}

/// One transcript line, and the offset it starts at, to whatever it contributes.
pub(crate) type LineParser = fn(&str, u64) -> LineOutcome;

pub(crate) fn parser_for(agent: AgentKind) -> LineParser {
    match agent {
        AgentKind::Claude => super::claude::parse_line,
        AgentKind::Codex => super::codex::parse_line,
        AgentKind::Pi => super::pi::parse_line,
    }
}

/// Instruction payloads that get appended to a user's own words rather than sent on their
/// own, so cutting at the tag is what leaves the message the person actually typed.
const INJECTED_TAGS: &[&str] = &[
    "<system-reminder>",
    "<task-notification>",
    "<skills_instructions>",
    "<local-command-caveat>",
    "<local-command-stdout>",
    "<local-command-stderr>",
];

/// One text block as a human would read it, or `None` when there is nothing left of it once
/// the injected instructions are removed.
pub(crate) fn conversational_text(raw: &str) -> Option<String> {
    if let Some(command) = slash_command(raw) {
        return Some(command);
    }
    let trimmed = raw.trim();
    let kept = INJECTED_TAGS
        .iter()
        .filter_map(|tag| trimmed.find(tag))
        .min()
        .map_or(trimmed, |cut| &trimmed[..cut])
        .trim();
    (!is_noise(kept)).then(|| kept.to_owned())
}

/// Instruction payloads both providers inject as whole messages. `discovery` already owns the
/// shared list (it uses it to keep session titles readable); `INJECTED_TAGS` covers the ones a
/// full conversation view additionally sees appended to real prose.
fn is_noise(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.is_empty()
        || discovery::is_internal_context(trimmed)
        || INJECTED_TAGS.iter().any(|tag| trimmed.starts_with(tag))
}

/// Renders a slash command the user typed as the text they would recognise. Claude stores it
/// as `<command-name>/goal</command-name>...<command-args>do the thing</command-args>`, which
/// `is_noise` would otherwise drop along with the real request inside it.
fn slash_command(text: &str) -> Option<String> {
    let name = tag_body(text, "command-name")?;
    let args = tag_body(text, "command-args").unwrap_or_default();
    let rendered = format!("{} {}", name.trim(), args.trim());
    let rendered = rendered.trim().to_owned();
    (!rendered.is_empty()).then_some(rendered)
}

fn tag_body<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    text.split_once(&format!("<{tag}>"))?
        .1
        .split_once(&format!("</{tag}>"))
        .map(|(body, _)| body)
}

/// Caps a string at `limit` characters, marking the cut and reporting the original size so the
/// UI can say "this was 4 MB" instead of silently implying the tool printed 2 000 bytes.
pub(crate) fn cap(value: &str, limit: usize) -> String {
    let mut kept = String::with_capacity(limit);
    for character in value.chars().take(limit) {
        kept.push(character);
    }
    if kept.len() == value.len() {
        return kept;
    }
    kept.push_str(&format!("\n… [truncated, {} bytes total]", value.len()));
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capping_marks_the_cut_and_reports_the_original_size() {
        let original = "x".repeat(SUMMARY_LIMIT + 500);
        let capped = cap(&original, SUMMARY_LIMIT);

        assert!(capped.starts_with(&"x".repeat(SUMMARY_LIMIT)));
        assert!(
            capped.contains(&format!("[truncated, {} bytes total]", original.len())),
            "the caller must be able to tell how much was dropped: {capped}"
        );
        assert!(capped.len() < original.len());
    }

    #[test]
    fn capping_leaves_short_values_untouched_including_multibyte_ones() {
        assert_eq!(cap("hello", SUMMARY_LIMIT), "hello");
        assert_eq!(cap("分析这个工具", SUMMARY_LIMIT), "分析这个工具");
    }

    #[test]
    fn capping_never_splits_a_multibyte_character() {
        let original = "é".repeat(10);
        let capped = cap(&original, 4);
        assert!(capped.starts_with("éééé"), "{capped}");
    }

    #[test]
    fn a_wholly_injected_payload_leaves_no_conversation_behind() {
        for injected in [
            "<system-reminder>\nremember this\n</system-reminder>",
            "<task-notification>\n<summary>check-in</summary>",
            "<local-command-stdout></local-command-stdout>",
            "<environment_context>\ncwd=/repo\n</environment_context>",
            "   ",
        ] {
            assert_eq!(conversational_text(injected), None, "{injected}");
        }
        assert_eq!(
            conversational_text("please fix the failing test").as_deref(),
            Some("please fix the failing test")
        );
    }

    #[test]
    fn an_instruction_appended_to_a_real_message_is_cut_off_it() {
        assert_eq!(
            conversational_text(
                "ship the web UI\n\n<system-reminder>\nthe user cannot see this\n</system-reminder>"
            )
            .as_deref(),
            Some("ship the web UI")
        );
    }

    #[test]
    fn a_typed_slash_command_reads_back_as_the_user_typed_it() {
        let raw = "<command-name>/goal</command-name>\n<command-message>goal</command-message>\n<command-args>ship the web UI</command-args>";
        assert_eq!(
            conversational_text(raw).as_deref(),
            Some("/goal ship the web UI")
        );
        assert_eq!(slash_command("no command here"), None);
    }
}
