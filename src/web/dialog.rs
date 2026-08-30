//! Recognising a provider's blocking TUI dialog from the terminal screen.
//!
//! An agent stops and waits on a menu -- Claude Code's "trust this folder" and its tool
//! permission prompt, Codex's "update available" and spend-cap notices, pi's "trust project
//! folder?". None of them is ever written to a transcript, so a session stopped on one looks
//! to the web UI exactly like a session doing nothing. Reading the screen is the only way to
//! see it, and for pi it is the *only* signal there is: pi emits no extension events at all
//! until its trust dialog is answered, so even the hook bridge stays silent.
//!
//! Two shapes, because the providers disagree about how a menu is answered. Codex numbers its
//! options and takes the digit. Claude Code and pi move a cursor through an unnumbered list and
//! take arrow keys. Both are reported the same way -- options the caller picks by number -- and
//! only the keystrokes differ.
//!
//! # Why this is anchored rather than searched
//!
//! An earlier revision looked for evidence anywhere on screen: any two numbered lines plus any
//! line saying "to confirm". That is far too loose, and the failures were not theoretical --
//! every one below was reproduced against the real code:
//!
//! - A stale plan (`1. Read the config` / `2. Patch the handler`) scrolled above a live
//!   permission dialog won over the dialog itself. Answering "option 1" then wrote `1\r`, and
//!   in a cursor menu the digit is inert while the carriage return **confirms whatever the
//!   agent has highlighted** -- approving a tool call the user never chose.
//! - Ordinary assistant prose ("Want me to continue?") above any numbered list was reported as
//!   a dialog, which both showed a fabricated decision card and took `/prompt` offline, since
//!   the sender refuses to submit into a dialog.
//! - A markdown bullet list or blockquote plus the word "cancel" became a cursor menu.
//!
//! So a menu is now a *contiguous block of options immediately above its own footer*, every
//! line of which has to look like an option. Prose elsewhere on the screen cannot contribute.
//!
//! # Why answering a cursor menu is two steps
//!
//! A cursor menu is answered positionally: walk the highlight, then press Enter. That is only
//! safe if the parse was right. A label that wraps onto the next line at the same column is
//! indistinguishable from a sibling option in text alone -- and a phone-sized viewport makes
//! wrapping likely -- so a miscount would confirm the wrong row. Rather than guess, the arrows
//! are sent, the screen is read back, and Enter follows only once the highlight is actually on
//! the label the caller asked for. See [`Answer`].

use serde::Serialize;

/// A blocking menu the agent is waiting on.
#[derive(Debug, Eq, PartialEq, Serialize)]
pub(super) struct BlockingPrompt {
    /// The prose above the options, e.g. the "Quick safety check: ..." paragraph.
    pub question: String,
    pub options: Vec<PromptOption>,
    pub style: PromptStyle,
    /// The option the agent currently highlights, for a cursor menu.
    pub selected: Option<u8>,
}

/// How the agent expects its menu to be answered.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum PromptStyle {
    /// Codex: type the option's own digit.
    Numbered,
    /// Claude Code and pi: move a cursor to the option, then press Enter.
    Cursor,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
pub(super) struct PromptOption {
    /// The number the caller picks. For a numbered menu it is the provider's own digit, which
    /// is free to skip or reorder; for a cursor menu it is the row's position.
    pub number: u8,
    pub label: String,
}

/// What has to be written to answer a prompt.
pub(super) enum Answer {
    /// A numbered menu: the digit and a carriage return, in one write.
    Once(Vec<u8>),
    /// A cursor menu: arrow keys, then a check that the highlight landed on `expect`, and only
    /// then [`CONFIRM`]. Splitting it is what makes a positional answer safe -- if the menu was
    /// mis-parsed, or wrapped, or moved under us, the check fails and nothing is confirmed.
    Move { keys: Vec<u8>, expect: String },
}

/// The keystroke that accepts the highlighted row of a cursor menu.
pub(super) const CONFIRM: &[u8] = b"\r";

