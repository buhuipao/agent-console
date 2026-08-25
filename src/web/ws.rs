use std::{
    io,
    sync::{Arc, TryLockError},
    time::Duration,
};

use axum::{
    extract::{
        Path, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::Response,
};
use serde::{Deserialize, Serialize};

use super::{AppState, agent, shells};
use crate::pty::{ManagedTerminal, RawPoll, Scrollback};

/// The web layer owns its own per-websocket offset cursor and relays raw bytes straight from
/// `ManagedTerminal::poll_raw` into the socket -- no vt100 parsing happens server-side.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long a keystroke waits for the App-wide lock before it is dropped.
///
/// Reads skip a frame and catch up on the next one, but a write has nowhere to go, so it gets
/// a short grace period first. It stays short on purpose: an attached dashboard workspace can
/// hold that lock for minutes, and queueing keystrokes for minutes would replay a burst of
/// them into a terminal long after whoever typed them gave up.
const WRITE_GRACE: Duration = Duration::from_millis(300);
const WRITE_RETRY: Duration = Duration::from_millis(20);

#[derive(Deserialize)]
pub(crate) struct WsQuery {
    #[serde(default)]
    cols: Option<u16>,
    #[serde(default)]
    rows: Option<u16>,
}

/// Which of a session's terminals a socket is bound to. The agent socket and a shell socket
/// differ only here -- attach, poll, write and resize are the same raw byte relay either way.
enum Target {
    Agent,
    Shell(String),
}

impl Target {
    /// Ensures the terminal exists **and is sized for this client**, on every attach rather
    /// than only the first: a terminal another surface started keeps its original size, so a
    /// re-attach from a wide window would otherwise render into a narrow strip of it.
    fn attach(&self, state: &AppState, key: &str, size: (u16, u16)) -> Result<(), String> {
        match self {
            Self::Agent => agent::attach(state, key, size).map_err(|error| error.to_string()),
            Self::Shell(id) => {
                shells::attach(state, key, id, size).map_err(|error| error.to_string())
            }
        }
    }

    /// Runs `action` against the live terminal.
    ///
    /// Everything downstream reads through `ManagedTerminal::poll_raw`, never `sync()`: that
    /// would advance the parser and offset the TUI and other readers render from.
    ///
    /// The App-wide lock is *tried*, never waited on. A dashboard sharing this `App` holds it
    /// for as long as it has a session workspace attached, and blocking here would park a
    /// tokio worker per open socket until it came back -- taking the rest of the server, and
    /// every other session's terminal, down with it.
    fn with_terminal<T>(
        &self,
        state: &AppState,
        key: &str,
        action: impl FnOnce(&ManagedTerminal) -> T,
    ) -> Reached<T> {
        match self.resolve(state, key) {
            // The lookup is over by here, so the action -- which for a daemon-backed terminal
            // is a round trip to another process -- runs with no lock held at all.
            Reached::Ok(terminal) => Reached::Ok(action(&terminal)),
            Reached::Busy => Reached::Busy,
            Reached::Gone => Reached::Gone,
        }
    }

    /// Looks the terminal up and hands back an owned handle, releasing the lock on the way
    /// out. Its own function so the guard cannot outlive the lookup.
    fn resolve(&self, state: &AppState, key: &str) -> Reached<Arc<ManagedTerminal>> {
        let app = match state.app.try_lock() {
            Ok(app) => app,
            Err(TryLockError::WouldBlock) => return Reached::Busy,
            Err(TryLockError::Poisoned(_)) => return Reached::Gone,
        };
        let terminal = match self {
            Self::Agent => app.terminals.agent(key),
            Self::Shell(id) => app.terminals.shell(key, id),
        };
        terminal.map_or(Reached::Gone, Reached::Ok)
    }

    /// A write's version of [`Self::with_terminal`]: retries for [`WRITE_GRACE`] before
    /// giving up, yielding to the runtime between attempts instead of blocking it.
    async fn with_terminal_waiting<T>(
        &self,
        state: &AppState,
        key: &str,
        mut action: impl FnMut(&ManagedTerminal) -> T,
    ) -> Reached<T> {
        let deadline = tokio::time::Instant::now() + WRITE_GRACE;
        loop {
            match self.with_terminal(state, key, &mut action) {
                Reached::Busy if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(WRITE_RETRY).await;
                }
                other => return other,
            }
        }
    }
}

/// Whether a terminal lookup got through.
///
/// `Busy` is deliberately distinct from `Gone`: a socket recovers from the first by trying
/// again next tick, and must tear itself down for the second.
enum Reached<T> {
    Ok(T),
    Busy,
    Gone,
}

pub(crate) async fn ws_handler(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Query(query): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    upgrade(state, key, Target::Agent, query, ws)
}

pub(crate) async fn shell_ws_handler(
    State(state): State<AppState>,
    Path((key, id)): Path<(String, String)>,
    Query(query): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    upgrade(state, key, Target::Shell(id), query, ws)
}

