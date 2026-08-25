//! The web layer's own view of what an agent has on screen.
//!
//! The conversation view is built from the transcript, but some things never reach a
//! transcript: a provider's startup banner, and blocking dialogs like Claude Code's "trust
//! this folder" or a tool permission request. Reading the screen is the only way to see
//! those, and a prompt typed while one is up is swallowed.
//!
//! Each tracker owns a private `vt100::Parser` and a private byte offset, fed only from
//! [`ManagedTerminal::poll_raw`]. It deliberately does **not** reuse the terminal's own
//! parser: that state backs the TUI and the terminal websocket, and a screen read that
//! advanced their cursor would both corrupt what they render and make this read race them.
//! An earlier revision did exactly that, and the result was a screen signal that flapped --
//! `prompt-status` reporting a dialog that `/prompt` could not see moments later.

use std::{collections::HashMap, io};

use crate::pty::{ManagedTerminal, Scrollback};

/// Only the visible screen matters here, so no scrollback is retained.
const NO_SCROLLBACK: usize = 0;

/// What the agent has on screen right now.
pub(super) struct ScreenState {
    pub text: String,
    /// Whether the agent has enabled bracketed paste, i.e. whether it started its own input
    /// loop. Necessary but *not* sufficient for "ready": Claude Code turns it on while its
    /// trust dialog is still up, which is measured, not assumed.
    pub accepts_input: bool,
}

/// One terminal's screen, reconstructed independently of every other reader.
struct ScreenTracker {
    parser: vt100::Parser,
    /// This reader's own cursor into the terminal's output. The websocket keeps a separate
    /// one; the daemon serves both from the same ring buffer.
    offset: u64,
    size: (u16, u16),
}

impl ScreenTracker {
    fn new(size: (u16, u16)) -> Self {
        Self {
            parser: vt100::Parser::new(size.1, size.0, NO_SCROLLBACK),
            offset: 0,
            size,
        }
    }

    /// Pulls whatever the terminal has produced since the last read and folds it in.
    fn advance(&mut self, terminal: &ManagedTerminal) -> io::Result<()> {
        let size = terminal.size();
        if size != self.size {
            self.size = size;
            self.parser.screen_mut().set_size(size.1, size.0);
        }

        // Only the visible screen matters here, so the rows above it are never asked for.
        let poll = terminal.poll_raw(self.offset, Scrollback::Omit)?;
        // A checkpoint means our offset fell outside what the terminal still retains (a
        // respawn, or output we were too slow to collect). The checkpoint repaints the whole
        // screen from scratch, so anything the old parser held is stale.
        if poll.checkpoint.is_some() || poll.start != self.offset {
            self.parser = vt100::Parser::new(size.1, size.0, NO_SCROLLBACK);
        }
        if let Some(checkpoint) = &poll.checkpoint {
            self.parser.process(checkpoint);
        }
        self.parser.process(&poll.bytes);
        self.offset = poll.end;
        Ok(())
    }

    fn state(&self) -> ScreenState {
        let screen = self.parser.screen();
        ScreenState {
            text: screen.contents(),
            accepts_input: screen.bracketed_paste(),
        }
    }
}

/// Every session's screen, keyed the same way sessions are.
#[derive(Default)]
pub(super) struct ScreenTrackers {
    by_key: HashMap<String, ScreenTracker>,
}

impl ScreenTrackers {
    /// Brings this session's screen up to date and returns it.
    pub(super) fn read(
        &mut self,
        key: &str,
        terminal: &ManagedTerminal,
    ) -> io::Result<ScreenState> {
        let tracker = self
            .by_key
            .entry(key.to_owned())
            .or_insert_with(|| ScreenTracker::new(terminal.size()));
        tracker.advance(terminal)?;
        Ok(tracker.state())
    }

    /// Drops a session's screen, so a later session reusing the key starts clean rather than
    /// inheriting a dead terminal's last frame.
    pub(super) fn forget(&mut self, key: &str) {
        self.by_key.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds bytes through a tracker's parser the way `advance` would, without needing a real
    /// terminal to poll. The polling itself is exercised end to end by the server tests.
    fn tracker_after(size: (u16, u16), frames: &[&[u8]]) -> ScreenTracker {
        let mut tracker = ScreenTracker::new(size);
        for frame in frames {
            tracker.parser.process(frame);
            tracker.offset += frame.len() as u64;
        }
        tracker
    }

    #[test]
    fn the_screen_reads_back_what_the_terminal_drew() {
        let tracker = tracker_after((40, 6), &[b"hello ", b"world"]);

        assert!(tracker.state().text.contains("hello world"));
        assert_eq!(tracker.offset, 11, "the tracker keeps its own byte cursor");
    }

    #[test]
    fn bracketed_paste_is_reported_only_once_the_agent_turns_it_on() {
        let quiet = tracker_after((40, 6), &[b"starting up"]);
        assert!(!quiet.state().accepts_input);

        let listening = tracker_after((40, 6), &[b"starting up", b"\x1b[?2004h"]);
        assert!(listening.state().accepts_input);
    }

    #[test]
    fn a_checkpoint_replaces_the_screen_instead_of_appending_to_it() {
        let mut tracker = tracker_after((40, 6), &[b"stale frame"]);

        // What `advance` does when `poll_raw` hands back a checkpoint: throw the parser away
        // so the repaint is not layered on top of output it already supersedes.
        tracker.parser = vt100::Parser::new(tracker.size.1, tracker.size.0, NO_SCROLLBACK);
        tracker.parser.process(b"\x1b[2J\x1b[Hfresh frame");

        let text = tracker.state().text;
        assert!(text.contains("fresh frame"));
        assert!(
            !text.contains("stale frame"),
            "a checkpoint is a full repaint, so nothing from before it survives: {text}"
        );
    }

    #[test]
    fn resizing_reflows_the_tracked_screen() {
        let mut tracker = tracker_after((10, 4), &[b"0123456789abc"]);
        assert!(tracker.state().text.contains("0123456789"));

        tracker.size = (20, 4);
        tracker.parser.screen_mut().set_size(4, 20);
        tracker.parser.process(b"\x1b[2J\x1b[H0123456789abc");

        assert!(
            tracker.state().text.contains("0123456789abc"),
            "a wider screen holds the line that used to wrap"
        );
    }

    #[test]
    fn forgetting_a_session_drops_its_screen() {
        let mut trackers = ScreenTrackers::default();
        trackers
            .by_key
            .insert("claude:one".into(), ScreenTracker::new((40, 6)));

        trackers.forget("claude:one");

        assert!(trackers.by_key.is_empty());
    }
}
