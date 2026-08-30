//! Driving a running agent without a terminal on screen.
//!
//! The native web UI never renders a pty, but the agents underneath it only speak pty. These
//! two endpoints are the whole translation layer: a prompt is a bracketed paste followed by a
//! carriage return, and a stop is the escape key -- exactly what the TUI sends when a person
//! types into the same session.

use std::time::{Duration, Instant};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};
use serde::{Deserialize, Serialize};

use super::{AppState, agent, agent::AgentError, dialog};
use crate::pty;

/// What a terminal receives when the user presses Enter. A bracketed paste on its own is only
/// staged in the agent's composer; the carriage return is what submits the turn.
const SUBMIT: &[u8] = b"\r";

/// What the agent's own key binding for "stop what you are doing" listens for.
const INTERRUPT: &[u8] = &[0x1b];

#[derive(Deserialize)]
pub(crate) struct PromptRequest {
    text: String,
}

#[derive(Serialize)]
pub(crate) struct OkResponse {
    ok: bool,
}

#[derive(Deserialize)]
pub(crate) struct AnswerRequest {
    option: u8,
}

#[derive(Serialize)]
pub(crate) struct PromptStatus {
    /// False while the agent is starting up or blocked, which is why a prompt would be lost.
    accepts_input: bool,
    prompt: Option<dialog::BlockingPrompt>,
}

pub(crate) async fn send_prompt(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Json(body): Json<PromptRequest>,
) -> Result<Json<OkResponse>, (StatusCode, String)> {
    let paste = prompt_payload(&body.text).ok_or((
        StatusCode::BAD_REQUEST,
        "prompt text must not be empty\n".to_owned(),
    ))?;
    // A freshly spawned agent is not reading input yet, and anything written before it is
    // swallowed by its splash or by a blocking dialog -- the prompt just vanishes. Wait for
    // the agent to say it is ready before pasting.
    agent::await_input_ready(&state, &key)
        .await
        .map_err(to_response)?;
    stage_then_submit(&state, &key, &body.text, &paste).await
}

/// How many times the paste itself is sent before giving up. More than one because a paste
/// can be swallowed outright by a provider still starting up; bounded low because a duplicate
/// is worse than a clear failure.
const PASTE_ATTEMPTS: usize = 3;
/// How many times each paste is checked for before re-sending it.
const CHECKS_PER_PASTE: usize = 8;
/// Gap between checking whether the agent has visibly taken the paste.
const STAGE_POLL: Duration = Duration::from_millis(400);

/// Pastes the prompt, waits until the agent visibly has it, and only then presses Enter.
///
/// Readiness cannot be predicted, so this confirms delivery instead of assuming it. Claude
/// Code enables bracketed paste while it is still painting its banner and while its trust
/// dialog is up, so a paste sent on that signal alone can land nowhere and the prompt is lost
/// with no error -- measured, not assumed. If the text has not appeared after a few checks,
/// the paste is re-sent, since a swallowed write is the likelier explanation than a repaint
/// that slow. Re-sending only ever happens while no trace of the text is on screen.
///
/// Splitting paste from submit is what makes this safe. A carriage return sent at the wrong
/// moment answers whatever dialog happens to be up -- that is how an earlier version of this
/// code silently accepted a "trust this folder" prompt, and why the paste payload no longer
/// carries its own Enter. The submit is only sent once the agent is visibly holding *our*
/// text and a fresh screen read shows nothing waiting on a keypress.
async fn stage_then_submit(
    state: &AppState,
    key: &str,
    text: &str,
    paste: &[u8],
) -> Result<Json<OkResponse>, (StatusCode, String)> {
    let marker = staged_marker(text);
    for _ in 0..PASTE_ATTEMPTS {
        agent::write(state, key, paste).map_err(to_response)?;
        for _ in 0..CHECKS_PER_PASTE {
            tokio::time::sleep(STAGE_POLL).await;
            let screen = agent::screen_state(state, key).map_err(to_response)?;
            // Checked immediately before pressing Enter, because this is the destructive
            // step. If anything is waiting on a keypress, the prompt is not ours to submit.
            if let Some(prompt) = dialog::detect(&screen.text) {
                return Err(to_response(AgentError::Blocked(prompt.question)));
            }
            if is_staged(&screen.text, &marker) {
                return write(state, key, SUBMIT);
            }
        }
    }
    Err((
        StatusCode::CONFLICT,
        "the prompt was sent but the agent never showed it; open its Terminal tab to check \
         before sending it again\n"
            .to_owned(),
    ))
}

/// A short, distinctive slice of the prompt to look for on screen. Whole-text matching fails
/// as soon as the composer wraps or scrolls it.
fn staged_marker(text: &str) -> String {
    collapse(
        text.lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or(""),
    )
    .chars()
    .take(24)
    .collect()
}

/// Whether the agent is visibly holding the prompt. A long or multi-line paste is shown as a
/// placeholder rather than the text itself, so that counts too.
fn is_staged(screen: &str, marker: &str) -> bool {
    if marker.is_empty() {
        return false;
    }
    let screen = collapse(screen);
    screen.contains(marker) || screen.to_lowercase().contains("pasted text")
}