/// Footers naming a key to press. Anchoring on one keeps every ordinary list on screen out.
///
/// `to continue` is Codex's own footer ("Press enter to continue") and needs the key named
/// alongside it: on its own it is ordinary prose, and "Want me to continue?" was reported as a
/// dialog until this was tightened.
fn is_footer(line: &str) -> bool {
    let lowered = line.to_lowercase();
    let names_a_key = lowered.contains("press") || lowered.contains("enter");
    ["to confirm", "to select", "to cancel"]
        .iter()
        .any(|hint| lowered.contains(hint))
        || (lowered.contains("to continue") && names_a_key)
        || (lowered.contains("navigate") && lowered.contains("select"))
}

/// Glyphs a TUI puts in front of the highlighted row.
///
/// Deliberately excludes `>` and `*`: those are a markdown blockquote and a bullet, and
/// accepting them turned ordinary agent output into a menu. A numbered option may still carry
/// them, because there a digit is required as well.
const CURSOR_MARKERS: [char; 3] = ['\u{276f}', '\u{203a}', '\u{2192}'];

/// Markers allowed in front of a *numbered* option, where the digit does the real work.
const OPTION_MARKERS: [char; 5] = ['\u{276f}', '\u{203a}', '\u{2192}', '>', '*'];

/// How many blank lines may sit between the options and their footer.
const MAX_FOOTER_GAP: usize = 2;

/// Parses the visible screen into a blocking prompt, or `None` when the agent is not waiting
/// on one.
pub(super) fn detect(screen: &str) -> Option<BlockingPrompt> {
    let lines = screen
        .lines()
        .map(|line| unframe(line.trim_end()))
        .collect::<Vec<_>>();
    // The last footer, not the first: an earlier dialog's footer can still be scrolled above
    // the live one.
    let footer = lines.iter().rposition(|line| is_footer(line))?;
    let (start, end) = option_block(&lines, footer)?;
    let block = &lines[start..end];
    let question = question_above(&lines, start);
    numbered_menu(block, &question).or_else(|| cursor_menu(block, &question))
}

/// The contiguous run of lines that ends just above `footer`.
///
/// Bounded on both sides: at most [`MAX_FOOTER_GAP`] blank lines may separate the block from
/// its footer, and the block itself stops at the first blank line above it. Everything the
/// agent printed earlier is therefore out of reach, which is the whole point.
fn option_block(lines: &[&str], footer: usize) -> Option<(usize, usize)> {
    let mut end = footer;
    let mut gap = 0;
    while end > 0 && lines[end - 1].trim().is_empty() {
        gap += 1;
        if gap > MAX_FOOTER_GAP {
            return None;
        }
        end -= 1;
    }
    if end == 0 {
        return None;
    }
    let mut start = end;
    while start > 0 && !lines[start - 1].trim().is_empty() {
        start -= 1;
    }
    (end - start >= 2).then_some((start, end))
}

/// Codex: every line carries its own digit.
///
/// *Every* line, not merely two of them. A block with prose in it is not a menu, and admitting
/// one is how a stale plan above a live dialog came to be answered as if it were the dialog.
fn numbered_menu(block: &[&str], question: &str) -> Option<BlockingPrompt> {
    let mut options: Vec<PromptOption> = Vec::new();
    for line in block {
        match parse_numbered(line) {
            Some(option) => options.push(option),
            // A deeper-indented line continues the label above it.
            None => append_continuation(&mut options, line)?,
        }
    }
    (options.len() >= 2).then(|| BlockingPrompt {
        question: question.to_owned(),
        options,
        style: PromptStyle::Numbered,
        selected: None,
    })
}

