//! Getting at a session's agent terminal from a web request.
//!
//! Two callers want different things from the same terminal. A websocket attach owns a real
//! viewport and is counted as one of the terminal's viewers -- the PTY runs at the smallest
//! of them, so a phone joining a desktop never squashes the desktop. A prompt or an interrupt
//! has no viewport at all, so it is not a viewer and never changes the size.

use std::{
    fmt, io,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::http::StatusCode;

use crate::{model::Session, pty::ManagedTerminal};

use super::{AppState, screen::ScreenState};

/// Why a request could not reach an agent. Kept as distinct cases rather than one string so a
/// handler can pick a status without pattern-matching on prose.
pub(super) enum AgentError {
    UnknownSession(String),
    /// The agent is alive but never started reading input.
    NotReady(String),
    /// The agent is waiting on a dialog, so it would type the prompt into that instead.
    Blocked(String),
    /// Another surface -- a TUI, or another server -- holds this session's input lease, so
    /// the PTY daemon refused the write. Recoverable by taking the lease over.
    LeaseDenied,
    Terminal(io::Error),
}

impl AgentError {
    /// The status a handler answers with.
    ///
    /// `LeaseDenied` gets its own code (`423 Locked`) rather than joining the other
    /// conflicts under 409. It is the one failure here with a specific remedy -- POST the
    /// session's `/lease` with `force: true` -- and a frontend has to be able to tell it
    /// apart to offer that button instead of printing the daemon's sentence at the user.
    pub(super) fn status(&self) -> StatusCode {
        match self {
            Self::UnknownSession(_) => StatusCode::NOT_FOUND,
            // Not an error in the agent: it is alive and simply not listening yet, so the
            // client should surface the reason and let the user act, not retry blindly.
            Self::NotReady(_) | Self::Blocked(_) => StatusCode::CONFLICT,
            Self::LeaseDenied => StatusCode::LOCKED,
            Self::Terminal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// Turns a terminal failure into the case that describes it.
///
/// The PTY daemon reports a lost lease as `PermissionDenied` -- the same kind the TUI's own
/// takeover path uses -- which is the only signal distinguishing "someone else is driving
/// this session" from a genuinely broken terminal.
fn classify(error: io::Error) -> AgentError {
    if error.kind() == io::ErrorKind::PermissionDenied {
        AgentError::LeaseDenied
    } else {
        AgentError::Terminal(error)
    }
}

impl fmt::Display for AgentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSession(key) => write!(formatter, "no session with key {key}"),
            Self::NotReady(key) => write!(
                formatter,
                "session {key} is not accepting input yet; it may still be starting up"
            ),
            Self::Blocked(question) => write!(
                formatter,
                "the agent is waiting for an answer before it can take a prompt: {question}"
            ),
            Self::LeaseDenied => formatter.write_str(
                "another surface has this session open and holds its input; take it over to \
                 type here",
            ),
            Self::Terminal(error) => error.fmt(formatter),
        }
    }
}

/// Size used when a headless request (prompt, interrupt) is the first thing to start an
/// agent. Only ever applied at spawn -- an attached browser resizes to its own viewport, and
/// nothing headless takes that away from it.
const HEADLESS_SIZE: (u16, u16) = (120, 40);

/// Mirrors the TUI's own derivation in `App::prepare_selected_agent`: a session with no
/// transcript has never been seen by its provider, so resuming it by id (`claude --resume
/// <id>`, `codex resume <id>`) fails immediately. It has to be launched as a new session.
pub(super) fn is_new_session(session: &Session) -> bool {
    session.transcript_path.is_none()
}

/// Ensures the agent terminal exists and registers this client as one of its viewers.
///
/// `TerminalManager::ensure_agent` only applies `size` on the call that spawns the pty. A
/// terminal the TUI (or an earlier, narrower browser) already started keeps its original size
/// forever, so an attach from a wide window renders the agent into half of it -- which is why
/// attaching has to say how big it is at all.
///
/// It says so as a *viewer* rather than as an order. Every window looking at this PTY is
/// counted, and the size that comes back is the smallest of them: a phone joining a desktop
/// no longer squashes the desktop, and the desktop letterboxes the columns the PTY is not
/// using instead of reflowing the agent's output into them.
pub(super) fn attach(
    state: &AppState,
    key: &str,
    viewer: &str,
    size: (u16, u16),
) -> Result<(Arc<ManagedTerminal>, (u16, u16)), AgentError> {
    let terminal = with_agent(state, key, size, |terminal| Ok(Arc::clone(terminal)))?;
    let effective = terminal
        .resize_viewer(viewer, size.0, size.1)
        .map_err(classify)?;
    Ok((terminal, effective))
}

/// Ensures the agent terminal exists, then writes to it, leaving its size alone.
pub(super) fn write(state: &AppState, key: &str, bytes: &[u8]) -> Result<(), AgentError> {
    with_agent(state, key, HEADLESS_SIZE, |terminal| terminal.write(bytes))
}

/// The agent's visible screen, plus whether it is listening -- read together so both describe
/// the same instant.
///
/// Served from this layer's own parser (see `screen`), never from the terminal's shared one.
pub(super) fn screen_state(state: &AppState, key: &str) -> Result<ScreenState, AgentError> {
    with_agent(state, key, HEADLESS_SIZE, |terminal| {
        state.screens.lock().unwrap().read(key, terminal)
    })
}

/// Drops this layer's screen for a session whose terminals are going away.
pub(super) fn forget_screen(state: &AppState, key: &str) {
    state.screens.lock().unwrap().forget(key);
}

/// How long a prompt waits for a freshly spawned agent to start reading input.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
/// Gap between readiness checks. Each check takes the App lock only for its own duration.
const READY_POLL: Duration = Duration::from_millis(50);

/// Waits until the agent is listening and nothing is holding the keyboard, or gives up.
///
/// This is only a cheap first gate, not proof of readiness. Bracketed paste going on says the
/// agent started an input loop, but Claude Code turns it on *while* its trust dialog is still
/// up and while it is still painting its banner -- measured, not assumed -- so a prompt sent
/// on that signal alone can still land nowhere. The caller confirms delivery afterwards by
/// looking for its own text on screen; this just avoids typing into an obvious dialog.
///
/// The wait deliberately re-takes the App lock per check instead of holding it: a spawning
/// agent can take seconds, and holding the App-wide lock that long would stall the tick
/// thread and every concurrent request. Each individual check is a few microseconds.
pub(super) async fn await_input_ready(state: &AppState, key: &str) -> Result<(), AgentError> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let screen = screen_state(state, key)?;
        match super::dialog::detect(&screen.text) {
            None if screen.accepts_input => return Ok(()),
            blocking => {
                if Instant::now() >= deadline {
                    return Err(match blocking {
                        Some(prompt) => AgentError::Blocked(prompt.question),
                        None => AgentError::NotReady(key.to_owned()),
                    });
                }
            }
        }
        tokio::time::sleep(READY_POLL).await;
    }
}