fn upgrade(
    state: AppState,
    key: String,
    target: Target,
    query: WsQuery,
    ws: WebSocketUpgrade,
) -> Response {
    let cols = query.cols.filter(|value| *value > 0).unwrap_or(80);
    let rows = query.rows.filter(|value| *value > 0).unwrap_or(24);
    ws.on_upgrade(move |socket| handle_socket(socket, state, key, target, (cols, rows)))
}

async fn handle_socket(
    mut socket: WebSocket,
    state: AppState,
    key: String,
    target: Target,
    size: (u16, u16),
) {
    if let Err(error) = target.attach(&state, &key, size) {
        let _ = socket.send(exit_message(&error)).await;
        return;
    }

    let mut offset = 0u64;
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Only a change in the verdict is worth a frame: a held key repeats the refusal dozens of
    // times a second, and the browser only needs to be told once that typing is going nowhere.
    let mut denied = false;
    // This client has seen nothing of this terminal, so its first answer has to be a whole
    // snapshot -- the rows above the screen as well as the screen. Every poll after it is a
    // delta: asking again would hand the browser the same history a second time. It stays
    // set until a poll actually gets through, so a tick lost to a busy dashboard does not
    // cost the connection its scrollback.
    let mut snapshot = Scrollback::Include;

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                match poll_once(&state, &key, &target, offset, snapshot) {
                    // The dashboard is holding the shared state: no output this tick, and
                    // the next one picks up from the same offset.
                    Reached::Busy => {}
                    Reached::Gone => break,
                    Reached::Ok(Err(error)) => {
                        let _ = socket.send(exit_message(&error.to_string())).await;
                        break;
                    }
                    Reached::Ok(Ok(raw)) => {
                        snapshot = Scrollback::Omit;
                        if !apply_poll(&mut socket, &mut offset, raw).await {
                            break;
                        }
                    }
                }
            }
            incoming = socket.recv() => {
                let refused = match incoming {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                    Some(Ok(Message::Binary(bytes))) => {
                        write_to_terminal(&state, &key, &target, &bytes).await
                    }
                    Some(Ok(Message::Text(text))) => {
                        handle_text_frame(&state, &key, &target, &text).await
                    }
                    Some(Ok(_)) => false,
                };
                if refused && !denied {
                    let _ = socket.send(lease_denied_message()).await;
                }
                denied = refused;
            }
        }
    }
}

/// Sends a poll result's checkpoint/bytes, advances `offset`, and sends a final exit frame if
/// the terminal is no longer alive. Returns `false` when the caller should stop the loop.
async fn apply_poll(socket: &mut WebSocket, offset: &mut u64, raw: RawPoll) -> bool {
    debug_assert!(
        raw.start <= raw.end,
        "poll_raw returned a start offset past its own end: {} > {}",
        raw.start,
        raw.end
    );
    // Ahead of everything else, and only ever on the first poll: the rows above the screen,
    // which the browser writes into its own scrollback before the checkpoint repaints over
    // them. A text frame rather than more bytes, because the browser has to pad the rows out
    // to its own viewport height -- see the frontend's `scrollback` handler -- and only it
    // knows that height exactly.
    if let Some(rows) = &raw.scrollback
        && socket.send(scrollback_message(rows)).await.is_err()
    {
        return false;
    }
    let mut frames = Vec::new();
    frames.extend(raw.checkpoint);
    if !raw.bytes.is_empty() {
        frames.push(raw.bytes);
    }
    *offset = raw.end;
    if !send_all(socket, frames).await {
        return false;
    }
    if !raw.alive {
        let detail = raw.exit.unwrap_or_else(|| "process exited".to_owned());
        let _ = socket.send(exit_message(&detail)).await;
        return false;
    }
    true
}

async fn send_all(socket: &mut WebSocket, frames: Vec<Vec<u8>>) -> bool {
    for frame in frames {
        if socket.send(Message::binary(frame)).await.is_err() {
            return false;
        }
    }
    true
}

fn poll_once(
    state: &AppState,
    key: &str,
    target: &Target,
    offset: u64,
    scrollback: Scrollback,
) -> Reached<std::io::Result<RawPoll>> {
    target.with_terminal(state, key, |terminal| terminal.poll_raw(offset, scrollback))
}

/// Writes keystrokes, reporting `true` when the daemon refused them because another surface
/// holds the session's input lease.
///
/// Every other failure stays swallowed as it always was: a terminal that has gone away is
/// already reported by the poll side as an exit frame, and there is nothing a reader could do
/// about a transient write error. A lease denial is the one refusal with a remedy -- take the
/// lease over -- so it is the one the browser has to hear about, instead of keystrokes
/// vanishing with no feedback at all.
async fn write_to_terminal(state: &AppState, key: &str, target: &Target, bytes: &[u8]) -> bool {
    match target
        .with_terminal_waiting(state, key, |terminal| match terminal.write(bytes) {
            Err(error) => error.kind() == io::ErrorKind::PermissionDenied,
            Ok(()) => false,
        })
        .await
    {
        Reached::Ok(refused) => refused,
        Reached::Busy | Reached::Gone => false,
    }
}