/// Claude Code and pi: no digits, one row marked, the rest aligned under its label.
fn cursor_menu(block: &[&str], question: &str) -> Option<BlockingPrompt> {
    let marked = block
        .iter()
        .position(|line| cursor_label_column(line).is_some())?;
    // Exactly one row may carry the marker. Two means this is not a menu.
    if block
        .iter()
        .filter(|line| cursor_label_column(line).is_some())
        .count()
        != 1
    {
        return None;
    }
    let column = cursor_label_column(block[marked])?;

    let mut options: Vec<PromptOption> = Vec::new();
    let mut selected = None;
    for (index, line) in block.iter().enumerate() {
        match label_at_column(line, column) {
            Some(label) => {
                let number = u8::try_from(options.len() + 1).ok()?;
                if index == marked {
                    selected = Some(number);
                }
                options.push(PromptOption { number, label });
            }
            None => append_continuation(&mut options, line)?,
        }
    }
    (options.len() >= 2 && selected.is_some()).then(|| BlockingPrompt {
        question: question.to_owned(),
        options,
        style: PromptStyle::Cursor,
        selected,
    })
}

/// Folds a wrapped line into the option above it, or rejects the block.
///
/// Inside a block already anchored to its footer, a line that is not itself an option is the
/// tail of the one above it -- pi wraps an option's path onto its own line at a *shallower*
/// indent than the option carries, so indentation cannot be the test. What still rejects a
/// block is a non-option line with no option above it at all, which is what prose looks like.
fn append_continuation(options: &mut [PromptOption], line: &str) -> Option<()> {
    let last = options.last_mut()?;
    last.label.push(' ');
    last.label.push_str(line.trim());
    Some(())
}

fn indent_of(line: &str) -> usize {
    line.chars().take_while(|value| *value == ' ').count()
}

/// Drops a box border so a framed dialog parses like a bare one. Claude Code draws its tool
/// permission prompt inside a rounded box, which was invisible to both shapes.
///
/// Only when the line is bordered on *both* sides, so a lone `|` in ordinary output is content.
fn unframe(line: &str) -> &str {
    const SIDES: [char; 4] = ['\u{2502}', '\u{2503}', '\u{2551}', '|'];
    let trimmed = line.trim_end();
    let inner = trimmed.trim_start();
    let Some(first) = inner.chars().next() else {
        return line;
    };
    if !SIDES.contains(&first) || !trimmed.ends_with(SIDES) {
        return line;
    }
    let body = &inner[first.len_utf8()..];
    match body.char_indices().next_back() {
        Some((offset, last)) if SIDES.contains(&last) => &body[..offset],
        _ => body,
    }
}

/// The prose immediately above the options, bounded: everything printed before the dialog is
/// still on screen above it.
fn question_above(lines: &[&str], start: usize) -> String {
    let kept = lines[..start]
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty() && !is_rule(line))
        .rev()
        .take(6)
        .collect::<Vec<_>>();
    kept.into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_owned()
}

/// A horizontal rule is decoration, never part of the question.
fn is_rule(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|character| matches!(character, '─' | '━' | '-' | '=' | '_' | '╭'..='╿'))
}

/// Matches `1. Yes, I trust this folder`, with or without a selection marker.
fn parse_numbered(line: &str) -> Option<PromptOption> {
    let trimmed = line.trim_start().trim_start_matches(OPTION_MARKERS).trim();
    let (number, rest) = trimmed.split_once('.')?;
    let number = number.trim().parse::<u8>().ok()?;
    let label = rest.trim();
    (!label.is_empty()).then(|| PromptOption {
        number,
        label: label.to_owned(),
    })
}

/// The column an option's text starts at, given the line carrying the cursor. Counted in
/// characters, never bytes: the markers are three bytes wide.
fn cursor_label_column(line: &str) -> Option<usize> {
    let indent = indent_of(line);
    let rest = &line[indent..];
    let width = CURSOR_MARKERS
        .iter()
        .find(|value| rest.starts_with(**value))?
        .len_utf8();
    let after = &rest[width..];
    if !after.starts_with(' ') {
        return None;
    }
    let spaces = after.chars().take_while(|value| *value == ' ').count();
    let label = after[spaces..].trim();
    (!label.is_empty()).then_some(indent + 1 + spaces)
}

