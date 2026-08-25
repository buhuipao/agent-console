//! Recognising a provider's blocking TUI dialog from the terminal screen.
//!
//! Both providers stop and wait on a numbered menu -- Claude Code's "trust this folder", a
//! tool permission request, a "continue?" confirmation. The conversation view is rendered
//! from the transcript, and a dialog is not in the transcript, so a session blocked on one
//! looks to the web UI exactly like a session doing nothing. Reading the screen is the only
//! way to see it.
//!
//! Deliberately conservative: it only reports a prompt when the screen shows numbered options
//! *and* the provider's own confirm hint. A false positive would put a fake question in front
//! of the user; a false negative just leaves them the Terminal tab, which is where they are
//! today.

use serde::Serialize;

/// A blocking menu the agent is waiting on.
#[derive(Debug, Eq, PartialEq, Serialize)]
pub(super) struct BlockingPrompt {
    /// The prose above the options, e.g. the "Quick safety check: ..." paragraph.
    pub question: String,
    pub options: Vec<PromptOption>,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
pub(super) struct PromptOption {
    /// The number the user would type. Not the position in `options`: a provider is free to
    /// skip or reorder, and the digit is what gets written to the pty.
    pub number: u8,
    pub label: String,
}

/// Lines that mean "this menu is waiting for a keypress". Matching the hint rather than the
/// question keeps this from firing on ordinary numbered lists in agent output.
const CONFIRM_HINTS: [&str; 3] = ["to confirm", "to select", "to cancel"];

/// Parses the visible screen into a blocking prompt, or `None` when the agent is not waiting
/// on one.
pub(super) fn detect(screen: &str) -> Option<BlockingPrompt> {
    let lines = screen.lines().map(str::trim_end).collect::<Vec<_>>();
    if !lines.iter().any(is_confirm_hint) {
        return None;
    }

    let mut options = Vec::new();
    let mut first_option = None;
    for (index, line) in lines.iter().enumerate() {
        if let Some(option) = parse_option(line) {
            first_option.get_or_insert(index);
            options.push(option);
        }
    }
    if options.len() < 2 {
        return None;
    }

    let question = lines[..first_option?]
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty() && !is_rule(line))
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join(" ");

    Some(BlockingPrompt {
        question: question.trim().to_owned(),
        options,
    })
}

fn is_confirm_hint(line: &&str) -> bool {
    let lowered = line.to_lowercase();
    CONFIRM_HINTS.iter().any(|hint| lowered.contains(hint))
}

/// A horizontal rule is decoration, never part of the question.
fn is_rule(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|character| matches!(character, '─' | '━' | '-' | '=' | '_' | '╭'..='╿'))
}

/// Matches `1. Yes, I trust this folder`, with or without the `❯` selection marker.
fn parse_option(line: &str) -> Option<PromptOption> {
    let trimmed = line.trim_start().trim_start_matches(['❯', '>', '*']).trim();
    let (number, rest) = trimmed.split_once('.')?;
    let number = number.trim().parse::<u8>().ok()?;
    let label = rest.trim();
    if label.is_empty() {
        return None;
    }
    Some(PromptOption {
        number,
        label: label.to_owned(),
    })
}

/// The bytes that answer a numbered menu: the digit, then a carriage return to confirm.
pub(super) fn answer_bytes(number: u8) -> Vec<u8> {
    let mut bytes = number.to_string().into_bytes();
    bytes.push(b'\r');
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rendered from a real `claude` launch in an untrusted directory.
    const TRUST_DIALOG: &str = "\
────────────────────────────────────────────────
 Accessing workspace:
 /private/tmp/dlg

 Quick safety check: Is this a project you created or one you trust?
 Claude Code'll be able to read, edit, and execute files here.

 ❯ 1. Yes, I trust this folder
   2. No, exit

 Enter to confirm · Esc to cancel
";

    #[test]
    fn a_trust_dialog_is_reported_with_its_question_and_both_options() {
        let prompt = detect(TRUST_DIALOG).expect("the trust dialog is a blocking prompt");
        assert_eq!(
            prompt.options,
            vec![
                PromptOption {
                    number: 1,
                    label: "Yes, I trust this folder".into()
                },
                PromptOption {
                    number: 2,
                    label: "No, exit".into()
                },
            ]
        );
        assert!(
            prompt.question.contains("Quick safety check"),
            "the prose above the options is the question: {}",
            prompt.question
        );
        assert!(
            !prompt.question.contains("─"),
            "decoration is not part of the question: {}",
            prompt.question
        );
    }

    #[test]
    fn ordinary_numbered_output_is_not_mistaken_for_a_prompt() {
        let screen = "\
Here is the plan:
 1. Read the config
 2. Patch the handler
 3. Run the tests
Done.
";
        assert_eq!(
            detect(screen),
            None,
            "a numbered list without a confirm hint is just agent output"
        );
    }

    #[test]
    fn a_confirm_hint_alone_is_not_a_prompt() {
        assert_eq!(detect("Press Enter to confirm\n"), None);
    }

    #[test]
    fn an_idle_screen_is_not_a_prompt() {
        assert_eq!(detect("╭─ Claude Code ─╮\n│ > │\n╰───╯\n"), None);
    }

    #[test]
    fn answering_writes_the_digit_and_a_carriage_return() {
        assert_eq!(answer_bytes(1), b"1\r".to_vec());
        assert_eq!(answer_bytes(2), b"2\r".to_vec());
    }
}