/// Screen text is padded and wrapped by the terminal grid; comparing it to a prompt only
/// works once both sides have their runs of whitespace flattened.
fn collapse(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The agent's blocking dialog, if it is sitting on one.
///
/// The conversation view is built from the transcript, and a dialog is never written to the
/// transcript, so without this a blocked session is indistinguishable from an idle one.
pub(crate) async fn blocking_prompt(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<PromptStatus>, (StatusCode, String)> {
    let screen = agent::screen_state(&state, &key).map_err(to_response)?;
    Ok(Json(PromptStatus {
        accepts_input: screen.accepts_input,
        prompt: dialog::detect(&screen.text),
    }))
}

/// How long a cursor menu is given to show the highlight on the chosen option.
const MOVE_TIMEOUT: Duration = Duration::from_secs(3);
/// Gap between reads while waiting for the highlight to land.
const MOVE_POLL: Duration = Duration::from_millis(100);

/// Answers a blocking dialog by choosing one of its options.
///
/// A numbered menu is one write: the digit names the option, so it cannot land on the wrong
/// one. A cursor menu is answered *positionally*, and is therefore done in two steps -- move,
/// read back, confirm. The parse can be wrong in ways no amount of care at parse time removes:
/// a label that wraps at the viewport width looks exactly like a sibling option, so a
/// miscounted walk would confirm the neighbour of what the user tapped. Reading the highlight
/// back before pressing Enter is what makes that impossible rather than unlikely.
pub(crate) async fn answer_prompt(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Json(body): Json<AnswerRequest>,
) -> Result<Json<OkResponse>, (StatusCode, String)> {
    let screen = agent::screen_state(&state, &key).map_err(to_response)?;
    let prompt = dialog::detect(&screen.text).ok_or((
        StatusCode::CONFLICT,
        "the agent is not waiting on a prompt\n".to_owned(),
    ))?;
    // Answering by number rather than by position, and only a number the dialog is actually
    // offering: the screen may have changed between the client reading it and this call.
    let answer = dialog::answer(&prompt, body.option).ok_or((
        StatusCode::CONFLICT,
        format!("the current prompt has no option {}\n", body.option),
    ))?;

    match answer {
        dialog::Answer::Once(bytes) => write(&state, &key, &bytes),
        dialog::Answer::Move { keys, expect } => {
            agent::write(&state, &key, &keys).map_err(to_response)?;
            let deadline = Instant::now() + MOVE_TIMEOUT;
            loop {
                tokio::time::sleep(MOVE_POLL).await;
                let screen = agent::screen_state(&state, &key).map_err(to_response)?;
                if dialog::marked_label(&screen.text).as_deref() == Some(expect.as_str()) {
                    return write(&state, &key, dialog::CONFIRM);
                }
                if Instant::now() >= deadline {
                    // Deliberately not confirmed. Enter here would accept whatever the agent
                    // happens to be highlighting, which is the accident this split prevents.
                    return Err((
                        StatusCode::CONFLICT,
                        format!(
                            "the menu did not move onto {expect:?}, so nothing was confirmed; \
                             answer it from the Agent TUI tab\n"
                        ),
                    ));
                }
            }
        }
    }
}

/// The bytes that put a prompt into the agent's composer, or `None` when there is no prompt
/// to send.
///
/// Deliberately carries no carriage return. Paste and submit have to be separate writes: an
/// Enter riding along with the paste is delivered whether or not the agent was ready for it,
/// and if a dialog is up at that moment it answers the dialog instead. Sending them together
/// is how a "trust this folder" prompt got silently accepted and the prompt discarded.
fn prompt_payload(text: &str) -> Option<Vec<u8>> {
    if text.trim().is_empty() {
        return None;
    }
    Some(pty::bracketed_paste(text))
}

pub(crate) async fn interrupt(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<OkResponse>, (StatusCode, String)> {
    write(&state, &key, INTERRUPT)
}

fn write(
    state: &AppState,
    key: &str,
    bytes: &[u8],
) -> Result<Json<OkResponse>, (StatusCode, String)> {
    agent::write(state, key, bytes)
        .map(|()| Json(OkResponse { ok: true }))
        .map_err(to_response)
}

fn to_response(error: AgentError) -> (StatusCode, String) {
    (error.status(), format!("{error}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_paste_carries_the_prompt_but_never_an_enter() {
        let payload = prompt_payload("run the tests").unwrap();

        assert!(payload.starts_with(b"\x1b[200~"));
        assert!(
            payload.ends_with(b"\x1b[201~"),
            "an Enter riding along with the paste would answer whatever dialog is up"
        );
        assert!(
            !payload.contains(&b'\r'),
            "the submit is a separate write, sent only once the agent visibly holds the text"
        );
        assert!(
            String::from_utf8_lossy(&payload).contains("run the tests"),
            "the prompt itself has to survive the paste framing"
        );
    }

    #[test]
    fn an_empty_or_whitespace_prompt_is_rejected_before_it_reaches_the_agent() {
        for text in ["", "   ", "\n\t"] {
            assert!(
                prompt_payload(text).is_none(),
                "{text:?} must not submit a turn"
            );
        }
    }

    #[test]
    fn the_stop_button_sends_the_escape_the_agents_listen_for() {
        assert_eq!(INTERRUPT, b"\x1b");
    }

    #[test]
    fn a_prompt_is_recognised_on_screen_even_once_the_composer_has_wrapped_it() {
        let marker = staged_marker("Reply with exactly: READY-OK");
        let screen = "\u{256d}\u{2500} Claude Code \u{2500}\u{256e}\n\u{276f} Reply with exactly:\n  READY-OK\n";

        assert!(
            is_staged(screen, &marker),
            "a composer that wrapped the prompt is still holding it"
        );
        assert!(
            !is_staged("\u{276f} \u{2502}", &marker),
            "an empty composer is not holding the prompt"
        );
    }

    #[test]
    fn a_long_paste_shown_as_a_placeholder_still_counts_as_staged() {
        let marker = staged_marker("line one\nline two\nline three");

        assert!(
            is_staged("\u{276f} [Pasted text #1 +3 lines]", &marker),
            "the agent abbreviates a multi-line paste instead of echoing it"
        );
    }

    #[test]
    fn a_blank_prompt_never_looks_staged() {
        assert!(!is_staged("anything at all", &staged_marker("   ")));
    }
}