/// The line's text when it begins exactly at `column`, which is what marks it a sibling of the
/// cursor's own option rather than a wrapped continuation of one.
fn label_at_column(line: &str, column: usize) -> Option<String> {
    let at_column = || {
        let label = line.chars().skip(column).collect::<String>();
        let label = label.trim().to_owned();
        (!label.is_empty()).then_some(label)
    };
    if indent_of(line) == column {
        return at_column();
    }
    (cursor_label_column(line) == Some(column))
        .then(at_column)
        .flatten()
}

/// What has to be written to answer `prompt` with `number`, or `None` when it offers no such
/// option.
pub(super) fn answer(prompt: &BlockingPrompt, number: u8) -> Option<Answer> {
    let target = prompt
        .options
        .iter()
        .find(|option| option.number == number)?;
    match prompt.style {
        PromptStyle::Numbered => {
            let mut bytes = number.to_string().into_bytes();
            bytes.extend_from_slice(CONFIRM);
            Some(Answer::Once(bytes))
        }
        PromptStyle::Cursor => {
            let selected = prompt.selected?;
            let steps = usize::from(number.abs_diff(selected));
            let arrow: &[u8] = if number > selected {
                b"\x1b[B"
            } else {
                b"\x1b[A"
            };
            Some(Answer::Move {
                keys: arrow.repeat(steps),
                expect: target.label.clone(),
            })
        }
    }
}