fn with_agent<T>(
    state: &AppState,
    key: &str,
    size: (u16, u16),
    action: impl FnOnce(&Arc<ManagedTerminal>) -> io::Result<T>,
) -> Result<T, AgentError> {
    let mut app = state.app.lock().unwrap();
    let session = app
        .sessions
        .iter()
        .find(|session| session.key == key)
        .cloned()
        .ok_or_else(|| AgentError::UnknownSession(key.to_owned()))?;
    // No `wait_for_first_output` here, unlike the TUI's one-shot render: callers either stream
    // the output themselves or do not read it at all, so waiting would only stall this
    // App-wide lock for output that arrives anyway.
    let terminal = app
        .terminals
        .ensure_agent(&session, &state.current_exe, is_new_session(&session), size)
        .map_err(classify)?;
    action(&terminal).map_err(classify)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, thread, time::Duration, time::Instant};

    use tempfile::tempdir;

    use super::*;
    use crate::{app::App, config::AgentConsoleConfig, model::AgentKind, pty::TerminalManager};

    /// A stand-in for `codex`/`claude` that reports the terminal size it is running under
    /// every time it is prodded, so a test can observe resizes without a real provider.
    fn size_reporting_agent(root: &Path) -> std::path::PathBuf {
        let script = root.join("fake-agent.sh");
        fs::write(
            &script,
            "#!/bin/sh\nn=0\nwhile IFS= read -r line; do\n  n=$((n+1))\n  printf 'probe%s=%s\\n' \"$n\" \"$(stty size | tr ' ' 'x')\"\ndone\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&script).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&script, permissions).unwrap();
        }
        script
    }

    /// Prods the fake agent and waits for *that* probe's answer. Reading the last size in the
    /// capture without a probe number would keep returning the previous one.
    fn probe_size(terminal: &ManagedTerminal, probe: usize) -> String {
        let marker = format!("probe{probe}=");
        terminal.write(b"\n").unwrap();
        let start = Instant::now();
        loop {
            let capture = terminal.plain_capture();
            if let Some(size) = capture
                .split_whitespace()
                .find_map(|word| word.strip_prefix(marker.as_str()))
            {
                return size.to_owned();
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "probe {probe} never answered: {capture}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// The bug this module exists to prevent: `ensure_agent` honours `size` only when it
    /// spawns, so re-attaching at a new size silently leaves the agent at the old one.
    #[test]
    fn re_ensuring_an_agent_ignores_the_new_size_so_attach_has_to_resize() {
        let root = tempdir().unwrap();
        let script = size_reporting_agent(root.path());
        let config = AgentConsoleConfig::parse(
            &format!("[providers]\ncodex = [\"{}\"]\n", script.display()),
            Path::new("config.toml"),
        )
        .unwrap();
        let mut terminals = TerminalManager::new_local(config);
        let mut session = App::test_fixture().sessions[0].clone();
        session.agent = AgentKind::Codex;
        session.cwd = root.path().to_path_buf();
        session.transcript_path = None;

        let exe = Path::new("/usr/bin/true");
        let terminal = terminals
            .ensure_agent(&session, exe, true, (80, 24))
            .unwrap();
        assert_eq!(probe_size(&terminal, 1), "24x80");

        let terminal = terminals
            .ensure_agent(&session, exe, true, (200, 50))
            .unwrap();
        assert_eq!(
            probe_size(&terminal, 2),
            "24x80",
            "ensure_agent silently drops the size of an already-running terminal"
        );

        terminal.resize_viewer("test-viewer", 200, 50).unwrap();
        assert_eq!(
            probe_size(&terminal, 3),
            "50x200",
            "the explicit resize in attach() is what actually applies a client's viewport"
        );
        terminal.terminate();
    }

    #[test]
    fn a_session_without_a_transcript_launches_as_new_rather_than_resuming() {
        let app = App::test_fixture();
        let mut session = app.sessions[0].clone();
        session.transcript_path = None;

        assert!(
            is_new_session(&session),
            "a session the provider has never written a transcript for cannot be resumed by id"
        );
    }

    #[test]
    fn a_session_with_a_transcript_resumes_instead_of_starting_over() {
        let app = App::test_fixture();
        let mut session = app.sessions[0].clone();
        session.transcript_path = Some("/tmp/transcript.jsonl".into());

        assert!(!is_new_session(&session));
    }
}