#[derive(Deserialize)]
struct ResizeControl {
    #[serde(rename = "type")]
    kind: String,
    cols: u16,
    rows: u16,
}

/// A text frame is either a `{"type":"resize",...}` control message or a raw keystroke; any
/// text that doesn't parse as the former is written straight to the terminal as the latter.
///
/// Reports the same "refused for lack of the lease" verdict [`write_to_terminal`] does.
async fn handle_text_frame(state: &AppState, key: &str, target: &Target, text: &str) -> bool {
    if let Ok(resize) = serde_json::from_str::<ResizeControl>(text)
        && resize.kind == "resize"
    {
        target
            .with_terminal_waiting(state, key, |terminal| {
                let _ = terminal.resize(resize.cols, resize.rows);
            })
            .await;
        return false;
    }
    write_to_terminal(state, key, target, text.as_bytes()).await
}

/// The shape of every control frame this socket sends: a `type` the browser switches on and
/// a human-readable `detail` it can print.
#[derive(Serialize)]
struct ExitFrame<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    detail: &'a str,
}

fn exit_message(detail: &str) -> Message {
    control_message("exit", detail)
}

/// Everything the terminal printed before this socket existed, in one frame.
///
/// It carries text rather than bytes because the receiver has to append one blank line per
/// visible row before the checkpoint arrives: writing a row scrolls the row above it out of
/// the viewport, so without that tail the last screenful of these would still be *on* the
/// screen when the checkpoint's clear-screen erased it -- a band of lines missing from the
/// join. Only the browser knows its emulator's height, so only the browser can pad.
fn scrollback_message(text: &str) -> Message {
    let payload = serde_json::to_string(&ScrollbackFrame {
        kind: "scrollback",
        text,
    })
    .unwrap_or_else(|_| "{\"type\":\"scrollback\",\"text\":\"\"}".to_owned());
    Message::text(payload)
}

#[derive(Serialize)]
struct ScrollbackFrame<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
}

/// The stream is still live -- this says only that what was typed did not land, and names the
/// remedy the frontend turns into a "take over" button (`POST /api/sessions/{key}/lease`).
fn lease_denied_message() -> Message {
    control_message(
        "lease_denied",
        "another surface has this session open and holds its input; take it over to type here",
    )
}

fn control_message(kind: &'static str, detail: &str) -> Message {
    let payload = serde_json::to_string(&ExitFrame { kind, detail })
        .unwrap_or_else(|_| format!("{{\"type\":\"{kind}\"}}"));
    Message::text(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exit_frame_names_the_reason_the_stream_stopped() {
        let Message::Text(payload) = exit_message("process exited") else {
            panic!("an exit notice has to be a text frame the browser can parse");
        };
        let value: serde_json::Value = serde_json::from_str(payload.as_str()).unwrap();
        assert_eq!(value["type"], "exit");
        assert_eq!(value["detail"], "process exited");
    }

    /// The rows above the screen ride a frame of their own because only the browser can pad
    /// them out to its own viewport height before the checkpoint repaints over them. That
    /// makes it a frame the browser has to parse, and one it must not mistake for the two
    /// that tear a terminal down or raise a takeover dialog.
    #[test]
    fn the_scrollback_frame_carries_the_rows_as_text_the_browser_can_parse() {
        let rows = "one\u{1b}[m\r\ntwo\u{1b}[m";

        let Message::Text(payload) = scrollback_message(rows) else {
            panic!("the snapshot has to be a text frame the browser can parse");
        };

        let value: serde_json::Value = serde_json::from_str(payload.as_str()).unwrap();
        assert_eq!(value["type"], "scrollback");
        assert_eq!(value["text"], rows);
        assert_ne!(value["type"], "exit");
        assert_ne!(value["type"], "lease_denied");
    }

    /// A refused write has to be distinguishable from a stream that ended: the terminal is
    /// still there, the keystrokes are not, and only a takeover fixes it. Sharing the `exit`
    /// code would make the view tear the terminal down over something it can recover from.
    #[test]
    fn a_refused_write_reports_its_own_code_so_the_view_can_offer_a_takeover() {
        let Message::Text(payload) = lease_denied_message() else {
            panic!("a refusal has to be a text frame the browser can parse");
        };
        let value: serde_json::Value = serde_json::from_str(payload.as_str()).unwrap();
        assert_eq!(value["type"], "lease_denied");
        assert_ne!(value["type"], "exit");
        assert!(
            value["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("take it over")),
            "the frame has to say what the user can do about it"
        );
    }
}