/// The label a cursor menu currently highlights, used to confirm an [`Answer::Move`] landed
/// before anything is confirmed.
pub(super) fn marked_label(screen: &str) -> Option<String> {
    let prompt = detect(screen)?;
    let selected = prompt.selected?;
    prompt
        .options
        .into_iter()
        .find(|option| option.number == selected)
        .map(|option| option.label)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(prompt: &BlockingPrompt) -> Vec<&str> {
        prompt
            .options
            .iter()
            .map(|option| option.label.as_str())
            .collect()
    }

    fn once(prompt: &BlockingPrompt, number: u8) -> Vec<u8> {
        match answer(prompt, number) {
            Some(Answer::Once(bytes)) => bytes,
            _ => panic!("expected a one-shot answer"),
        }
    }

    fn moved(prompt: &BlockingPrompt, number: u8) -> (Vec<u8>, String) {
        match answer(prompt, number) {
            Some(Answer::Move { keys, expect }) => (keys, expect),
            _ => panic!("expected a cursor answer"),
        }
    }

    // ---- dialogs that must be seen -------------------------------------------------------

    /// Rendered from a real `claude` launch in an untrusted directory, older build.
    const CLAUDE_NUMBERED_TRUST: &str = "\
────────────────────────────────────────────────
 Accessing workspace:
 /private/tmp/dlg

 Quick safety check: Is this a project you created or one you trust?
 Claude Code'll be able to read, edit, and execute files here.

 ❯ 1. Yes, I trust this folder
   2. No, exit

 Enter to confirm · Esc to cancel
";

    /// Claude Code 2.1.251 dropped the digits: same dialog, now a cursor menu.
    const CLAUDE_CURSOR_TRUST: &str = "\
 Quick safety check: Is this a project you created or one you trust?

 ❯ No, exit
   Yes, I trust this folder

 Enter to confirm · Esc to cancel
";

    /// Captured from `codex` 0.150.1. Footer says "continue", marker is a single angle quote.
    const CODEX_UPDATE: &str = "\
  ✨ Update available! 0.150.1 -> 0.151.0
  Release notes: https://github.com/openai/codex/releases/latest

› 1. Update now (runs `npm install -g @openai/codex`)
  2. Skip
  3. Skip until next version

  Press enter to continue
";

    /// Captured from `pi` 0.84.4. Note the shallower-indented path line: it belongs to the
    /// option above it, not to the menu.
    const PI_TRUST: &str = "\
 Trust project folder?
 /private/tmp/dlg

 This allows pi to load .pi settings and resources.

 → Trust
   Trust parent folder
 (/private/tmp)
   Trust (this session only)
   Do not trust

 ↑↓ navigate  enter select  escape/ctrl+c cancel
";

    #[test]
    fn a_numbered_trust_dialog_keeps_its_question_and_answers_with_a_digit() {
        let prompt = detect(CLAUDE_NUMBERED_TRUST).expect("a numbered trust dialog blocks");
        assert_eq!(prompt.style, PromptStyle::Numbered);
        assert_eq!(labels(&prompt), ["Yes, I trust this folder", "No, exit"]);
        assert!(prompt.question.contains("Quick safety check"));
        assert!(!prompt.question.contains('─'), "{}", prompt.question);
        assert_eq!(once(&prompt, 2), b"2\r".to_vec());
    }

    #[test]
    fn an_unnumbered_trust_dialog_is_answered_by_walking_the_highlight() {
        let prompt = detect(CLAUDE_CURSOR_TRUST).expect("an unnumbered trust dialog blocks");
        assert_eq!(prompt.style, PromptStyle::Cursor);
        assert_eq!(prompt.selected, Some(1));
        assert_eq!(labels(&prompt), ["No, exit", "Yes, I trust this folder"]);
        // The dangerous option is the highlighted one, so trusting means moving off it first.
        let (keys, expect) = moved(&prompt, 2);
        assert_eq!(keys, b"\x1b[B".to_vec());
        assert_eq!(expect, "Yes, I trust this folder");
        // Answering the already-highlighted row still confirms, and still verifies first.
        assert_eq!(moved(&prompt, 1), (Vec::new(), "No, exit".to_owned()));
    }

    #[test]
    fn the_codex_update_menu_keeps_its_marked_first_option() {
        let prompt = detect(CODEX_UPDATE).expect("codex stops here waiting for a key");
        assert_eq!(prompt.style, PromptStyle::Numbered);
        assert_eq!(
            prompt
                .options
                .iter()
                .map(|option| option.number)
                .collect::<Vec<_>>(),
            vec![1, 2, 3],
            "the marked option is an option too, or the user cannot choose to update"
        );
        assert_eq!(once(&prompt, 2), b"2\r".to_vec());
    }

    #[test]
    fn a_pi_cursor_menu_folds_a_wrapped_path_into_the_option_above_it() {
        let prompt = detect(PI_TRUST).expect("pi's trust dialog blocks");
        assert_eq!(prompt.style, PromptStyle::Cursor);
        assert_eq!(prompt.selected, Some(1));
        assert_eq!(
            labels(&prompt),
            [
                "Trust",
                "Trust parent folder (/private/tmp)",
                "Trust (this session only)",
                "Do not trust",
            ],
            "the shallower path line continues its option instead of becoming one"
        );
        let (keys, expect) = moved(&prompt, 3);
        assert_eq!(keys, b"\x1b[B\x1b[B".to_vec());
        assert_eq!(expect, "Trust (this session only)");
    }

    /// Claude Code draws its tool permission prompt inside a rounded box, which made it
    /// invisible to a parser that required the marker to be the first character on the line.
    #[test]
    fn a_dialog_drawn_inside_a_box_is_still_a_dialog() {
        let screen = "\
│ Bash command: npm test                    │
│ Do you want to proceed?                   │
│                                           │
│ ❯ Yes                                     │
│   No, and tell Claude what to do          │
│                                           │
│ Enter to confirm · Esc to cancel          │
";
        let prompt = detect(screen).expect("a framed dialog blocks just the same");
        assert_eq!(prompt.style, PromptStyle::Cursor);
        assert_eq!(labels(&prompt), ["Yes", "No, and tell Claude what to do"]);
        assert!(prompt.question.contains("Do you want to proceed?"));
    }

    // ---- screens that must NOT be seen as dialogs -----------------------------------------

    /// The precedence bug, verbatim. A stale plan scrolled above a live cursor dialog used to
    /// win, and answering "option 1" wrote `1\r` -- inert digit, live Enter -- silently
    /// approving the highlighted `Yes`.
    #[test]
    fn a_stale_list_above_a_live_dialog_never_wins_over_the_dialog() {
        let screen = "\
❯ refactor the parser

  Here is the plan:
  1. Read the config
  2. Patch the handler

  Bash command: npm test
  Do you want to proceed?

  ❯ Yes
    No, and tell Claude what to do

  Enter to confirm · Esc to cancel
";
        let prompt = detect(screen).expect("the live dialog is what blocks");
        assert_eq!(prompt.style, PromptStyle::Cursor);
        assert_eq!(labels(&prompt), ["Yes", "No, and tell Claude what to do"]);
        assert_eq!(prompt.selected, Some(1));
    }

    #[test]
    fn ordinary_assistant_output_is_not_a_dialog() {
        // A numbered list plus prose that merely contains "continue".
        assert_eq!(
            detect(
                "\
  Three tests fail:

  1. web::dialog::tests::a_trust_dialog
  2. web::control::tests::the_paste
  3. pty::tests::alt_screen

  Want me to continue and fix them?
"
            ),
            None
        );
        // Markdown bullets next to a footer word.
        assert_eq!(
            detect(
                "\
* run the tests
* update the docs
* ship it

Press Enter to continue
"
            ),
            None,
            "a bullet is not a selection marker"
        );
        // A markdown blockquote next to "cancel".
        assert_eq!(
            detect(
                "\
> servers MUST retry
> clients MAY give up

  Press ctrl-c to cancel
"
            ),
            None
        );
        // The user's own prompt, echoed and wrapped under a cursor glyph.
        assert_eq!(
            detect(
                "\
╭─ Claude Code ─╮
❯ read the deploy log and tell me
  whether the migration is
  safe to continue
"
            ),
            None
        );
    }

    #[test]
    fn a_list_far_from_its_supposed_footer_is_not_a_dialog() {
        let screen = "\
 1. Read the config
 2. Patch the handler




 Enter to confirm
";
        assert_eq!(
            detect(screen),
            None,
            "options have to be adjacent to the footer they claim"
        );
    }

    #[test]
    fn a_confirm_hint_alone_is_not_a_prompt() {
        assert_eq!(detect("Press Enter to confirm\n"), None);
        assert_eq!(detect("╭─ Claude Code ─╮\n│ > │\n╰───╯\n"), None);
    }

    // ---- answering ------------------------------------------------------------------------

    #[test]
    fn an_option_the_dialog_does_not_offer_has_no_answer() {
        let prompt = detect(CLAUDE_CURSOR_TRUST).unwrap();
        assert!(answer(&prompt, 3).is_none());
        assert!(answer(&prompt, 0).is_none());
    }

    /// The highlight is read back from the screen, which is how a mis-parsed or wrapped menu
    /// fails to confirm rather than confirming the wrong row.
    #[test]
    fn the_marked_label_is_recoverable_from_the_screen() {
        assert_eq!(
            marked_label(CLAUDE_CURSOR_TRUST).as_deref(),
            Some("No, exit")
        );
        assert_eq!(marked_label(PI_TRUST).as_deref(), Some("Trust"));
        // A numbered menu highlights nothing, so there is nothing to verify against.
        assert_eq!(marked_label(CODEX_UPDATE), None);
    }

    #[test]
    fn a_label_in_any_script_survives_the_round_trip() {
        let screen = "\
 是否信任此目录？

 ❯ 信任
   不信任

 Enter to confirm
";
        let prompt = detect(screen).expect("a CJK dialog blocks too");
        assert_eq!(labels(&prompt), ["信任", "不信任"]);
        let (keys, expect) = moved(&prompt, 2);
        assert_eq!(keys, b"\x1b[B".to_vec());
        assert_eq!(expect, "不信任");
    }
}
