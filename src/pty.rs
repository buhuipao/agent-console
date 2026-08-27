use std::{
    collections::{HashMap, VecDeque},
    env,
    ffi::{OsStr, OsString},
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::{
    net::{UnixListener, UnixStream},
    process::CommandExt,
};

use crossterm::SynchronizedUpdate;
#[cfg(not(unix))]
use crossterm::event;
#[cfg(any(not(unix), test))]
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use uuid::Uuid;

use crate::{
    clipboard,
    config::{AgentConsoleConfig, format_key_label},
    model::{AgentKind, Session},
    store::{ensure_private_dir, make_private_file},
};

/// Claude Code's switch for staying on the normal screen. Read from `claude`'s own build
/// (it sits alongside `CLAUDE_CODE_DISABLE_MOUSE` and `CLAUDE_CODE_SCROLL_SPEED`) and
/// verified against 2.1.247: with it set, no `\x1b[?1049h` is ever emitted.
const CLAUDE_ALTERNATE_SCREEN_VAR: &str = "CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN";

const CAPTURE_LINES: usize = 200;
const SCROLLBACK_LINES: usize = 2_000;

/// Bumped whenever the daemon learns to answer something a newer client depends on.
///
/// The daemon outlives the binary that started it -- it owns every running agent's PTY, so
/// nothing may restart it behind the user's back -- and the wire format tolerates a version
/// gap field by field, which is what made an upgrade degrade in silence: an older daemon
/// simply left `scrollback` out of its answer and browser terminals opened with no history
/// above the fold, with nothing anywhere saying why. Asking for the version says so.
///
/// 2 added the viewer registry -- `Resize` carries which viewer asked and answers with the
/// size the terminal actually ended up at, and `Detach` takes a viewer back out -- and the
/// environment a spawn carries. A daemon at 1 resizes to whoever asked last, which is the
/// several-viewers bug this exists to report, and starts Claude Code without the variable
/// that keeps it out of the alternate screen, which is a terminal with no history to scroll.
pub const DAEMON_PROTOCOL: u32 = 2;
const CAPTURE_BYTES: usize = 16 * 1024;
const RAW_CAPTURE_BYTES: usize = 128 * 1024;
const LEASE_STALE_AFTER: Duration = Duration::from_millis(500);
const ALTERNATE_REPAINT_SETTLE: Duration = Duration::from_millis(120);
const ALTERNATE_SCROLL_TIMEOUT: Duration = Duration::from_millis(350);
const ALTERNATE_SCROLL_QUEUE_LIMIT: usize = 4;
/// Capacity of `sockaddr_un.sun_path`, which a Unix socket path is copied into whole (plus a
/// NUL). 104 bytes on macOS/BSD, 108 on Linux.
const SUN_PATH_CAPACITY: usize = if cfg!(target_os = "linux") { 108 } else { 104 };
const ENABLE_MOUSE_REPORTING: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const DISABLE_MOUSE_REPORTING: &[u8] =
    b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1016l";
const ENABLE_KEYBOARD_ENHANCEMENT: &[u8] = b"\x1b[>1u";
const DISABLE_KEYBOARD_ENHANCEMENT: &[u8] = b"\x1b[<1u";

fn sync_keyboard_enhancement(
    output: &mut impl Write,
    enabled: &mut bool,
    focus: WorkspaceFocus,
) -> io::Result<()> {
    let should_enable = focus == WorkspaceFocus::Agent;
    if should_enable == *enabled {
        return Ok(());
    }
    output.write_all(if should_enable {
        ENABLE_KEYBOARD_ENHANCEMENT
    } else {
        DISABLE_KEYBOARD_ENHANCEMENT
    })?;
    *enabled = should_enable;
    Ok(())
}

#[derive(Clone, Debug)]
pub struct CommandSpec {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    /// Variables set on the child on top of the inherited environment.
    ///
    /// Some providers are configured this way rather than by flag -- Claude Code's
    /// alternate-screen switch is an environment variable where Codex's is `--no-alt-screen`
    /// -- so a spec has to be able to carry one.
    pub env: Vec<(OsString, OsString)>,
}

impl CommandSpec {
    pub fn new(program: impl Into<OsString>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: cwd.into(),
            env: Vec::new(),
        }
    }

    pub fn arg(mut self, value: impl Into<OsString>) -> Self {
        self.args.push(value.into());
        self
    }

    pub fn env(mut self, name: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((name.into(), value.into()));
        self
    }
}

#[derive(Debug, Default)]
struct OutputState {
    raw: VecDeque<u8>,
    base_offset: u64,
    exited: bool,
    exit_description: Option<String>,
}

struct TerminalOutputDelta {
    start: u64,
    end: u64,
    bytes: Vec<u8>,
    checkpoint: Option<Vec<u8>>,
    status_bar_rows: Option<Vec<Vec<u8>>>,
    scrollback: Option<String>,
    exit: Option<String>,
}

#[derive(Debug, Default)]
enum OutputScanState {
    #[default]
    Ground,
    Escape,
    Csi(Vec<u8>),
    String {
        escaped: bool,
    },
}

#[derive(Debug)]
enum OutputScanEvent {
    LineFeed,
    Csi { params: Vec<u8>, final_byte: u8 },
}

#[derive(Debug, Default)]
/// Captures rows removed from a top-anchored partial scroll region. Codex's inline TUI uses
/// this pattern to keep its composer fixed at the bottom, while `vt100` only records rows from
/// full-screen scrolling in its native scrollback buffer.
struct StatusBarScrollback {
    rows: VecDeque<Vec<u8>>,
    offset: usize,
    scroll_region: Option<(u16, u16)>,
    scan_state: OutputScanState,
}

impl StatusBarScrollback {
    fn process(&mut self, parser: &mut vt100::Parser, bytes: &[u8]) {
        // Feed ordinary output to vte in batches, splitting only where a scroll operation needs
        // a snapshot of the row that is about to leave the visible region.
        let mut unprocessed = 0;
        for (index, &byte) in bytes.iter().enumerate() {
            let event = self.scan(byte);
            match event {
                Some(OutputScanEvent::LineFeed) => {
                    parser.process(&bytes[unprocessed..index]);
                    self.capture_scrolled_rows(parser, 1);
                    parser.process(&bytes[index..=index]);
                    unprocessed = index + 1;
                }
                Some(OutputScanEvent::Csi {
                    params,
                    final_byte: b'S',
                }) => {
                    parser.process(&bytes[unprocessed..index]);
                    self.capture_scrolled_rows(parser, csi_count(&params));
                    parser.process(&bytes[index..=index]);
                    unprocessed = index + 1;
                }
                Some(OutputScanEvent::Csi {
                    params,
                    final_byte: b'r',
                }) => {
                    parser.process(&bytes[unprocessed..=index]);
                    self.set_scroll_region(&params, parser.screen().size().0);
                    unprocessed = index + 1;
                }
                _ => {}
            }
        }
        if unprocessed < bytes.len() {
            parser.process(&bytes[unprocessed..]);
        }
    }

    fn scan(&mut self, byte: u8) -> Option<OutputScanEvent> {
        let state = std::mem::take(&mut self.scan_state);
        let (next, event) = match state {
            OutputScanState::Ground if byte == 0x1b => (OutputScanState::Escape, None),
            OutputScanState::Ground if byte == b'\n' => {
                (OutputScanState::Ground, Some(OutputScanEvent::LineFeed))
            }
            OutputScanState::Ground => (OutputScanState::Ground, None),
            OutputScanState::Escape if byte == b'[' => (OutputScanState::Csi(Vec::new()), None),
            OutputScanState::Escape if matches!(byte, b']' | b'P' | b'X' | b'^' | b'_') => {
                (OutputScanState::String { escaped: false }, None)
            }
            OutputScanState::Escape if byte == 0x1b => (OutputScanState::Escape, None),
            OutputScanState::Escape => (OutputScanState::Ground, None),
            OutputScanState::Csi(_params) if byte == 0x1b => (OutputScanState::Escape, None),
            OutputScanState::Csi(params) if (0x40..=0x7e).contains(&byte) => (
                OutputScanState::Ground,
                Some(OutputScanEvent::Csi {
                    params,
                    final_byte: byte,
                }),
            ),
            OutputScanState::Csi(mut params) => {
                if (0x20..=0x3f).contains(&byte) {
                    params.push(byte);
                }
                (OutputScanState::Csi(params), None)
            }
            OutputScanState::String { .. } if byte == 0x07 => (OutputScanState::Ground, None),
            OutputScanState::String { escaped: true } if byte == b'\\' => {
                (OutputScanState::Ground, None)
            }
            OutputScanState::String { .. } if byte == 0x1b => {
                (OutputScanState::String { escaped: true }, None)
            }
            OutputScanState::String { .. } => (OutputScanState::String { escaped: false }, None),
        };
        self.scan_state = next;
        event
    }

    fn set_scroll_region(&mut self, params: &[u8], screen_rows: u16) {
        self.scroll_region = parse_scroll_region(params, screen_rows);
    }

    fn capture_scrolled_rows(&mut self, parser: &vt100::Parser, count: usize) {
        let screen = parser.screen();
        let (screen_rows, cols) = screen.size();
        let Some((top, bottom)) = self.scroll_region else {
            return;
        };
        if top != 0
            || bottom >= screen_rows.saturating_sub(1)
            || screen.cursor_position().0 != bottom
        {
            return;
        }
        let count = count.min(usize::from(bottom - top + 1));
        for row in screen.rows_formatted(0, cols).take(count) {
            if self.rows.len() == SCROLLBACK_LINES {
                self.rows.pop_front();
            }
            self.rows.push_back(row);
            if self.offset > 0 {
                self.offset = self.offset.saturating_add(1).min(self.rows.len());
            }
        }
    }

    fn scroll(&mut self, rows: isize) -> usize {
        self.offset = if rows >= 0 {
            self.offset
                .saturating_add(rows as usize)
                .min(self.rows.len())
        } else {
            self.offset.saturating_sub(rows.unsigned_abs())
        };
        self.offset
    }

    fn live_tail(&mut self) {
        self.offset = 0;
    }

    fn screen_rows(&self, live_rows: &[Vec<u8>]) -> Option<Vec<Vec<u8>>> {
        if self.offset == 0 || self.rows.is_empty() {
            return None;
        }
        let height = live_rows.len();
        let end = self
            .rows
            .len()
            .saturating_add(height)
            .saturating_sub(self.offset);
        let start = end.saturating_sub(height);
        Some(
            (start..end)
                .filter_map(|index| {
                    self.rows.get(index).cloned().or_else(|| {
                        live_rows
                            .get(index.saturating_sub(self.rows.len()))
                            .cloned()
                    })
                })
                .collect(),
        )
    }

    fn resize(&mut self, rows: u16) {
        if self
            .scroll_region
            .is_some_and(|(top, bottom)| top >= rows || bottom >= rows || top >= bottom)
        {
            self.scroll_region = None;
        }
    }
}

fn csi_count(params: &[u8]) -> usize {
    std::str::from_utf8(params)
        .ok()
        .and_then(|value| value.split(';').next())
        .and_then(|value| value.parse().ok())
        .unwrap_or(1)
}

fn parse_scroll_region(params: &[u8], screen_rows: u16) -> Option<(u16, u16)> {
    if params.is_empty() {
        return None;
    }
    let value = std::str::from_utf8(params).ok()?;
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || byte == b';')
    {
        return None;
    }
    let mut values = value.split(';');
    let top = values.next()?.parse::<u16>().ok()?.saturating_sub(1);
    let bottom = values
        .next()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(screen_rows)
        .saturating_sub(1);
    (top < bottom && bottom < screen_rows).then_some((top, bottom))
}

fn process_terminal_output(
    parser: &mut vt100::Parser,
    status_bar_scrollback: &mut StatusBarScrollback,
    bytes: &[u8],
) {
    status_bar_scrollback.process(parser, bytes);
}

fn terminal_screen_view(
    parser: &vt100::Parser,
    status_bar_scrollback: &StatusBarScrollback,
) -> ScreenView {
    let screen = parser.screen();
    let (_, cols) = screen.size();
    let live_rows = screen.rows_formatted(0, cols).collect::<Vec<_>>();
    let rows = status_bar_scrollback
        .screen_rows(&live_rows)
        .unwrap_or(live_rows);
    ScreenView {
        rows,
        size: screen.size(),
        cursor: screen.cursor_position(),
        hide_cursor: screen.hide_cursor()
            || screen.scrollback() > 0
            || status_bar_scrollback.offset > 0,
    }
}

fn terminal_history_len(
    parser: &mut vt100::Parser,
    status_bar_scrollback: &StatusBarScrollback,
) -> usize {
    if !status_bar_scrollback.rows.is_empty() {
        return status_bar_scrollback.rows.len();
    }
    let screen = parser.screen_mut();
    let original = screen.scrollback();
    screen.set_scrollback(usize::MAX);
    let history_len = screen.scrollback();
    screen.set_scrollback(original);
    history_len
}

fn terminal_buffer_cell(
    parser: &mut vt100::Parser,
    status_bar_scrollback: &StatusBarScrollback,
    cell: TerminalCell,
) -> Option<TerminalBufferCell> {
    let (rows, cols) = parser.screen().size();
    if rows == 0 || cols == 0 {
        return None;
    }
    let cell = cell.clamped(rows, cols);
    let history_len = terminal_history_len(parser, status_bar_scrollback);
    let offset = if status_bar_scrollback.rows.is_empty() {
        parser.screen().scrollback()
    } else {
        status_bar_scrollback.offset
    };
    Some(TerminalBufferCell {
        row: history_len
            .saturating_sub(offset)
            .saturating_add(usize::from(cell.row)),
        col: cell.col,
    })
}

fn terminal_retained_rows(
    parser: &mut vt100::Parser,
    status_bar_scrollback: &StatusBarScrollback,
) -> Vec<Vec<u8>> {
    let (height, cols) = parser.screen().size();
    if height == 0 || cols == 0 {
        return Vec::new();
    }
    if !status_bar_scrollback.rows.is_empty() {
        let screen = parser.screen_mut();
        let original = screen.scrollback();
        screen.set_scrollback(0);
        let live_rows = screen.rows_formatted(0, cols).collect::<Vec<_>>();
        screen.set_scrollback(original);
        return status_bar_scrollback
            .rows
            .iter()
            .cloned()
            .chain(live_rows)
            .collect();
    }

    let screen = parser.screen_mut();
    let original = screen.scrollback();
    screen.set_scrollback(usize::MAX);
    let history_len = screen.scrollback();
    let total = history_len.saturating_add(usize::from(height));
    let mut rows = Vec::with_capacity(total);
    let mut next = 0;
    // vt100 exposes only one viewport at a time. Walk the retained grid in viewport-sized
    // windows, accounting for the overlap when the history length is not a multiple of height.
    while next < total {
        let offset = history_len.saturating_sub(next.min(history_len));
        screen.set_scrollback(offset);
        let view_top = history_len.saturating_sub(offset);
        let skip = next.saturating_sub(view_top);
        let take = usize::from(height)
            .saturating_sub(skip)
            .min(total.saturating_sub(next));
        rows.extend(screen.rows_formatted(0, cols).skip(skip).take(take));
        next = next.saturating_add(take);
    }
    screen.set_scrollback(original);
    rows
}

/// Whether a raw poll carries the rows retained *above* the terminal's current screen.
///
/// A client attaching for the first time needs them. A checkpoint is exactly one screenful,
/// so on its own it leaves the client's emulator with an empty scrollback and nothing above
/// the fold -- everything printed before the client arrived is simply not there. Every later
/// poll of the same connection must say [`Scrollback::Omit`], or the client would be handed
/// the same rows again on top of the ones it already has.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scrollback {
    Omit,
    Include,
}

/// The rows this terminal retains above its current screen, oldest first, ANSI-formatted and
/// CRLF-joined -- ready to be written into an emulator immediately before a checkpoint.
///
/// The two halves come from one parser under one lock, at one instant: this is
/// [`terminal_retained_rows`] with the visible screen -- which the checkpoint already carries
/// -- dropped off the end. That is what makes them a partition of a single snapshot rather
/// than two reads that have to be aligned afterwards, and it is why no band of lines can end
/// up duplicated or missing where they meet.
///
/// Each row closes its own colour: `rows_formatted` emits only the attributes a row needs, so
/// a row that ends mid-colour would otherwise bleed into the next one.
fn terminal_scrollback_snapshot(
    parser: &mut vt100::Parser,
    status_bar_scrollback: &StatusBarScrollback,
) -> Option<String> {
    let height = usize::from(parser.screen().size().0);
    let mut rows = terminal_retained_rows(parser, status_bar_scrollback);
    rows.truncate(rows.len().saturating_sub(height));
    // A bounded scrollback evicts its blank starting rows before it ever evicts a line of
    // text, so the oldest rows are usually padding. Left in, they make the top of the
    // history look empty; anything blank further down is real spacing and stays.
    let first = rows
        .iter()
        .position(|row| !plain_text(row).trim().is_empty())?;
    Some(
        rows[first..]
            .iter()
            .map(|row| format!("{}\u{1b}[m", String::from_utf8_lossy(row)))
            .collect::<Vec<_>>()
            .join("\r\n"),
    )
}

fn selected_row_text(formatted: Option<&[u8]>, cols: u16, first: u16, last: u16) -> String {
    let width = last - first + 1;
    let mut row_parser = vt100::Parser::new(1, cols, 0);
    if let Some(formatted) = formatted {
        row_parser.process(formatted);
    }
    let text = row_parser
        .screen()
        .contents_between(0, first, 0, last.saturating_add(1));
    fit_text(&text, width)
}

fn normalized_buffer_selection(
    first: TerminalBufferCell,
    second: TerminalBufferCell,
    rows: usize,
    cols: u16,
) -> Option<(TerminalBufferCell, TerminalBufferCell)> {
    if rows == 0 || cols == 0 {
        return None;
    }
    let mut start = first.clamped(rows, cols);
    let mut end = second.clamped(rows, cols);
    if (start.row, start.col) > (end.row, end.col) {
        std::mem::swap(&mut start, &mut end);
    }
    Some((start, end))
}

fn terminal_selected_rows(
    parser: &mut vt100::Parser,
    status_bar_scrollback: &StatusBarScrollback,
    first: TerminalBufferCell,
    second: TerminalBufferCell,
) -> Vec<(TerminalCell, String)> {
    let (height, cols) = parser.screen().size();
    let history_len = terminal_history_len(parser, status_bar_scrollback);
    let total_rows = history_len.saturating_add(usize::from(height));
    let Some((start, end)) = normalized_buffer_selection(first, second, total_rows, cols) else {
        return Vec::new();
    };
    let offset = if status_bar_scrollback.rows.is_empty() {
        parser.screen().scrollback()
    } else {
        status_bar_scrollback.offset
    };
    let view_top = history_len.saturating_sub(offset);
    let view_end = view_top.saturating_add(usize::from(height));
    let visible_start = start.row.max(view_top);
    let visible_end = end.row.min(view_end.saturating_sub(1));
    if visible_start > visible_end {
        return Vec::new();
    }
    let view = terminal_screen_view(parser, status_bar_scrollback);
    (visible_start..=visible_end)
        .map(|buffer_row| {
            let row = u16::try_from(buffer_row.saturating_sub(view_top)).unwrap_or(u16::MAX);
            let first_col = if buffer_row == start.row {
                start.col
            } else {
                0
            };
            let last_col = if buffer_row == end.row {
                end.col
            } else {
                cols - 1
            };
            (
                TerminalCell {
                    row,
                    col: first_col,
                },
                selected_row_text(
                    view.rows.get(usize::from(row)).map(Vec::as_slice),
                    cols,
                    first_col,
                    last_col,
                ),
            )
        })
        .collect()
}

fn terminal_selected_text(
    parser: &mut vt100::Parser,
    status_bar_scrollback: &StatusBarScrollback,
    first: TerminalBufferCell,
    second: TerminalBufferCell,
) -> String {
    let (_, cols) = parser.screen().size();
    let rows = terminal_retained_rows(parser, status_bar_scrollback);
    let Some((start, end)) = normalized_buffer_selection(first, second, rows.len(), cols) else {
        return String::new();
    };
    (start.row..=end.row)
        .map(|row| {
            let first_col = if row == start.row { start.col } else { 0 };
            let last_col = if row == end.row { end.col } else { cols - 1 };
            selected_row_text(rows.get(row).map(Vec::as_slice), cols, first_col, last_col)
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn terminal_state_checkpoint(
    parser: &vt100::Parser,
    status_bar_scrollback: &StatusBarScrollback,
) -> Vec<u8> {
    let screen = parser.screen();
    let mut checkpoint = Vec::new();
    if screen.alternate_screen() {
        checkpoint.extend_from_slice(b"\x1b[?1049h");
    }
    if let Some((top, bottom)) = status_bar_scrollback.scroll_region {
        checkpoint.extend_from_slice(
            format!(
                "\x1b[{};{}r",
                top.saturating_add(1),
                bottom.saturating_add(1)
            )
            .as_bytes(),
        );
    }
    checkpoint.extend(screen.state_formatted());
    checkpoint
}

/// A session we spawn is a new, independent agent session -- it is not a child of whatever
/// session happened to launch the console. Inheriting these makes the provider believe
/// otherwise: `CLAUDE_CODE_CHILD_SESSION` silently turns transcript saving off, which leaves
/// discovery and the whole conversation view permanently empty, and the rest point the child
/// at the launcher's session id and IPC socket.
const INHERITED_SESSION_VARS: [&str; 5] = [
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_ENTRYPOINT",
];

struct LocalTerminal {
    /// Behind a mutex only so this terminal is `Sync`: a `MasterPty` is `Send` but not
    /// `Sync`, and without this the whole terminal could not be shared through an `Arc` --
    /// which is what lets a websocket and a workspace hold the same terminal and use it with
    /// no lock of their own held.
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    output: Arc<Mutex<OutputState>>,
    output_generation: Arc<AtomicU64>,
    parser: Arc<Mutex<vt100::Parser>>,
    status_bar_scrollback: Arc<Mutex<StatusBarScrollback>>,
    /// Every window currently looking at this terminal, and the size each one asked for.
    /// See [`smallest_viewer`] for why the PTY is sized from all of them rather than from
    /// whichever one spoke last.
    viewers: Mutex<ViewerSizes>,
}

impl LocalTerminal {
    pub fn spawn(spec: &CommandSpec, size: (u16, u16)) -> io::Result<Self> {
        let size = normalized_size(size);
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: size.1,
                cols: size.0,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io::Error::other)?;
        let mut command = CommandBuilder::new(&spec.program);
        command.args(&spec.args);
        command.cwd(&spec.cwd);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        for name in INHERITED_SESSION_VARS {
            command.env_remove(name);
        }
        // After the removals, so a spec can deliberately set one of them back.
        for (name, value) in &spec.env {
            command.env(name, value);
        }
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(io::Error::other)?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().map_err(io::Error::other)?;
        let writer = Arc::new(Mutex::new(
            pair.master.take_writer().map_err(io::Error::other)?,
        ));
        let writer_for_thread = Arc::clone(&writer);

        let output = Arc::new(Mutex::new(OutputState::default()));
        let output_for_thread = Arc::clone(&output);
        let output_generation = Arc::new(AtomicU64::new(0));
        let generation_for_thread = Arc::clone(&output_generation);
        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            size.1,
            size.0,
            SCROLLBACK_LINES,
        )));
        let parser_for_thread = Arc::clone(&parser);
        let status_bar_scrollback = Arc::new(Mutex::new(StatusBarScrollback::default()));
        let status_bar_scrollback_for_thread = Arc::clone(&status_bar_scrollback);

        thread::Builder::new()
            .name("agent-console-pty-output".into())
            .spawn(move || {
                let mut query_router = TerminalQueryRouter::default();
                let mut buffer = [0_u8; 8 * 1024];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => {
                            let bytes = &buffer[..read];
                            let responses = {
                                let mut parser = parser_for_thread.lock().unwrap();
                                let mut scrollback =
                                    status_bar_scrollback_for_thread.lock().unwrap();
                                process_terminal_output(&mut parser, &mut scrollback, bytes);
                                let responses =
                                    query_router.route(bytes, parser.screen().cursor_position());
                                // Keep the parsed screen and retained raw offset in one atomic
                                // order. Poll can then create a checkpoint that corresponds
                                // exactly to the returned end offset.
                                let mut state = output_for_thread.lock().unwrap();
                                state.raw.extend(bytes);
                                while state.raw.len() > RAW_CAPTURE_BYTES {
                                    state.raw.pop_front();
                                    state.base_offset = state.base_offset.saturating_add(1);
                                }
                                responses
                            };
                            if !responses.is_empty() {
                                let mut writer = writer_for_thread.lock().unwrap();
                                for response in responses {
                                    let _ = writer.write_all(&response);
                                }
                                let _ = writer.flush();
                            }
                            generation_for_thread.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                output_for_thread.lock().unwrap().exited = true;
                generation_for_thread.fetch_add(1, Ordering::Relaxed);
            })
            .map_err(io::Error::other)?;

        Ok(Self {
            master: Mutex::new(pair.master),
            writer,
            child: Arc::new(Mutex::new(child)),
            output,
            output_generation,
            parser,
            status_bar_scrollback,
            viewers: Mutex::new(ViewerSizes::new()),
        })
    }

    pub fn is_alive(&self) -> bool {
        let reader_exited = self.output.lock().unwrap().exited;
        let mut child = self.child.lock().unwrap();
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut output = self.output.lock().unwrap();
                output.exited = true;
                output.exit_description = Some(format!("{status:?}"));
                false
            }
            Ok(None) => !reader_exited,
            Err(error) => {
                let mut output = self.output.lock().unwrap();
                output.exited = true;
                output.exit_description = Some(error.to_string());
                false
            }
        }
    }

    pub fn write(&self, bytes: &[u8]) -> io::Result<()> {
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(bytes)?;
        writer.flush()
    }

    pub fn wait_for_first_output(&self, timeout: Duration) {
        let start = Instant::now();
        while self.output_generation.load(Ordering::Relaxed) == 0 && start.elapsed() < timeout {
            thread::sleep(Duration::from_millis(25));
        }
    }

    pub fn plain_capture(&self) -> String {
        let raw = self
            .output
            .lock()
            .unwrap()
            .raw
            .iter()
            .copied()
            .collect::<Vec<_>>();
        plain_text(&raw)
    }

    fn exit_description(&self) -> Option<String> {
        let _ = self.is_alive();
        self.output.lock().unwrap().exit_description.clone()
    }

    fn screen_view(&self) -> ScreenView {
        let parser = self.parser.lock().unwrap();
        let scrollback = self.status_bar_scrollback.lock().unwrap();
        terminal_screen_view(&parser, &scrollback)
    }

    fn scroll_viewport(&self, rows: isize) -> usize {
        let mut parser = self.parser.lock().unwrap();
        let mut status_bar_scrollback = self.status_bar_scrollback.lock().unwrap();
        if !status_bar_scrollback.rows.is_empty() {
            parser.screen_mut().set_scrollback(0);
            let applied = status_bar_scrollback.scroll(rows);
            drop(status_bar_scrollback);
            drop(parser);
            self.output_generation.fetch_add(1, Ordering::Relaxed);
            return applied;
        }
        let screen = parser.screen_mut();
        let current = screen.scrollback();
        let requested = if rows >= 0 {
            current.saturating_add(rows as usize)
        } else {
            current.saturating_sub(rows.unsigned_abs())
        };
        screen.set_scrollback(requested);
        let applied = screen.scrollback();
        drop(parser);
        self.output_generation.fetch_add(1, Ordering::Relaxed);
        applied
    }

    fn scroll_to_live_tail(&self) {
        let mut parser = self.parser.lock().unwrap();
        self.status_bar_scrollback.lock().unwrap().live_tail();
        parser.screen_mut().set_scrollback(0);
        drop(parser);
        self.output_generation.fetch_add(1, Ordering::Relaxed);
    }

    fn scrollback_offset(&self) -> usize {
        let parser = self.parser.lock().unwrap();
        let status_bar_scrollback = self.status_bar_scrollback.lock().unwrap();
        if status_bar_scrollback.rows.is_empty() {
            parser.screen().scrollback()
        } else {
            status_bar_scrollback.offset
        }
    }

    fn mouse_protocol(&self) -> (vt100::MouseProtocolMode, vt100::MouseProtocolEncoding) {
        let parser = self.parser.lock().unwrap();
        let screen = parser.screen();
        (
            screen.mouse_protocol_mode(),
            screen.mouse_protocol_encoding(),
        )
    }

    fn alternate_screen(&self) -> bool {
        self.parser.lock().unwrap().screen().alternate_screen()
    }

    fn size(&self) -> (u16, u16) {
        let parser = self.parser.lock().unwrap();
        let (rows, cols) = parser.screen().size();
        (cols, rows)
    }

    fn selection_cell(&self, cell: TerminalCell) -> Option<TerminalBufferCell> {
        let mut parser = self.parser.lock().unwrap();
        let scrollback = self.status_bar_scrollback.lock().unwrap();
        terminal_buffer_cell(&mut parser, &scrollback, cell)
    }

    fn selected_text(&self, first: TerminalBufferCell, second: TerminalBufferCell) -> String {
        let mut parser = self.parser.lock().unwrap();
        let scrollback = self.status_bar_scrollback.lock().unwrap();
        terminal_selected_text(&mut parser, &scrollback, first, second)
    }

    fn selected_rows(
        &self,
        first: TerminalBufferCell,
        second: TerminalBufferCell,
    ) -> Vec<(TerminalCell, String)> {
        let mut parser = self.parser.lock().unwrap();
        let scrollback = self.status_bar_scrollback.lock().unwrap();
        terminal_selected_rows(&mut parser, &scrollback, first, second)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let (cols, rows) = normalized_size((cols, rows));
        self.master
            .lock()
            .unwrap()
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io::Error::other)?;
        self.parser
            .lock()
            .unwrap()
            .screen_mut()
            .set_size(rows, cols);
        self.status_bar_scrollback.lock().unwrap().resize(rows);
        self.output_generation.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Records what one viewer wants and resizes the PTY to fit every viewer, answering
    /// with the size it actually ended up at. See [`smallest_viewer`].
    fn resize_viewer(&self, viewer: &str, cols: u16, rows: u16) -> io::Result<(u16, u16)> {
        let target = {
            let mut viewers = self.viewers.lock().unwrap();
            viewers.insert(viewer.to_owned(), normalized_size((cols, rows)));
            drop_dead_viewers(&mut viewers);
            smallest_viewer(&viewers)
        };
        self.apply_viewer_size(target)
    }

    /// Takes a viewer back out, growing the terminal again when it was the small one.
    ///
    /// The last viewer leaving does *not* resize: nothing is looking, so any size is as good
    /// as another, and the agent would only be made to repaint for nobody.
    fn detach_viewer(&self, viewer: &str) -> io::Result<(u16, u16)> {
        let target = {
            let mut viewers = self.viewers.lock().unwrap();
            viewers.remove(viewer);
            drop_dead_viewers(&mut viewers);
            smallest_viewer(&viewers)
        };
        self.apply_viewer_size(target)
    }

    fn apply_viewer_size(&self, target: Option<(u16, u16)>) -> io::Result<(u16, u16)> {
        let Some(target) = target else {
            return Ok(self.size());
        };
        if self.size() != target {
            self.resize(target.0, target.1)?;
        }
        Ok(target)
    }

    pub fn terminate(&self) {
        let mut child = self.child.lock().unwrap();
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
        }
    }

    /// The output a caller has not seen yet, and -- for a caller attaching from nothing --
    /// the rows above the current screen as well.
    ///
    /// The two answers are exclusive by construction. Either the requested offset is still
    /// inside the retained ring, in which case the bytes returned start at the terminal's
    /// very first byte and rebuild the whole history on their own; or it is not, in which
    /// case a checkpoint stands in for the screen and [`Scrollback::Include`] adds the rows
    /// above it. Both halves of that second answer are taken here, under one parser lock, at
    /// one instant -- which is what leaves no seam between them to align.
    fn output_since(&self, requested: u64, scrollback_wanted: Scrollback) -> TerminalOutputDelta {
        let mut parser = self.parser.lock().unwrap();
        let scrollback = self.status_bar_scrollback.lock().unwrap();
        let state = self.output.lock().unwrap();
        let end = state.base_offset.saturating_add(state.raw.len() as u64);
        if requested < state.base_offset || requested > end {
            let checkpoint = terminal_state_checkpoint(&parser, &scrollback);
            let retained = (scrollback_wanted == Scrollback::Include)
                .then(|| terminal_scrollback_snapshot(&mut parser, &scrollback))
                .flatten();
            return TerminalOutputDelta {
                start: end,
                end,
                bytes: Vec::new(),
                checkpoint: Some(checkpoint),
                status_bar_rows: Some(scrollback.rows.iter().cloned().collect()),
                scrollback: retained,
                exit: state.exit_description.clone(),
            };
        }
        let start = requested;
        let skip = start.saturating_sub(state.base_offset) as usize;
        let bytes = state.raw.iter().skip(skip).copied().collect::<Vec<_>>();
        TerminalOutputDelta {
            start,
            end,
            bytes,
            checkpoint: None,
            status_bar_rows: None,
            scrollback: None,
            exit: state.exit_description.clone(),
        }
    }

    /// Backend-agnostic raw poll for a caller that owns its own offset cursor and does not
    /// share this terminal's parser/output state (e.g. the web server).
    pub(crate) fn poll_raw(&self, offset: u64, scrollback: Scrollback) -> RawPoll {
        let alive = self.is_alive();
        let delta = self.output_since(offset, scrollback);
        RawPoll {
            start: delta.start,
            end: delta.end,
            bytes: delta.bytes,
            checkpoint: delta.checkpoint,
            scrollback: delta.scrollback,
            size: self.size(),
            alive,
            exit: delta.exit,
        }
    }
}

#[derive(Clone, Debug)]
struct ScreenView {
    rows: Vec<Vec<u8>>,
    size: (u16, u16),
    cursor: (u16, u16),
    hide_cursor: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TerminalCell {
    row: u16,
    col: u16,
}

impl TerminalCell {
    fn clamped(self, rows: u16, cols: u16) -> Self {
        Self {
            row: self.row.min(rows.saturating_sub(1)),
            col: self.col.min(cols.saturating_sub(1)),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TerminalBufferCell {
    // Physical row counted from the oldest row still retained by this terminal.
    row: usize,
    col: u16,
}

impl TerminalBufferCell {
    fn clamped(self, rows: usize, cols: u16) -> Self {
        Self {
            row: self.row.min(rows.saturating_sub(1)),
            col: self.col.min(cols.saturating_sub(1)),
        }
    }
}

#[derive(Clone, Copy)]
enum TerminalQuery {
    CursorPosition,
    KeyboardMode,
    DeviceAttributes,
}

#[derive(Default)]
struct TerminalQueryRouter {
    pending: Vec<u8>,
}

impl TerminalQueryRouter {
    fn route(&mut self, input: &[u8], cursor: (u16, u16)) -> Vec<Vec<u8>> {
        let mut responses = Vec::new();
        for &byte in input {
            self.pending.push(byte);
            loop {
                if let Some(query) = terminal_query(&self.pending) {
                    self.pending.clear();
                    responses.push(match query {
                        TerminalQuery::CursorPosition => {
                            format!("\x1b[{};{}R", cursor.0 + 1, cursor.1 + 1).into_bytes()
                        }
                        TerminalQuery::KeyboardMode => b"\x1b[?0u".to_vec(),
                        TerminalQuery::DeviceAttributes => b"\x1b[?1;2c".to_vec(),
                    });
                    break;
                }
                if is_terminal_query_prefix(&self.pending) {
                    break;
                }
                self.pending.remove(0);
                if self.pending.is_empty() {
                    break;
                }
            }
        }
        responses
    }
}

fn terminal_query(input: &[u8]) -> Option<TerminalQuery> {
    match input {
        b"\x1b[6n" => Some(TerminalQuery::CursorPosition),
        b"\x1b[?u" => Some(TerminalQuery::KeyboardMode),
        b"\x1b[c" => Some(TerminalQuery::DeviceAttributes),
        _ => None,
    }
}

fn is_terminal_query_prefix(input: &[u8]) -> bool {
    [
        b"\x1b[6n".as_slice(),
        b"\x1b[?u".as_slice(),
        b"\x1b[c".as_slice(),
    ]
    .iter()
    .any(|query| query.starts_with(input))
}

fn normalized_size((cols, rows): (u16, u16)) -> (u16, u16) {
    (cols.max(2), rows.max(2))
}

/// Every viewer attached to one terminal, by [`viewer_id`], and the size each asked for.
type ViewerSizes = HashMap<String, (u16, u16)>;

/// The size a PTY runs at for a given set of viewers: the element-wise minimum of what they
/// each asked for, or `None` when nothing is attached.
///
/// A PTY has one size and a session can be open in several windows at once -- a desktop
/// browser, a phone, the dashboard's own workspace. Sizing it to whoever attached last is
/// what squashed a 180-column desktop into 40 columns the moment a phone joined, and because
/// a resize reflows the scrollback it mangled the history everyone else was reading at the
/// same time. Taking the minimum is what every terminal multiplexer settled on: the output
/// always fits every window, and the windows with room to spare letterbox the rest rather
/// than reflowing what the agent drew.
fn smallest_viewer(viewers: &ViewerSizes) -> Option<(u16, u16)> {
    viewers
        .values()
        .copied()
        .reduce(|(cols, rows), (other_cols, other_rows)| {
            (cols.min(other_cols), rows.min(other_rows))
        })
}

/// A viewer's name, unique across every process that can attach to the same daemon.
///
/// The pid is in front on purpose: a browser tab that dies without closing its socket cleanly
/// is caught by the socket task's own teardown, but a whole process killed outright cannot
/// detach at all, and a viewer nobody can remove would pin the terminal to that window's size
/// forever. [`drop_dead_viewers`] reads the pid back out and forgets those.
pub fn viewer_id(kind: &str) -> String {
    format!("{}:{kind}:{}", std::process::id(), Uuid::new_v4())
}

/// Forgets viewers whose process is gone. See [`viewer_id`].
fn drop_dead_viewers(viewers: &mut ViewerSizes) {
    if !cfg!(unix) {
        // `process_is_alive` cannot answer off unix, and answering "no" there would forget
        // every viewer including the live ones.
        return;
    }
    viewers.retain(|id, _| {
        id.split(':')
            .next()
            .and_then(|pid| pid.parse::<u32>().ok())
            .is_none_or(process_is_alive)
    });
}

impl Drop for LocalTerminal {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct WireCommandSpec {
    program: String,
    args: Vec<String>,
    cwd: PathBuf,
    /// Defaulted so a daemon older than protocol 2 still parses a newer client's spawn --
    /// it simply starts the agent without these, which `doctor` already reports as an
    /// out-of-date daemon.
    #[serde(default)]
    env: Vec<(String, String)>,
}

impl From<&CommandSpec> for WireCommandSpec {
    fn from(spec: &CommandSpec) -> Self {
        Self {
            program: spec.program.to_string_lossy().into_owned(),
            args: spec
                .args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
            cwd: spec.cwd.clone(),
            env: spec
                .env
                .iter()
                .map(|(name, value)| {
                    (
                        name.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
                .collect(),
        }
    }
}

impl WireCommandSpec {
    fn command_spec(&self) -> CommandSpec {
        CommandSpec {
            program: self.program.clone().into(),
            args: self.args.iter().map(OsString::from).collect(),
            cwd: self.cwd.clone(),
            env: self
                .env
                .iter()
                .map(|(name, value)| (OsString::from(name), OsString::from(value)))
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct LeaseOwner {
    instance_id: String,
    pid: u32,
    started_at: u64,
}

impl LeaseOwner {
    fn new() -> Self {
        Self {
            instance_id: Uuid::new_v4().to_string(),
            pid: std::process::id(),
            started_at: crate::model::unix_timestamp(),
        }
    }

    #[cfg(test)]
    fn new_for_test(instance_id: &str, _pid: u32) -> Self {
        Self {
            instance_id: instance_id.into(),
            pid: std::process::id(),
            started_at: 1,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
enum DaemonRequest {
    Ping,
    /// Answered with [`DAEMON_PROTOCOL`]. A daemon from before this request existed fails to
    /// parse it and answers `Error`, which is the signal that it is the older one.
    Version,
    Ensure {
        id: String,
        spec: WireCommandSpec,
        cols: u16,
        rows: u16,
    },
    Poll {
        id: String,
        offset: u64,
        /// Whether the answer should carry the rows above the current screen. Defaulted so
        /// a daemon left running by an older build still understands a newer client's poll.
        #[serde(default)]
        scrollback: bool,
    },
    Write {
        id: String,
        owner_id: String,
        bytes: Vec<u8>,
    },
    Resize {
        id: String,
        cols: u16,
        rows: u16,
        /// Which viewer is asking. The terminal is sized to the smallest of every viewer
        /// attached to it, so the name is what lets a second window be counted alongside the
        /// first instead of replacing it. Defaulted for a client older than protocol 2,
        /// whose resize is applied as-is because it has no viewer to be one of.
        #[serde(default)]
        viewer: Option<String>,
    },
    /// Forgets one viewer, so the terminal can grow back once the small window is gone.
    Detach {
        id: String,
        viewer: String,
    },
    Terminate {
        id: String,
    },
    List {
        prefix: String,
    },
    Rekey {
        old_prefix: String,
        new_prefix: String,
    },
    Acquire {
        session_key: String,
        owner: LeaseOwner,
        force: bool,
    },
    Release {
        session_key: String,
        owner_id: String,
    },
    ValidateLease {
        session_key: String,
        owner_id: String,
    },
    Shutdown,
}

#[derive(Debug, Deserialize, Serialize)]
enum DaemonResponse {
    Ok,
    Version {
        protocol: u32,
    },
    /// The size a terminal ended up at, which is the smallest of its attached viewers rather
    /// than whatever the caller asked for.
    Size {
        cols: u16,
        rows: u16,
    },
    Poll {
        start: u64,
        end: u64,
        bytes: Vec<u8>,
        #[serde(default)]
        checkpoint: Option<Vec<u8>>,
        #[serde(default)]
        status_bar_rows: Option<Vec<Vec<u8>>>,
        #[serde(default)]
        scrollback: Option<String>,
        /// The size the terminal is running at. Zero from a daemon older than protocol 2,
        /// which is read as "unknown" rather than as a size.
        #[serde(default)]
        cols: u16,
        #[serde(default)]
        rows: u16,
        alive: bool,
        exit: Option<String>,
    },
    List(Vec<String>),
    LeaseGranted,
    LeaseDenied {
        owner: LeaseOwner,
    },
    Error(String),
}

#[derive(Default)]
struct PtyDaemonState {
    terminals: HashMap<String, LocalTerminal>,
    leases: HashMap<String, LeaseOwner>,
    lease_seen: HashMap<String, Instant>,
}

impl PtyDaemonState {
    fn handle(&mut self, request: DaemonRequest) -> (DaemonResponse, bool) {
        let response = match request {
            DaemonRequest::Ping => DaemonResponse::Ok,
            DaemonRequest::Version => DaemonResponse::Version {
                protocol: DAEMON_PROTOCOL,
            },
            DaemonRequest::Ensure {
                id,
                spec,
                cols,
                rows,
            } => {
                use std::collections::hash_map::Entry;

                match self.terminals.entry(id) {
                    Entry::Vacant(entry) => {
                        match LocalTerminal::spawn(&spec.command_spec(), (cols, rows)) {
                            Ok(terminal) => {
                                entry.insert(terminal);
                            }
                            Err(error) => {
                                return (DaemonResponse::Error(error.to_string()), false);
                            }
                        }
                    }
                    Entry::Occupied(mut entry) if !entry.get().is_alive() => {
                        match LocalTerminal::spawn(&spec.command_spec(), (cols, rows)) {
                            Ok(terminal) => {
                                entry.insert(terminal);
                            }
                            Err(error) => {
                                return (DaemonResponse::Error(error.to_string()), false);
                            }
                        }
                    }
                    Entry::Occupied(_) => {}
                }
                DaemonResponse::Ok
            }
            DaemonRequest::Poll {
                id,
                offset,
                scrollback,
            } => {
                let Some(terminal) = self.terminals.get(&id) else {
                    return (
                        DaemonResponse::Error(format!("unknown terminal {id}")),
                        false,
                    );
                };
                let alive = terminal.is_alive();
                let wanted = if scrollback {
                    Scrollback::Include
                } else {
                    Scrollback::Omit
                };
                let delta = terminal.output_since(offset, wanted);
                let (cols, rows) = terminal.size();
                DaemonResponse::Poll {
                    start: delta.start,
                    end: delta.end,
                    bytes: delta.bytes,
                    checkpoint: delta.checkpoint,
                    status_bar_rows: delta.status_bar_rows,
                    scrollback: delta.scrollback,
                    cols,
                    rows,
                    alive,
                    exit: delta.exit,
                }
            }
            DaemonRequest::Write {
                id,
                owner_id,
                bytes,
            } => {
                let session_key = terminal_session_key(&id);
                if session_key
                    .as_deref()
                    .is_some_and(|key| !self.owner_can_write(key, &owner_id))
                {
                    DaemonResponse::Error(LEASE_DENIED_MESSAGE.into())
                } else {
                    match self.terminals.get(&id) {
                        Some(terminal) => terminal
                            .write(&bytes)
                            .map(|()| DaemonResponse::Ok)
                            .unwrap_or_else(|error| DaemonResponse::Error(error.to_string())),
                        None => DaemonResponse::Error(format!("unknown terminal {id}")),
                    }
                }
            }
            DaemonRequest::Resize {
                id,
                cols,
                rows,
                viewer,
            } => match self.terminals.get(&id) {
                Some(terminal) => match viewer {
                    Some(viewer) => terminal
                        .resize_viewer(&viewer, cols, rows)
                        .map(|(cols, rows)| DaemonResponse::Size { cols, rows })
                        .unwrap_or_else(|error| DaemonResponse::Error(error.to_string())),
                    // A client from before viewers existed has no name to be counted under,
                    // so its resize is applied the way it always was.
                    None => terminal
                        .resize(cols, rows)
                        .map(|()| DaemonResponse::Ok)
                        .unwrap_or_else(|error| DaemonResponse::Error(error.to_string())),
                },
                None => DaemonResponse::Error(format!("unknown terminal {id}")),
            },
            // A terminal that has already gone is not a failure to detach from: the viewer
            // wanted to stop being counted, and it is not being counted.
            DaemonRequest::Detach { id, viewer } => match self.terminals.get(&id) {
                Some(terminal) => terminal
                    .detach_viewer(&viewer)
                    .map(|(cols, rows)| DaemonResponse::Size { cols, rows })
                    .unwrap_or_else(|error| DaemonResponse::Error(error.to_string())),
                None => DaemonResponse::Ok,
            },
            DaemonRequest::Terminate { id } => match self.terminals.remove(&id) {
                Some(terminal) => {
                    terminal.terminate();
                    DaemonResponse::Ok
                }
                None => DaemonResponse::Error(format!("unknown terminal {id}")),
            },
            DaemonRequest::List { prefix } => {
                let mut ids = self
                    .terminals
                    .keys()
                    .filter(|id| id.starts_with(&prefix))
                    .cloned()
                    .collect::<Vec<_>>();
                ids.sort();
                DaemonResponse::List(ids)
            }
            DaemonRequest::Rekey {
                old_prefix,
                new_prefix,
            } => {
                let renames = self
                    .terminals
                    .keys()
                    .filter_map(|id| {
                        id.strip_prefix(&old_prefix)
                            .map(|suffix| (id.clone(), format!("{new_prefix}{suffix}")))
                    })
                    .collect::<Vec<_>>();
                for (old, new) in renames {
                    if !self.terminals.contains_key(&new)
                        && let Some(terminal) = self.terminals.remove(&old)
                    {
                        self.terminals.insert(new, terminal);
                    }
                }
                if let Some(old_key) = old_prefix.strip_prefix("agent|")
                    && let Some(new_key) = new_prefix.strip_prefix("agent|")
                    && let Some(lease) = self.leases.remove(old_key)
                {
                    self.leases.insert(new_key.to_owned(), lease);
                    if let Some(seen) = self.lease_seen.remove(old_key) {
                        self.lease_seen.insert(new_key.to_owned(), seen);
                    }
                }
                DaemonResponse::Ok
            }
            DaemonRequest::Acquire {
                session_key,
                owner,
                force,
            } => {
                let denied = self.leases.get(&session_key).filter(|current| {
                    current.instance_id != owner.instance_id
                        && !force
                        && process_is_alive(current.pid)
                        && self
                            .lease_seen
                            .get(&session_key)
                            .is_some_and(|seen| seen.elapsed() < LEASE_STALE_AFTER)
                });
                if let Some(current) = denied {
                    DaemonResponse::LeaseDenied {
                        owner: current.clone(),
                    }
                } else {
                    self.lease_seen.insert(session_key.clone(), Instant::now());
                    self.leases.insert(session_key, owner);
                    DaemonResponse::LeaseGranted
                }
            }
            DaemonRequest::Release {
                session_key,
                owner_id,
            } => {
                if self
                    .leases
                    .get(&session_key)
                    .is_some_and(|owner| owner.instance_id == owner_id)
                {
                    self.leases.remove(&session_key);
                    self.lease_seen.remove(&session_key);
                }
                DaemonResponse::Ok
            }
            DaemonRequest::ValidateLease {
                session_key,
                owner_id,
            } => {
                if self.owner_can_write(&session_key, &owner_id)
                    && self.leases.contains_key(&session_key)
                {
                    self.lease_seen.insert(session_key, Instant::now());
                    DaemonResponse::Ok
                } else {
                    DaemonResponse::Error(LEASE_DENIED_MESSAGE.into())
                }
            }
            DaemonRequest::Shutdown => return (DaemonResponse::Ok, true),
        };
        (response, false)
    }

    fn owner_can_write(&self, session_key: &str, owner_id: &str) -> bool {
        self.leases
            .get(session_key)
            .is_none_or(|owner| owner.instance_id == owner_id)
    }
}

fn terminal_session_key(id: &str) -> Option<String> {
    if let Some(key) = id.strip_prefix("agent|") {
        return Some(key.to_owned());
    }
    id.strip_prefix("shell|")
        .and_then(|value| value.rsplit_once('|').map(|(key, _)| key.to_owned()))
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: signal 0 performs only an existence/permission check.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_is_alive(_pid: u32) -> bool {
    false
}

#[cfg(unix)]
fn daemon_request(socket: &Path, request: &DaemonRequest) -> io::Result<DaemonResponse> {
    let mut stream = UnixStream::connect(socket)?;
    serde_json::to_writer(&mut stream, request).map_err(io::Error::other)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    serde_json::from_str(&line).map_err(io::Error::other)
}

#[cfg(not(unix))]
fn daemon_request(_socket: &Path, _request: &DaemonRequest) -> io::Result<DaemonResponse> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "detached PTY daemon is unavailable on this platform",
    ))
}

/// What the daemon says when a write or a lease validation loses to whoever holds the
/// session lease. Named so both the daemon and the callers that have to recognise it agree
/// on one string instead of two copies drifting apart.
pub const LEASE_DENIED_MESSAGE: &str = "session lease is owned by another TUI";

/// The error a resize/detach answers with when the daemon said something unexpected. Shares
/// `response_ok`'s lease handling so a resize that lost the lease is still `PermissionDenied`.
fn resize_failure(response: DaemonResponse) -> io::Error {
    match response_ok(response) {
        Ok(()) => io::Error::other("daemon did not answer a resize with a size"),
        Err(error) => error,
    }
}

fn response_ok(response: DaemonResponse) -> io::Result<()> {
    match response {
        DaemonResponse::Ok => Ok(()),
        // Reported as `PermissionDenied` -- the same kind `attach_workspace` already uses for
        // a denied lease -- so a caller can tell "another surface owns this session, offer a
        // takeover" apart from a genuine terminal failure without matching on prose.
        DaemonResponse::Error(error) if error == LEASE_DENIED_MESSAGE => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            LEASE_DENIED_MESSAGE,
        )),
        DaemonResponse::Error(error) => Err(io::Error::other(error)),
        other => Err(io::Error::other(format!(
            "unexpected daemon response: {other:?}"
        ))),
    }
}

#[cfg(unix)]
pub fn run_pty_daemon(socket: &Path) -> io::Result<()> {
    if let Some(parent) = socket.parent() {
        ensure_private_dir(parent)?;
    }
    if socket.exists() {
        fs::remove_file(socket)?;
    }
    let listener = UnixListener::bind(socket)?;
    make_private_file(socket)?;
    let mut state = PtyDaemonState::default();
    let result = (|| {
        for connection in listener.incoming() {
            let mut stream = connection?;
            let mut line = String::new();
            BufReader::new(stream.try_clone()?).read_line(&mut line)?;
            let (response, shutdown) = match serde_json::from_str(&line) {
                Ok(request) => state.handle(request),
                Err(error) => (DaemonResponse::Error(error.to_string()), false),
            };
            serde_json::to_writer(&mut stream, &response).map_err(io::Error::other)?;
            stream.write_all(b"\n")?;
            stream.flush()?;
            if shutdown {
                break;
            }
        }
        Ok(())
    })();
    let _ = fs::remove_file(socket);
    result
}

#[cfg(not(unix))]
pub fn run_pty_daemon(_socket: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "detached PTY daemon is unavailable on this platform",
    ))
}

#[cfg(unix)]
pub fn stop_pty_daemon(socket: &Path) -> io::Result<()> {
    response_ok(daemon_request(socket, &DaemonRequest::Shutdown)?)
}

#[cfg(not(unix))]
pub fn stop_pty_daemon(_socket: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "detached PTY daemon is unavailable on this platform",
    ))
}

#[cfg(unix)]
pub fn daemon_health(socket: &Path) -> io::Result<Option<()>> {
    if !socket.exists() {
        return Ok(None);
    }
    response_ok(daemon_request(socket, &DaemonRequest::Ping)?).map(Some)
}

#[cfg(not(unix))]
pub fn daemon_health(_socket: &Path) -> io::Result<Option<()>> {
    Ok(None)
}

/// The protocol the daemon at `socket` speaks, or `None` when no daemon is running.
///
/// A daemon that does not recognise the request predates the version being carried at all,
/// which is reported as protocol 0 rather than as an error: it is running and healthy, just
/// older than this build.
#[cfg(unix)]
pub fn daemon_protocol(socket: &Path) -> io::Result<Option<u32>> {
    if !socket.exists() {
        return Ok(None);
    }
    match daemon_request(socket, &DaemonRequest::Version)? {
        DaemonResponse::Version { protocol } => Ok(Some(protocol)),
        DaemonResponse::Error(_) => Ok(Some(0)),
        other => Err(io::Error::other(format!(
            "unexpected daemon response: {other:?}"
        ))),
    }
}

#[cfg(not(unix))]
pub fn daemon_protocol(_socket: &Path) -> io::Result<Option<u32>> {
    Ok(None)
}

struct RemoteTerminal {
    socket: PathBuf,
    id: Mutex<String>,
    owner_id: String,
    offset: Mutex<u64>,
    output: Mutex<OutputState>,
    output_generation: AtomicU64,
    parser: Mutex<vt100::Parser>,
    status_bar_scrollback: Mutex<StatusBarScrollback>,
    size: Mutex<(u16, u16)>,
}

impl RemoteTerminal {
    fn ensure(
        socket: PathBuf,
        id: String,
        owner_id: String,
        spec: &CommandSpec,
        size: (u16, u16),
    ) -> io::Result<Self> {
        let size = normalized_size(size);
        response_ok(daemon_request(
            &socket,
            &DaemonRequest::Ensure {
                id: id.clone(),
                spec: WireCommandSpec::from(spec),
                cols: size.0,
                rows: size.1,
            },
        )?)?;
        Self::connect(socket, id, owner_id, size)
    }

    fn connect(
        socket: PathBuf,
        id: String,
        owner_id: String,
        size: (u16, u16),
    ) -> io::Result<Self> {
        let size = normalized_size(size);
        let terminal = Self {
            socket,
            id: Mutex::new(id),
            owner_id,
            offset: Mutex::new(0),
            output: Mutex::new(OutputState::default()),
            output_generation: AtomicU64::new(0),
            parser: Mutex::new(vt100::Parser::new(size.1, size.0, SCROLLBACK_LINES)),
            status_bar_scrollback: Mutex::new(StatusBarScrollback::default()),
            size: Mutex::new(size),
        };
        terminal.sync()?;
        Ok(terminal)
    }

    fn sync(&self) -> io::Result<bool> {
        let requested = *self.offset.lock().unwrap();
        let response = daemon_request(
            &self.socket,
            &DaemonRequest::Poll {
                id: self.id.lock().unwrap().clone(),
                offset: requested,
                // The TUI rebuilds its own scrollback from the bytes and the status-bar rows
                // as it always has; only a client attaching from nothing needs the snapshot.
                scrollback: false,
            },
        )?;
        let DaemonResponse::Poll {
            start,
            end,
            bytes,
            checkpoint,
            status_bar_rows,
            scrollback: _,
            cols,
            rows,
            alive,
            exit,
        } = response
        else {
            return match response {
                DaemonResponse::Error(error) => Err(io::Error::other(error)),
                other => Err(io::Error::other(format!(
                    "unexpected daemon response: {other:?}"
                ))),
            };
        };
        // Another viewer attaching or leaving changes the PTY's size without this client
        // asking for anything, and the bytes that follow are drawn for the new size. Adopting
        // it here is what keeps this parser measuring them the way the agent wrote them.
        self.adopt_size((cols, rows));
        if checkpoint.is_some() || start != requested {
            let size = *self.size.lock().unwrap();
            *self.parser.lock().unwrap() = vt100::Parser::new(size.1, size.0, SCROLLBACK_LINES);
            *self.status_bar_scrollback.lock().unwrap() = StatusBarScrollback::default();
            let mut output = self.output.lock().unwrap();
            output.raw.clear();
            output.base_offset = start;
        }
        if checkpoint.is_some() || !bytes.is_empty() {
            let mut parser = self.parser.lock().unwrap();
            let mut scrollback = self.status_bar_scrollback.lock().unwrap();
            if let Some(checkpoint) = checkpoint.as_deref() {
                process_terminal_output(&mut parser, &mut scrollback, checkpoint);
            }
            process_terminal_output(&mut parser, &mut scrollback, &bytes);
        }
        if let Some(rows) = status_bar_rows {
            self.status_bar_scrollback.lock().unwrap().rows = rows
                .into_iter()
                .rev()
                .take(SCROLLBACK_LINES)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
        }
        let mut output = self.output.lock().unwrap();
        let changed = checkpoint.is_some() || !bytes.is_empty() || output.exited == alive;
        if let Some(checkpoint) = checkpoint {
            output.raw.extend(checkpoint);
        }
        output.raw.extend(bytes);
        while output.raw.len() > RAW_CAPTURE_BYTES {
            output.raw.pop_front();
            output.base_offset = output.base_offset.saturating_add(1);
        }
        output.exited = !alive;
        output.exit_description = exit;
        drop(output);
        *self.offset.lock().unwrap() = end;
        if changed {
            self.output_generation.fetch_add(1, Ordering::Relaxed);
        }
        Ok(alive)
    }

    /// Backend-agnostic raw poll that issues `DaemonRequest::Poll` directly, mirroring
    /// `sync()`, but WITHOUT touching `self.parser`/`self.offset`/`self.output`/
    /// `self.status_bar_scrollback`. Those back the TUI's own view of this terminal, so an
    /// independent poller (e.g. the web server) must not mutate them.
    fn poll_raw(&self, offset: u64, scrollback: Scrollback) -> io::Result<RawPoll> {
        let response = daemon_request(
            &self.socket,
            &DaemonRequest::Poll {
                id: self.id.lock().unwrap().clone(),
                offset,
                scrollback: scrollback == Scrollback::Include,
            },
        )?;
        let DaemonResponse::Poll {
            start,
            end,
            bytes,
            checkpoint,
            status_bar_rows: _,
            scrollback,
            cols,
            rows,
            alive,
            exit,
        } = response
        else {
            return match response {
                DaemonResponse::Error(error) => Err(io::Error::other(error)),
                other => Err(io::Error::other(format!(
                    "unexpected daemon response: {other:?}"
                ))),
            };
        };
        Ok(RawPoll {
            start,
            end,
            bytes,
            checkpoint,
            scrollback,
            // Deliberately read straight off the wire rather than adopted into `self.size`:
            // this poll belongs to a caller that shares none of this handle's state.
            size: if cols == 0 || rows == 0 {
                *self.size.lock().unwrap()
            } else {
                (cols, rows)
            },
            alive,
            exit,
        })
    }

    fn is_alive(&self) -> bool {
        match self.sync() {
            Ok(alive) => alive,
            Err(_) => {
                let mut output = self.output.lock().unwrap();
                output.exited = true;
                output.exit_description = Some("daemon disconnected".into());
                false
            }
        }
    }

    fn write(&self, bytes: &[u8]) -> io::Result<()> {
        response_ok(daemon_request(
            &self.socket,
            &DaemonRequest::Write {
                id: self.id.lock().unwrap().clone(),
                owner_id: self.owner_id.clone(),
                bytes: bytes.to_vec(),
            },
        )?)
    }

    fn wait_for_first_output(&self, timeout: Duration) {
        let start = Instant::now();
        while self.output_generation() == 0 && start.elapsed() < timeout {
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn plain_capture(&self) -> String {
        let _ = self.sync();
        let raw = self
            .output
            .lock()
            .unwrap()
            .raw
            .iter()
            .copied()
            .collect::<Vec<_>>();
        plain_text(&raw)
    }

    fn exit_description(&self) -> Option<String> {
        let _ = self.sync();
        self.output.lock().unwrap().exit_description.clone()
    }

    fn screen_view(&self) -> ScreenView {
        let _ = self.sync();
        let parser = self.parser.lock().unwrap();
        let scrollback = self.status_bar_scrollback.lock().unwrap();
        terminal_screen_view(&parser, &scrollback)
    }

    fn scroll_viewport(&self, rows: isize) -> usize {
        let _ = self.sync();
        let mut parser = self.parser.lock().unwrap();
        let mut status_bar_scrollback = self.status_bar_scrollback.lock().unwrap();
        if !status_bar_scrollback.rows.is_empty() {
            parser.screen_mut().set_scrollback(0);
            let applied = status_bar_scrollback.scroll(rows);
            drop(status_bar_scrollback);
            drop(parser);
            self.output_generation.fetch_add(1, Ordering::Relaxed);
            return applied;
        }
        let screen = parser.screen_mut();
        let current = screen.scrollback();
        let requested = if rows >= 0 {
            current.saturating_add(rows as usize)
        } else {
            current.saturating_sub(rows.unsigned_abs())
        };
        screen.set_scrollback(requested);
        let applied = screen.scrollback();
        drop(parser);
        self.output_generation.fetch_add(1, Ordering::Relaxed);
        applied
    }

    fn scroll_to_live_tail(&self) {
        let mut parser = self.parser.lock().unwrap();
        self.status_bar_scrollback.lock().unwrap().live_tail();
        parser.screen_mut().set_scrollback(0);
        self.output_generation.fetch_add(1, Ordering::Relaxed);
    }

    fn scrollback_offset(&self) -> usize {
        let parser = self.parser.lock().unwrap();
        let status_bar_scrollback = self.status_bar_scrollback.lock().unwrap();
        if status_bar_scrollback.rows.is_empty() {
            parser.screen().scrollback()
        } else {
            status_bar_scrollback.offset
        }
    }

    fn mouse_protocol(&self) -> (vt100::MouseProtocolMode, vt100::MouseProtocolEncoding) {
        let _ = self.sync();
        let parser = self.parser.lock().unwrap();
        let screen = parser.screen();
        (
            screen.mouse_protocol_mode(),
            screen.mouse_protocol_encoding(),
        )
    }

    fn alternate_screen(&self) -> bool {
        let _ = self.sync();
        self.parser.lock().unwrap().screen().alternate_screen()
    }

    /// The size this terminal was last resized to. Read straight off the stored value rather
    /// than the parser, so asking for the size never syncs and never disturbs the shared
    /// screen state the TUI and the websocket render from.
    fn size(&self) -> (u16, u16) {
        *self.size.lock().unwrap()
    }

    fn selection_cell(&self, cell: TerminalCell) -> Option<TerminalBufferCell> {
        let _ = self.sync();
        let mut parser = self.parser.lock().unwrap();
        let scrollback = self.status_bar_scrollback.lock().unwrap();
        terminal_buffer_cell(&mut parser, &scrollback, cell)
    }

    fn selected_text(&self, first: TerminalBufferCell, second: TerminalBufferCell) -> String {
        let _ = self.sync();
        let mut parser = self.parser.lock().unwrap();
        let scrollback = self.status_bar_scrollback.lock().unwrap();
        terminal_selected_text(&mut parser, &scrollback, first, second)
    }

    fn selected_rows(
        &self,
        first: TerminalBufferCell,
        second: TerminalBufferCell,
    ) -> Vec<(TerminalCell, String)> {
        let _ = self.sync();
        let mut parser = self.parser.lock().unwrap();
        let scrollback = self.status_bar_scrollback.lock().unwrap();
        terminal_selected_rows(&mut parser, &scrollback, first, second)
    }

    fn resize_viewer(&self, viewer: &str, cols: u16, rows: u16) -> io::Result<(u16, u16)> {
        self.request_resize(Some(viewer), cols, rows)
    }

    fn detach_viewer(&self, viewer: &str) -> io::Result<(u16, u16)> {
        let response = daemon_request(
            &self.socket,
            &DaemonRequest::Detach {
                id: self.id.lock().unwrap().clone(),
                viewer: viewer.to_owned(),
            },
        )?;
        match response {
            DaemonResponse::Size { cols, rows } => {
                self.adopt_size((cols, rows));
                Ok((cols, rows))
            }
            // Either the terminal is gone, or the daemon is older than viewers. Neither is
            // worth failing a teardown over.
            _ => Ok(*self.size.lock().unwrap()),
        }
    }

    fn request_resize(&self, viewer: Option<&str>, cols: u16, rows: u16) -> io::Result<(u16, u16)> {
        let requested = normalized_size((cols, rows));
        let response = daemon_request(
            &self.socket,
            &DaemonRequest::Resize {
                id: self.id.lock().unwrap().clone(),
                cols: requested.0,
                rows: requested.1,
                viewer: viewer.map(str::to_owned),
            },
        )?;
        let effective = match response {
            DaemonResponse::Size { cols, rows } => (cols, rows),
            // A daemon older than protocol 2 answers `Ok` and resizes to exactly what was
            // asked, which is the pre-viewer behaviour.
            DaemonResponse::Ok => requested,
            other => return Err(resize_failure(other)),
        };
        self.adopt_size(effective);
        Ok(effective)
    }

    /// Points this handle's own parser at the size the PTY is actually running at.
    ///
    /// A no-op when nothing changed, because it costs the TUI a full repaint: `set_size`
    /// reflows the screen and bumps the generation every render loop compares against.
    fn adopt_size(&self, size: (u16, u16)) {
        if size.0 == 0 || size.1 == 0 {
            return;
        }
        {
            let mut current = self.size.lock().unwrap();
            if *current == size {
                return;
            }
            *current = size;
        }
        self.parser
            .lock()
            .unwrap()
            .screen_mut()
            .set_size(size.1, size.0);
        self.status_bar_scrollback.lock().unwrap().resize(size.1);
        self.output_generation.fetch_add(1, Ordering::Relaxed);
    }

    fn terminate(&self) {
        let _ = daemon_request(
            &self.socket,
            &DaemonRequest::Terminate {
                id: self.id.lock().unwrap().clone(),
            },
        );
    }

    fn output_generation(&self) -> u64 {
        let _ = self.sync();
        self.output_generation.load(Ordering::Relaxed)
    }

    fn rekey_prefix(&self, old_prefix: &str, new_prefix: &str) {
        let mut id = self.id.lock().unwrap();
        if let Some(suffix) = id.strip_prefix(old_prefix) {
            *id = format!("{new_prefix}{suffix}");
        }
    }
}

/// Raw output delta for a backend-agnostic poller (e.g. the web server) that owns its own
/// offset cursor and does not share a terminal's parser/output state with the TUI.
pub struct RawPoll {
    pub start: u64,
    pub end: u64,
    pub bytes: Vec<u8>,
    /// ANSI-formatted full-screen resync (`vt100::Screen::state_formatted()`), present when
    /// the caller's offset fell outside the retained buffer and needs a fresh screen.
    pub checkpoint: Option<Vec<u8>>,
    /// Everything this terminal retains *above* `checkpoint`, oldest first, ANSI-formatted
    /// and CRLF-joined, present only when the caller asked for [`Scrollback::Include`].
    ///
    /// A checkpoint is exactly one screenful, so a client that got only that starts with an
    /// empty scrollback: every line printed before it attached is missing, and there is
    /// nothing above the fold to scroll to. These are those lines -- both the ones `vt100`
    /// holds for the normal screen and the ones `pty.rs` captures out of a top-anchored
    /// partial scroll region, which is how a TUI pins a composer to the bottom.
    ///
    /// Written immediately before `checkpoint`, they land in the client emulator's own
    /// scrollback and the checkpoint repaints the screen over them. Both come from one
    /// parser under one lock at one instant, so the join between them cannot duplicate or
    /// drop a band of lines.
    pub scrollback: Option<String>,
    /// The size the PTY is running at right now, which is not necessarily the size this
    /// poller asked for: it is the smallest of every attached viewer (see `smallest_viewer`).
    /// A poller that is larger than this letterboxes the difference -- reported here so it
    /// can do that deliberately, and so it hears about a change another viewer caused.
    pub size: (u16, u16),
    pub alive: bool,
    pub exit: Option<String>,
}

pub struct ManagedTerminal {
    backend: TerminalBackend,
}

enum TerminalBackend {
    Local(LocalTerminal),
    Remote(Box<RemoteTerminal>),
}

impl ManagedTerminal {
    pub fn spawn(spec: &CommandSpec, size: (u16, u16)) -> io::Result<Self> {
        Ok(Self {
            backend: TerminalBackend::Local(LocalTerminal::spawn(spec, size)?),
        })
    }

    fn ensure_remote(
        socket: PathBuf,
        id: String,
        owner_id: String,
        spec: &CommandSpec,
        size: (u16, u16),
    ) -> io::Result<Self> {
        Ok(Self {
            backend: TerminalBackend::Remote(Box::new(RemoteTerminal::ensure(
                socket, id, owner_id, spec, size,
            )?)),
        })
    }

    fn connect_remote(
        socket: PathBuf,
        id: String,
        owner_id: String,
        size: (u16, u16),
    ) -> io::Result<Self> {
        Ok(Self {
            backend: TerminalBackend::Remote(Box::new(RemoteTerminal::connect(
                socket, id, owner_id, size,
            )?)),
        })
    }

    pub fn is_alive(&self) -> bool {
        match &self.backend {
            TerminalBackend::Local(terminal) => terminal.is_alive(),
            TerminalBackend::Remote(terminal) => terminal.is_alive(),
        }
    }

    pub fn write(&self, bytes: &[u8]) -> io::Result<()> {
        match &self.backend {
            TerminalBackend::Local(terminal) => terminal.write(bytes),
            TerminalBackend::Remote(terminal) => terminal.write(bytes),
        }
    }

    pub fn wait_for_first_output(&self, timeout: Duration) {
        match &self.backend {
            TerminalBackend::Local(terminal) => terminal.wait_for_first_output(timeout),
            TerminalBackend::Remote(terminal) => terminal.wait_for_first_output(timeout),
        }
    }

    pub fn plain_capture(&self) -> String {
        match &self.backend {
            TerminalBackend::Local(terminal) => terminal.plain_capture(),
            TerminalBackend::Remote(terminal) => terminal.plain_capture(),
        }
    }

    fn exit_description(&self) -> Option<String> {
        match &self.backend {
            TerminalBackend::Local(terminal) => terminal.exit_description(),
            TerminalBackend::Remote(terminal) => terminal.exit_description(),
        }
    }

    fn screen_view(&self) -> ScreenView {
        match &self.backend {
            TerminalBackend::Local(terminal) => terminal.screen_view(),
            TerminalBackend::Remote(terminal) => terminal.screen_view(),
        }
    }

    fn scroll_viewport(&self, rows: isize) -> usize {
        match &self.backend {
            TerminalBackend::Local(terminal) => terminal.scroll_viewport(rows),
            TerminalBackend::Remote(terminal) => terminal.scroll_viewport(rows),
        }
    }

    fn scroll_to_live_tail(&self) {
        match &self.backend {
            TerminalBackend::Local(terminal) => terminal.scroll_to_live_tail(),
            TerminalBackend::Remote(terminal) => terminal.scroll_to_live_tail(),
        }
    }

    fn scrollback_offset(&self) -> usize {
        match &self.backend {
            TerminalBackend::Local(terminal) => terminal.scrollback_offset(),
            TerminalBackend::Remote(terminal) => terminal.scrollback_offset(),
        }
    }

    fn mouse_protocol(&self) -> (vt100::MouseProtocolMode, vt100::MouseProtocolEncoding) {
        match &self.backend {
            TerminalBackend::Local(terminal) => terminal.mouse_protocol(),
            TerminalBackend::Remote(terminal) => terminal.mouse_protocol(),
        }
    }

    fn alternate_screen(&self) -> bool {
        match &self.backend {
            TerminalBackend::Local(terminal) => terminal.alternate_screen(),
            TerminalBackend::Remote(terminal) => terminal.alternate_screen(),
        }
    }

    /// The size this terminal is currently running at, as `(cols, rows)`.
    ///
    /// A caller that keeps its own parser (see `poll_raw`) needs this to size that parser the
    /// same way, or the text it reads back wraps differently from what the agent drew.
    pub fn size(&self) -> (u16, u16) {
        match &self.backend {
            TerminalBackend::Local(terminal) => terminal.size(),
            TerminalBackend::Remote(terminal) => terminal.size(),
        }
    }

    fn selection_cell(&self, cell: TerminalCell) -> Option<TerminalBufferCell> {
        match &self.backend {
            TerminalBackend::Local(terminal) => terminal.selection_cell(cell),
            TerminalBackend::Remote(terminal) => terminal.selection_cell(cell),
        }
    }

    fn selected_text(&self, first: TerminalBufferCell, second: TerminalBufferCell) -> String {
        match &self.backend {
            TerminalBackend::Local(terminal) => terminal.selected_text(first, second),
            TerminalBackend::Remote(terminal) => terminal.selected_text(first, second),
        }
    }

    fn selected_rows(
        &self,
        first: TerminalBufferCell,
        second: TerminalBufferCell,
    ) -> Vec<(TerminalCell, String)> {
        match &self.backend {
            TerminalBackend::Local(terminal) => terminal.selected_rows(first, second),
            TerminalBackend::Remote(terminal) => terminal.selected_rows(first, second),
        }
    }

    /// Tells the terminal how big *this* window is and answers with the size it settled on.
    ///
    /// The answer is the smallest of every attached viewer, so it can be smaller than what
    /// was asked for -- see `smallest_viewer`. A caller with room to spare letterboxes the
    /// difference; it must not stretch or reflow what came back to fill its window, which
    /// would corrupt the very output somebody is reading.
    pub fn resize_viewer(&self, viewer: &str, cols: u16, rows: u16) -> io::Result<(u16, u16)> {
        match &self.backend {
            TerminalBackend::Local(terminal) => terminal.resize_viewer(viewer, cols, rows),
            TerminalBackend::Remote(terminal) => terminal.resize_viewer(viewer, cols, rows),
        }
    }

    /// Stops counting a window that has gone away, letting the terminal grow back.
    ///
    /// Call it from a teardown that runs on an abrupt disconnect as well as a clean one: a
    /// viewer nobody removes pins the terminal to a dead window's size.
    pub fn detach_viewer(&self, viewer: &str) -> io::Result<(u16, u16)> {
        match &self.backend {
            TerminalBackend::Local(terminal) => terminal.detach_viewer(viewer),
            TerminalBackend::Remote(terminal) => terminal.detach_viewer(viewer),
        }
    }

    pub fn terminate(&self) {
        match &self.backend {
            TerminalBackend::Local(terminal) => terminal.terminate(),
            TerminalBackend::Remote(terminal) => terminal.terminate(),
        }
    }

    /// Raw poll for a caller that owns its own offset cursor, independent of the TUI's
    /// view of this terminal (see `RawPoll`). Used by the web server.
    pub fn poll_raw(&self, offset: u64, scrollback: Scrollback) -> io::Result<RawPoll> {
        match &self.backend {
            TerminalBackend::Local(terminal) => Ok(terminal.poll_raw(offset, scrollback)),
            TerminalBackend::Remote(terminal) => terminal.poll_raw(offset, scrollback),
        }
    }

    fn output_generation(&self) -> u64 {
        match &self.backend {
            TerminalBackend::Local(terminal) => terminal.output_generation.load(Ordering::Relaxed),
            TerminalBackend::Remote(terminal) => terminal.output_generation(),
        }
    }

    fn rekey_prefix(&self, old_prefix: &str, new_prefix: &str) {
        if let TerminalBackend::Remote(terminal) = &self.backend {
            terminal.rekey_prefix(old_prefix, new_prefix);
        }
    }
}

struct ShellPane {
    /// Names this shell to callers outside this module (the web API's `/shells` routes).
    /// It is the unique suffix of the daemon id `shell|<session key>|<id>` rather than the
    /// whole id, so a `rekey` -- which rewrites only the `shell|<session key>|` prefix --
    /// leaves it valid.
    id: String,
    terminal: Arc<ManagedTerminal>,
    name: String,
    capture_prefix: String,
}

/// Who currently owns a session's input lease, for a caller outside this module that has to
/// name the conflict to a user. A projection of the daemon's own `LeaseOwner`, kept separate
/// so the wire type stays private to this module.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeaseHolder {
    pub pid: u32,
    pub instance_id: String,
    pub started_at: u64,
}

/// The answer to asking the daemon for a session's input lease.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LeaseOutcome {
    Granted,
    Denied(LeaseHolder),
}

/// One shell's identity, for a caller outside this module that has to name a specific shell.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShellInfo {
    pub id: String,
    pub name: String,
}

impl ShellPane {
    fn new(id: String, terminal: ManagedTerminal, name: String) -> Self {
        Self {
            id,
            terminal: Arc::new(terminal),
            name,
            capture_prefix: String::new(),
        }
    }

    fn info(&self) -> ShellInfo {
        ShellInfo {
            id: self.id.clone(),
            name: self.name.clone(),
        }
    }

    fn mark_command_start(&mut self) {
        self.capture_prefix = self.terminal.plain_capture();
    }

    fn command_capture(&self) -> String {
        let capture = self.terminal.plain_capture();
        command_block_after(&capture, &self.capture_prefix)
    }
}

fn command_block_after(capture: &str, prefix: &str) -> String {
    capture.strip_prefix(prefix).unwrap_or(capture).to_owned()
}

#[derive(Default)]
pub struct SessionTerminals {
    pub agent: Option<Arc<ManagedTerminal>>,
    shells: Vec<ShellPane>,
    pub selected_shell: usize,
    selection: Option<TerminalSelection>,
    alternate_selection: Option<AlternateSelectionBuffer>,
    pending_alternate_copy: Option<PendingAlternateCopy>,
    suppressed_mouse_buttons: u8,
    pending_agent_click: Option<PendingAgentClick>,
    notice: Option<String>,
    daemon_socket: Option<PathBuf>,
    lease_owner_id: String,
    maximized: Option<PaneTarget>,
    shell_height_adjust: i16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceFocus {
    Sessions,
    Agent,
    Shell,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShellCloseAction {
    Ignore,
    Close,
}

fn shell_close_action(focus: WorkspaceFocus) -> ShellCloseAction {
    if focus != WorkspaceFocus::Shell {
        ShellCloseAction::Ignore
    } else {
        ShellCloseAction::Close
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceExit {
    Dashboard,
    Alert,
    ActivateSession,
    FocusShell,
    NewSession,
    OpenShell,
    ToggleArchive,
    RefreshSessions,
    PreviousSession(WorkspaceFocus),
    NextSession(WorkspaceFocus),
}

pub enum WorkspaceSearchUpdate {
    Preview(String),
    Cancel {
        query: String,
        selected_session_key: Option<String>,
    },
}

/// Everything one open workspace needs between frames.
///
/// It exists so the workspace can be *stepped*. The loop that used to own these as locals ran
/// until the user left the session, so its caller -- the dashboard, holding the `App` mutex it
/// shares with the embedded web server -- could not let go of that mutex while a session was
/// open. Since "a session is open" is the normal way the TUI is used, that made every web
/// request time out exactly when the web UI is worth having. Handing the state back to the
/// caller lets it take the lock for one frame at a time.
///
/// The two things a frame does that are unbounded -- the daemon lease round-trip and the wait
/// for a keystroke -- are deliberately *not* part of a step; see [`WorkspaceSession::wait`].
pub struct WorkspaceSession {
    /// This attach's own handle on the session's terminals. Holding it is what lets a frame
    /// run without the `App` lock: everything a repaint touches is behind this.
    handle: SessionHandle,
    lease: WorkspaceLease,
    stdout: io::StdoutLock<'static>,
    pending_input: Option<PolledTerminalInput>,
    size: (u16, u16),
    keyboard_enhancement_enabled: bool,
    exit: WorkspaceExit,
    focus: WorkspaceFocus,
    render_bindings: WorkspaceBindings,
    input_router: WorkspaceInputRouter,
    last_signature: Vec<u64>,
    last_layout_key: Option<WorkspaceLayoutKey>,
    clear_next_frame: bool,
    search: Option<WorkspaceSearch>,
    help_open: bool,
    chrome: WorkspaceChrome,
    /// Kept from the last repaint because mouse and scroll input are resolved against the
    /// layout the user is actually looking at, which is the one the previous frame drew.
    layout: WorkspaceLayout,
}

/// What has to change before the panes are re-measured and the screen is cleared.
type WorkspaceLayoutKey = ((u16, u16), usize, usize, Option<PaneTarget>, i16);

/// What one frame's input asked its caller to do, once the caller has the `App` lock back.
///
/// Search is the one thing a keystroke does that the session's own terminals cannot answer:
/// it filters the *session list*, which lives in the `App`. Reporting it instead of calling
/// back into the `App` from inside a frame is what keeps the lock order one-way.
#[derive(Default)]
pub struct WorkspaceInputOutcome {
    pub search: Option<WorkspaceSearchUpdate>,
    pub exit: Option<WorkspaceExit>,
}

impl WorkspaceSession {
    /// Applies whatever the last [`Self::wait`] polled.
    ///
    /// Takes this session's terminal lock and nothing else, so the `App` stays free for the
    /// web server while a keystroke is written to a daemon in another process.
    pub fn apply_input(&mut self, session: &Session) -> io::Result<WorkspaceInputOutcome> {
        let mut outcome = WorkspaceInputOutcome::default();
        let Some(input) = self.pending_input.take() else {
            return Ok(outcome);
        };
        let handle = Arc::clone(&self.handle);
        let terminals = &mut *handle.lock().unwrap();
        outcome.exit = terminals.apply_workspace_input(self, session, input, &mut outcome)?;
        Ok(outcome)
    }

    /// Repaints the workspace from `chrome`, which the caller refreshed under the `App` lock.
    ///
    /// This is where a frame spends nearly all of its time -- polling the daemon for new
    /// output, parsing it, and writing a screen -- and it is deliberately all behind this
    /// session's own lock rather than the `App`'s.
    pub fn render(&mut self, chrome: WorkspaceChrome) -> io::Result<Option<WorkspaceExit>> {
        let handle = Arc::clone(&self.handle);
        let terminals = &mut *handle.lock().unwrap();
        terminals.render_workspace(self, chrome)
    }

    /// Waits for the next input, with the caller's lock released.
    ///
    /// Both halves block: validating the lease is a round-trip to a daemon in another process,
    /// and the poll waits out `timeout` for a keystroke. Neither reads session state, so
    /// neither belongs inside a step -- doing them here is what keeps the shared `App` mutex
    /// free for all but the microseconds a repaint takes.
    pub fn wait(&mut self, timeout: Duration) -> io::Result<()> {
        self.lease.validate()?;
        self.pending_input = Some(poll_terminal_input(timeout)?);
        Ok(())
    }

    /// Puts the terminal back the way the dashboard expects it and drops the input lease.
    ///
    /// Call this on the error paths too: skipping it leaves mouse reporting and the keyboard
    /// enhancement flags on, and leaves the session leased to a workspace nobody is in.
    pub fn finish(mut self) -> io::Result<()> {
        // Before the terminal is handed back: this window is no longer looking at the
        // session's PTYs, so it must stop being one of the viewers they are sized to.
        self.handle.lock().unwrap().release_workspace_viewer();
        if self.keyboard_enhancement_enabled {
            self.stdout.write_all(DISABLE_KEYBOARD_ENHANCEMENT)
        } else {
            Ok(())
        }
        .and_then(|()| self.stdout.write_all(DISABLE_MOUSE_REPORTING))
        .and_then(|()| self.stdout.write_all(b"\x1b[0m\x1b[?25h"))
        .and_then(|()| self.stdout.flush())
    }
}

/// A session's terminals, shared between whatever surfaces are looking at them.
pub type SessionHandle = Arc<Mutex<SessionTerminals>>;

/// The session input lease an attach holds.
///
/// It carries its own copy of everything the daemon needs, so validating and releasing it
/// never touches the terminal map -- and therefore never needs the lock the terminal map
/// lives behind.
struct WorkspaceLease {
    socket: Option<PathBuf>,
    session_key: String,
    owner_id: String,
    held: bool,
}

impl WorkspaceLease {
    fn validate(&self) -> io::Result<()> {
        if self.held
            && let Some(socket) = &self.socket
        {
            response_ok(daemon_request(
                socket,
                &DaemonRequest::ValidateLease {
                    session_key: self.session_key.clone(),
                    owner_id: self.owner_id.clone(),
                },
            )?)
        } else {
            Ok(())
        }
    }
}

/// Releasing on drop rather than at one call site is what makes every way out of an attach --
/// a clean exit, a failed repaint, a terminal that never opened -- give the session back.
impl Drop for WorkspaceLease {
    fn drop(&mut self) {
        if self.held
            && let Some(socket) = &self.socket
        {
            let _ = daemon_request(
                socket,
                &DaemonRequest::Release {
                    session_key: self.session_key.clone(),
                    owner_id: self.owner_id.clone(),
                },
            );
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PaneTarget {
    Agent,
    Shell(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AlternateScrollDirection {
    Older,
    Newer,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AlternateScrollRequest {
    direction: AlternateScrollDirection,
    bytes: Vec<u8>,
}

#[derive(Debug)]
struct AlternateViewCandidate {
    generation: u64,
    stable_since: Instant,
    view: ScreenView,
}

#[derive(Debug)]
struct PendingAlternateScroll {
    direction: AlternateScrollDirection,
    generation: u64,
    started_at: Instant,
    candidate: Option<AlternateViewCandidate>,
}

#[derive(Debug, Eq, PartialEq)]
struct BufferEdit {
    old_positions: Vec<usize>,
}

#[derive(Debug)]
struct AlternateSelectionBuffer {
    pane: PaneTarget,
    rows: Vec<Vec<u8>>,
    viewport_rows: Vec<Vec<u8>>,
    viewport_positions: Vec<usize>,
    chrome_prefix: usize,
    chrome_suffix: usize,
    cols: u16,
    pending_scroll: Option<PendingAlternateScroll>,
    queued_scrolls: VecDeque<AlternateScrollRequest>,
}

impl AlternateSelectionBuffer {
    fn new(pane: PaneTarget, view: ScreenView) -> Option<Self> {
        let (_, cols) = view.size;
        if view.rows.is_empty() || view.rows.len() > SCROLLBACK_LINES || cols == 0 {
            return None;
        }
        let viewport_positions = (0..view.rows.len()).collect();
        Some(Self {
            pane,
            rows: view.rows.clone(),
            viewport_rows: view.rows,
            viewport_positions,
            chrome_prefix: 0,
            chrome_suffix: 0,
            cols,
            pending_scroll: None,
            queued_scrolls: VecDeque::new(),
        })
    }

    fn cell(&self, cell: TerminalCell) -> TerminalBufferCell {
        let row = usize::from(cell.row).min(self.viewport_rows.len().saturating_sub(1));
        TerminalBufferCell {
            row: self.viewport_positions.get(row).copied().unwrap_or(0),
            col: cell.col.min(self.cols.saturating_sub(1)),
        }
    }

    fn queue_scroll(
        &mut self,
        request: AlternateScrollRequest,
        generation: u64,
    ) -> Option<Vec<u8>> {
        if self.pending_scroll.is_some() {
            if self
                .queued_scrolls
                .back()
                .is_some_and(|queued| queued.direction != request.direction)
            {
                self.queued_scrolls.pop_back();
                return None;
            }
            if self.queued_scrolls.len() == ALTERNATE_SCROLL_QUEUE_LIMIT {
                self.queued_scrolls.pop_front();
            }
            self.queued_scrolls.push_back(request);
            return None;
        }
        let bytes = request.bytes;
        self.pending_scroll = Some(PendingAlternateScroll {
            direction: request.direction,
            generation,
            started_at: Instant::now(),
            candidate: None,
        });
        Some(bytes)
    }

    fn begin_next_scroll(&mut self, generation: u64) -> Option<Vec<u8>> {
        let request = self.queued_scrolls.pop_front()?;
        let bytes = request.bytes;
        self.pending_scroll = Some(PendingAlternateScroll {
            direction: request.direction,
            generation,
            started_at: Instant::now(),
            candidate: None,
        });
        Some(bytes)
    }

    fn view_matches(&self, view: &ScreenView) -> bool {
        view.size.1 == self.cols
            && alternate_row_keys(&view.rows, self.cols)
                == alternate_row_keys(&self.viewport_rows, self.cols)
    }

    fn replace_current_view(&mut self, view: ScreenView) {
        for (offset, row) in view.rows.iter().enumerate() {
            if let Some(buffer_row) = self
                .viewport_positions
                .get(offset)
                .and_then(|position| self.rows.get_mut(*position))
            {
                buffer_row.clone_from(row);
            }
        }
        self.viewport_rows = view.rows;
    }

    fn replace_chrome_update(
        &mut self,
        view: ScreenView,
        direction: AlternateScrollDirection,
    ) -> bool {
        if view.rows.len() != self.viewport_rows.len() || view.size.1 != self.cols {
            return false;
        }
        let (prefix, suffix) = infer_alternate_chrome(
            &self.viewport_rows,
            &view.rows,
            self.cols,
            self.chrome_prefix,
            self.chrome_suffix,
            direction,
        );
        if prefix + suffix == 0 {
            return false;
        }
        let old_keys = alternate_row_keys(&self.viewport_rows, self.cols);
        let new_keys = alternate_row_keys(&view.rows, self.cols);
        let changed = old_keys
            .iter()
            .zip(&new_keys)
            .enumerate()
            .filter_map(|(row, (old, new))| (old != new).then_some(row))
            .collect::<Vec<_>>();
        if changed.is_empty()
            || changed
                .iter()
                .any(|row| *row >= prefix && *row < new_keys.len().saturating_sub(suffix))
        {
            return false;
        }
        self.replace_current_view(view);
        true
    }

    fn merge_view(&mut self, view: ScreenView) -> Option<BufferEdit> {
        let pending = self.pending_scroll.take()?;
        if view.rows.is_empty() || view.size.1 != self.cols {
            return None;
        }
        let (chrome_prefix, chrome_suffix) = infer_alternate_chrome(
            &self.viewport_rows,
            &view.rows,
            self.cols,
            self.chrome_prefix,
            self.chrome_suffix,
            pending.direction,
        );
        let mut merged = directional_row_merge(
            &self.rows,
            &self.viewport_positions,
            &view.rows,
            self.cols,
            AlternateChrome {
                prefix: self.chrome_prefix,
                suffix: self.chrome_suffix,
            },
            AlternateChrome {
                prefix: chrome_prefix,
                suffix: chrome_suffix,
            },
            pending.direction,
        );
        bound_alternate_row_merge(&mut merged, pending.direction);
        self.rows = merged.rows;
        self.viewport_rows = view.rows;
        self.viewport_positions = merged.new_positions;
        self.chrome_prefix = chrome_prefix;
        self.chrome_suffix = chrome_suffix;
        Some(BufferEdit {
            old_positions: merged.old_positions,
        })
    }

    fn selected_text(&self, first: TerminalBufferCell, second: TerminalBufferCell) -> String {
        let Some((start, end)) =
            normalized_buffer_selection(first, second, self.rows.len(), self.cols)
        else {
            return String::new();
        };
        (start.row..=end.row)
            .map(|row| {
                let first_col = if row == start.row { start.col } else { 0 };
                let last_col = if row == end.row {
                    end.col
                } else {
                    self.cols - 1
                };
                selected_row_text(
                    self.rows.get(row).map(Vec::as_slice),
                    self.cols,
                    first_col,
                    last_col,
                )
                .trim_end()
                .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn selected_rows(
        &self,
        first: TerminalBufferCell,
        second: TerminalBufferCell,
    ) -> Vec<(TerminalCell, String)> {
        let Some((start, end)) =
            normalized_buffer_selection(first, second, self.rows.len(), self.cols)
        else {
            return Vec::new();
        };
        self.viewport_positions
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, buffer_row)| (start.row..=end.row).contains(buffer_row))
            .map(|(screen_row, buffer_row)| {
                let row = u16::try_from(screen_row).unwrap_or(u16::MAX);
                let first_col = if buffer_row == start.row {
                    start.col
                } else {
                    0
                };
                let last_col = if buffer_row == end.row {
                    end.col
                } else {
                    self.cols - 1
                };
                (
                    TerminalCell {
                        row,
                        col: first_col,
                    },
                    selected_row_text(
                        self.viewport_rows.get(usize::from(row)).map(Vec::as_slice),
                        self.cols,
                        first_col,
                        last_col,
                    ),
                )
            })
            .collect()
    }
}

fn alternate_row_keys(rows: &[Vec<u8>], cols: u16) -> Vec<String> {
    rows.iter()
        .map(|row| {
            selected_row_text(Some(row), cols, 0, cols.saturating_sub(1))
                .trim_end()
                .to_owned()
        })
        .collect()
}

#[derive(Debug)]
struct AlternateRowMerge {
    rows: Vec<Vec<u8>>,
    old_positions: Vec<usize>,
    new_positions: Vec<usize>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct AlternateChrome {
    prefix: usize,
    suffix: usize,
}

fn infer_alternate_chrome(
    old_view: &[Vec<u8>],
    new_view: &[Vec<u8>],
    cols: u16,
    known_prefix: usize,
    known_suffix: usize,
    direction: AlternateScrollDirection,
) -> (usize, usize) {
    let len = old_view.len().min(new_view.len());
    if len < 2 {
        return (0, 0);
    }
    let old_keys = alternate_row_keys(&old_view[..len], cols);
    let new_keys = alternate_row_keys(&new_view[..len], cols);
    let edge_limit = len.saturating_sub(1) / 2;
    let common_prefix = old_keys
        .iter()
        .zip(&new_keys)
        .take(edge_limit)
        .take_while(|(old, new)| old == new)
        .count();
    let common_suffix = old_keys
        .iter()
        .rev()
        .zip(new_keys.iter().rev())
        .take(edge_limit)
        .take_while(|(old, new)| old == new)
        .count();
    let trusted_prefix = known_prefix.min(edge_limit);
    let trusted_suffix = known_suffix
        .min(edge_limit)
        .min(len.saturating_sub(trusted_prefix + 1));
    let mut best: Option<(usize, usize, usize, usize, usize, usize)> = None;
    for prefix in trusted_prefix..=edge_limit {
        let max_suffix = edge_limit.min(len.saturating_sub(prefix + 1));
        for suffix in trusted_suffix..=max_suffix {
            let overlap = directional_boundary_overlap(
                &old_keys[prefix..len - suffix],
                &new_keys[prefix..len - suffix],
                direction,
            );
            if overlap == 0 {
                continue;
            }
            let unsupported = prefix.saturating_sub(common_prefix.max(trusted_prefix))
                + suffix.saturating_sub(common_suffix.max(trusted_suffix));
            let candidate = (
                usize::MAX - unsupported,
                overlap,
                usize::MAX - prefix - suffix,
                usize::MAX - prefix,
                prefix,
                suffix,
            );
            if best.is_none_or(|current| candidate > current) {
                best = Some(candidate);
            }
        }
    }
    if let Some((_, _, _, _, prefix, suffix)) = best {
        return (prefix, suffix);
    }
    if known_prefix + known_suffix > 0 {
        return (trusted_prefix, trusted_suffix);
    }
    if common_prefix > 0 && common_suffix > 0 && common_prefix + common_suffix < len {
        return (common_prefix, common_suffix);
    }
    (0, 0)
}

fn directional_boundary_overlap(
    old_keys: &[String],
    new_keys: &[String],
    direction: AlternateScrollDirection,
) -> usize {
    let max = old_keys.len().min(new_keys.len());
    (1..=max)
        .rev()
        .find(|overlap| {
            let (old, new) = match direction {
                AlternateScrollDirection::Older => {
                    (&old_keys[..*overlap], &new_keys[new_keys.len() - overlap..])
                }
                AlternateScrollDirection::Newer => {
                    (&old_keys[old_keys.len() - overlap..], &new_keys[..*overlap])
                }
            };
            old == new && overlap_has_text(new)
        })
        .unwrap_or(0)
}

fn directional_row_merge(
    old_rows: &[Vec<u8>],
    old_view_positions: &[usize],
    new_rows: &[Vec<u8>],
    cols: u16,
    old_chrome: AlternateChrome,
    new_chrome: AlternateChrome,
    direction: AlternateScrollDirection,
) -> AlternateRowMerge {
    let prefix = old_chrome
        .prefix
        .max(new_chrome.prefix)
        .min(old_rows.len())
        .min(new_rows.len());
    let suffix = old_chrome
        .suffix
        .max(new_chrome.suffix)
        .min(old_rows.len().saturating_sub(prefix))
        .min(new_rows.len().saturating_sub(prefix));
    let old_content_end = old_rows.len().saturating_sub(suffix);
    let new_content_end = new_rows.len().saturating_sub(suffix);
    let old_content = &old_rows[prefix..old_content_end];
    let new_content = &new_rows[prefix..new_content_end];
    let current_positions = old_view_positions
        .get(prefix..old_view_positions.len().saturating_sub(suffix))
        .unwrap_or_default()
        .iter()
        .map(|position| position.saturating_sub(prefix))
        .collect::<Vec<_>>();
    let content = directional_content_merge(
        old_content,
        &current_positions,
        new_content,
        cols,
        direction,
    );
    let content_rows_len = content.rows.len();

    let mut rows = Vec::with_capacity(prefix + content_rows_len + suffix);
    rows.extend_from_slice(&new_rows[..prefix]);
    rows.extend(content.rows);
    rows.extend_from_slice(&new_rows[new_content_end..]);

    let mut old_positions = vec![0; old_rows.len()];
    for (position, mapped) in old_positions.iter_mut().take(prefix).enumerate() {
        *mapped = position.min(rows.len().saturating_sub(1));
    }
    for (position, mapped) in content.old_positions.into_iter().enumerate() {
        old_positions[prefix + position] = prefix + mapped;
    }
    let suffix_start = prefix + content_rows_len;
    for position in 0..suffix {
        old_positions[old_content_end + position] = suffix_start + position;
    }

    let mut new_positions = Vec::with_capacity(new_rows.len());
    new_positions.extend(0..prefix);
    new_positions.extend(
        content
            .new_positions
            .into_iter()
            .map(|position| prefix + position),
    );
    new_positions.extend(suffix_start..suffix_start + suffix);
    AlternateRowMerge {
        rows,
        old_positions,
        new_positions,
    }
}

fn directional_content_merge(
    old_rows: &[Vec<u8>],
    current_positions: &[usize],
    new_rows: &[Vec<u8>],
    cols: u16,
    direction: AlternateScrollDirection,
) -> AlternateRowMerge {
    let old_keys = alternate_row_keys(old_rows, cols);
    let new_keys = alternate_row_keys(new_rows, cols);
    let current_keys = current_positions
        .iter()
        .filter_map(|position| old_keys.get(*position).cloned())
        .collect::<Vec<_>>();
    let mut matched_start = 0;
    let mut matched = 0;
    match direction {
        AlternateScrollDirection::Newer => {
            for overlap in (1..=current_keys.len().min(new_keys.len())).rev() {
                if current_keys[current_keys.len() - overlap..] == new_keys[..overlap]
                    && overlap_has_text(&new_keys[..overlap])
                {
                    matched_start = current_positions[current_positions.len() - overlap];
                    matched = overlap;
                    while matched < new_keys.len()
                        && matched_start + matched < old_keys.len()
                        && old_keys[matched_start + matched] == new_keys[matched]
                    {
                        matched += 1;
                    }
                    break;
                }
            }
            if matched == 0 {
                let first = current_positions
                    .last()
                    .copied()
                    .map_or(0, |position| position.saturating_add(1));
                for start in first..old_keys.len() {
                    let overlap = matching_prefix(&old_keys[start..], &new_keys);
                    if overlap > matched && overlap_is_strong(&new_keys[..overlap]) {
                        matched_start = start;
                        matched = overlap;
                    }
                }
            }
        }
        AlternateScrollDirection::Older => {
            for overlap in (1..=current_keys.len().min(new_keys.len())).rev() {
                if new_keys[new_keys.len() - overlap..] == current_keys[..overlap]
                    && overlap_has_text(&new_keys[new_keys.len() - overlap..])
                {
                    matched_start = current_positions[0];
                    matched = overlap;
                    while matched < new_keys.len()
                        && matched_start > 0
                        && new_keys.len() > matched
                        && old_keys[matched_start - 1] == new_keys[new_keys.len() - matched - 1]
                    {
                        matched_start -= 1;
                        matched += 1;
                    }
                    break;
                }
            }
            if matched == 0 {
                let end = current_positions.first().copied().unwrap_or(old_keys.len());
                for candidate_end in (1..=end.min(old_keys.len())).rev() {
                    let overlap = matching_suffix(&old_keys[..candidate_end], &new_keys);
                    if overlap > matched && overlap_is_strong(&new_keys[new_keys.len() - overlap..])
                    {
                        matched_start = candidate_end - overlap;
                        matched = overlap;
                    }
                }
            }
        }
    }

    let (insertion, inserted_rows, mut new_positions) = match direction {
        AlternateScrollDirection::Newer => {
            let insertion = if matched > 0 {
                matched_start + matched
            } else {
                current_positions
                    .last()
                    .copied()
                    .map_or(old_rows.len(), |position| position + 1)
            }
            .min(old_rows.len());
            let inserted = new_rows[matched..].to_vec();
            let positions = (0..new_rows.len())
                .map(|position| matched_start + position)
                .collect::<Vec<_>>();
            (insertion, inserted, positions)
        }
        AlternateScrollDirection::Older => {
            let new_prefix = new_rows.len().saturating_sub(matched);
            let insertion = if matched > 0 {
                matched_start
            } else {
                current_positions.first().copied().unwrap_or(0)
            }
            .min(old_rows.len());
            let inserted = new_rows[..new_prefix].to_vec();
            let positions = (0..new_rows.len())
                .map(|position| insertion + position)
                .collect::<Vec<_>>();
            (insertion, inserted, positions)
        }
    };
    let inserted = inserted_rows.len();
    let mut rows = old_rows.to_vec();
    rows.splice(insertion..insertion, inserted_rows);
    let old_positions = (0..old_rows.len())
        .map(|position| position + usize::from(position >= insertion) * inserted)
        .collect::<Vec<_>>();
    if direction == AlternateScrollDirection::Newer && matched == 0 {
        new_positions = (insertion..insertion + new_rows.len()).collect();
    }
    for (row, position) in new_rows.iter().zip(&new_positions) {
        if let Some(existing) = rows.get_mut(*position) {
            existing.clone_from(row);
        }
    }
    AlternateRowMerge {
        rows,
        old_positions,
        new_positions,
    }
}

fn matching_prefix(left: &[String], right: &[String]) -> usize {
    left.iter()
        .zip(right)
        .take_while(|(left, right)| left == right)
        .count()
}

fn matching_suffix(left: &[String], right: &[String]) -> usize {
    left.iter()
        .rev()
        .zip(right.iter().rev())
        .take_while(|(left, right)| left == right)
        .count()
}

fn overlap_has_text(keys: &[String]) -> bool {
    keys.iter().any(|key| !key.is_empty())
}

fn overlap_is_strong(keys: &[String]) -> bool {
    keys.len() >= 2
        || keys.iter().any(|key| {
            key.chars()
                .filter(|character| character.is_alphanumeric())
                .take(2)
                .count()
                == 2
        })
}

fn bound_alternate_row_merge(merged: &mut AlternateRowMerge, direction: AlternateScrollDirection) {
    let overflow = merged.rows.len().saturating_sub(SCROLLBACK_LINES);
    if overflow == 0 {
        return;
    }
    let mut protected = vec![false; merged.rows.len()];
    for position in &merged.new_positions {
        protected[*position] = true;
    }
    let mut removed = vec![false; merged.rows.len()];
    let mut remaining = overflow;
    match direction {
        AlternateScrollDirection::Older => {
            for position in (0..merged.rows.len()).rev() {
                if remaining == 0 {
                    break;
                }
                if !protected[position] {
                    removed[position] = true;
                    remaining -= 1;
                }
            }
        }
        AlternateScrollDirection::Newer => {
            for position in 0..merged.rows.len() {
                if remaining == 0 {
                    break;
                }
                if !protected[position] {
                    removed[position] = true;
                    remaining -= 1;
                }
            }
        }
    }
    debug_assert_eq!(remaining, 0);

    let mut kept_before = vec![0; merged.rows.len() + 1];
    for position in 0..merged.rows.len() {
        kept_before[position + 1] = kept_before[position] + usize::from(!removed[position]);
    }
    let retained = kept_before[merged.rows.len()];
    let compact = |position: usize| {
        if !removed[position] {
            kept_before[position]
        } else {
            match direction {
                AlternateScrollDirection::Older => kept_before[position].saturating_sub(1),
                AlternateScrollDirection::Newer => kept_before[position].min(retained - 1),
            }
        }
    };
    for position in &mut merged.old_positions {
        *position = compact(*position);
    }
    for position in &mut merged.new_positions {
        *position = compact(*position);
    }
    let mut position = 0;
    merged.rows.retain(|_| {
        let keep = !removed[position];
        position += 1;
        keep
    });
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TerminalSelection {
    pane: PaneTarget,
    start: TerminalBufferCell,
    end: TerminalBufferCell,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingAgentClick {
    event: WorkspaceMouseEvent,
    cell: TerminalCell,
    selection_start: TerminalBufferCell,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingAlternateCopy {
    pane: PaneTarget,
    cell: TerminalCell,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceChrome {
    pub sessions: Vec<String>,
    pub selected: usize,
    pub selected_session_key: Option<String>,
    pub search_query: String,
    pub status_counts: (usize, usize, usize, usize),
    pub preview: Vec<String>,
    pub notification: Option<String>,
}

struct WorkspaceSearch {
    value: String,
    original_query: String,
    original_selected_session_key: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkspaceSearchInput {
    Editing,
    Commit,
    Cancel,
}

fn apply_workspace_search_input(
    search: &mut WorkspaceSearch,
    input: &[u8],
) -> (WorkspaceSearchInput, bool) {
    let mut text = Vec::new();
    let mut changed = false;
    let flush_text = |text: &mut Vec<u8>, value: &mut String| {
        let appended = std::str::from_utf8(text)
            .ok()
            .map(|text| {
                text.chars()
                    .filter(|character| !character.is_control())
                    .collect::<String>()
            })
            .filter(|text| !text.is_empty());
        text.clear();
        if let Some(appended) = appended {
            value.push_str(&appended);
            true
        } else {
            false
        }
    };

    for byte in input {
        match *byte {
            0x1b => return (WorkspaceSearchInput::Cancel, changed),
            b'\r' | b'\n' => {
                changed |= flush_text(&mut text, &mut search.value);
                return (WorkspaceSearchInput::Commit, changed);
            }
            0x08 | 0x7f => {
                changed |= flush_text(&mut text, &mut search.value);
                changed |= search.value.pop().is_some();
            }
            byte if byte.is_ascii_control() => {}
            byte => text.push(byte),
        }
    }
    changed |= flush_text(&mut text, &mut search.value);
    (WorkspaceSearchInput::Editing, changed)
}

#[derive(Clone, Copy, Debug)]
struct PaneRect {
    top: u16,
    left: u16,
    width: u16,
    height: u16,
}

struct WorkspaceLayout {
    sidebar_width: u16,
    agent: PaneRect,
    shell_divider_row: Option<u16>,
    shell_panes: Vec<(usize, PaneRect)>,
    shell_list: Option<PaneRect>,
    status_row: u16,
    notification_row: u16,
}

#[derive(Clone, Copy)]
struct WorkspaceRenderState<'a> {
    focus: WorkspaceFocus,
    search: Option<&'a str>,
    help: bool,
}

impl WorkspaceLayout {
    fn new(cols: u16, rows: u16, shell_count: usize, selected_shell: usize) -> Self {
        let cols = cols.max(40);
        let rows = rows.max(12);
        let sidebar_width = (cols / 6).clamp(20, 28);
        let right_left = sidebar_width + 1;
        let right_width = cols.saturating_sub(right_left).max(2);
        let status_row = rows - 2;
        let notification_row = rows - 1;
        let content_height = status_row;
        if shell_count == 0 {
            return Self {
                sidebar_width,
                agent: PaneRect {
                    top: 1,
                    left: right_left,
                    width: right_width,
                    height: content_height.saturating_sub(1),
                },
                shell_divider_row: None,
                shell_panes: Vec::new(),
                shell_list: None,
                status_row,
                notification_row,
            };
        }

        let shell_height = (content_height / 3).max(5);
        let agent_height = content_height.saturating_sub(shell_height + 1).max(4);
        let shell_top = agent_height + 1;
        let list_width = if shell_count > 1 {
            (right_width / 5).clamp(14, 20)
        } else {
            0
        };
        let pane_area_width = right_width.saturating_sub(list_width + u16::from(list_width > 0));
        let max_visible = usize::from((pane_area_width / 24).clamp(1, 3));
        let visible_count = shell_count.min(max_visible);
        let selected_shell = selected_shell.min(shell_count - 1);
        let first_visible = selected_shell
            .saturating_sub(visible_count - 1)
            .min(shell_count - visible_count);
        let separators = visible_count.saturating_sub(1) as u16;
        let usable_width = pane_area_width.saturating_sub(separators);
        let base_width = usable_width / visible_count as u16;
        let remainder = usable_width % visible_count as u16;
        let mut left = right_left;
        let mut shell_panes = Vec::with_capacity(visible_count);
        for offset in 0..visible_count {
            let width = base_width + u16::from((offset as u16) < remainder);
            shell_panes.push((
                first_visible + offset,
                PaneRect {
                    top: shell_top,
                    left,
                    width,
                    height: shell_height,
                },
            ));
            left += width + 1;
        }
        let shell_list = (list_width > 0).then_some(PaneRect {
            top: shell_top,
            left: right_left + pane_area_width + 1,
            width: list_width,
            height: shell_height,
        });
        Self {
            sidebar_width,
            agent: PaneRect {
                top: 1,
                left: right_left,
                width: right_width,
                height: agent_height.saturating_sub(1),
            },
            shell_divider_row: Some(agent_height),
            shell_panes,
            shell_list,
            status_row,
            notification_row,
        }
    }

    fn apply_options(&mut self, maximized: Option<PaneTarget>, shell_height_adjust: i16) {
        let right_left = self.agent.left;
        let right_width = self.agent.width;
        match maximized {
            Some(PaneTarget::Agent) => {
                self.agent.top = 1;
                self.agent.height = self.status_row.saturating_sub(1);
                self.shell_divider_row = None;
                self.shell_panes.clear();
                self.shell_list = None;
                return;
            }
            Some(PaneTarget::Shell(index)) => {
                self.agent.height = 0;
                self.shell_divider_row = None;
                self.shell_panes = vec![(
                    index,
                    PaneRect {
                        top: 0,
                        left: right_left,
                        width: right_width,
                        height: self.status_row,
                    },
                )];
                self.shell_list = None;
                return;
            }
            None => {}
        }
        if shell_height_adjust == 0 || self.shell_panes.is_empty() {
            return;
        }
        let old_height = self.shell_panes[0].1.height;
        let max_height = self.status_row.saturating_sub(5).max(5);
        let new_height =
            (old_height as i16 + shell_height_adjust).clamp(5, max_height as i16) as u16;
        let agent_height = self.status_row.saturating_sub(new_height + 1).max(4);
        self.agent.height = agent_height.saturating_sub(1);
        self.shell_divider_row = Some(agent_height);
        for (_, pane) in &mut self.shell_panes {
            pane.top = agent_height + 1;
            pane.height = new_height;
        }
        if let Some(list) = &mut self.shell_list {
            list.top = agent_height + 1;
            list.height = new_height;
        }
    }
}

fn focused_viewport_height(
    layout: &WorkspaceLayout,
    terminals: &SessionTerminals,
    focus: WorkspaceFocus,
) -> u16 {
    let visible = match focus {
        WorkspaceFocus::Sessions => 1,
        WorkspaceFocus::Agent => layout.agent.height,
        WorkspaceFocus::Shell => layout
            .shell_panes
            .iter()
            .find(|(index, _)| *index == terminals.selected_shell)
            .map_or(1, |(_, rect)| rect.height.saturating_sub(1)),
    };
    visible.saturating_sub(1).max(1)
}

fn point_in_rect(col: u16, row: u16, rect: PaneRect) -> bool {
    col >= rect.left
        && col < rect.left.saturating_add(rect.width)
        && row >= rect.top
        && row < rect.top.saturating_add(rect.height)
}

fn pane_at(layout: &WorkspaceLayout, col: u16, row: u16) -> Option<(PaneTarget, TerminalCell)> {
    if point_in_rect(col, row, layout.agent) {
        return Some((
            PaneTarget::Agent,
            TerminalCell {
                row: row - layout.agent.top,
                col: col - layout.agent.left,
            },
        ));
    }
    layout.shell_panes.iter().find_map(|(index, rect)| {
        let terminal = PaneRect {
            top: rect.top + 1,
            left: rect.left,
            width: rect.width,
            height: rect.height.saturating_sub(1),
        };
        point_in_rect(col, row, terminal).then(|| {
            (
                PaneTarget::Shell(*index),
                TerminalCell {
                    row: row - terminal.top,
                    col: col - terminal.left,
                },
            )
        })
    })
}

fn terminal_pane_rect(layout: &WorkspaceLayout, pane: PaneTarget) -> Option<PaneRect> {
    match pane {
        PaneTarget::Agent => Some(layout.agent),
        PaneTarget::Shell(selected) => layout
            .shell_panes
            .iter()
            .find(|(index, _)| *index == selected)
            .map(|(_, rect)| PaneRect {
                top: rect.top + 1,
                left: rect.left,
                width: rect.width,
                height: rect.height.saturating_sub(1),
            }),
    }
}

fn clamped_pane_cell(rect: PaneRect, col: u16, row: u16) -> TerminalCell {
    TerminalCell {
        row: row
            .clamp(
                rect.top,
                rect.top.saturating_add(rect.height.saturating_sub(1)),
            )
            .saturating_sub(rect.top),
        col: col
            .clamp(
                rect.left,
                rect.left.saturating_add(rect.width.saturating_sub(1)),
            )
            .saturating_sub(rect.left),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkspaceCommand {
    ToggleFocus,
    NewShell,
    PreviousShell,
    NextShell,
    CloseShell,
    Dashboard,
    Alert,
    Search,
    Help,
    PreviousSession,
    NextSession,
    SelectShell(usize),
    ToggleMaximize,
    ToggleShellArea,
    GrowShell,
    ShrinkShell,
    CopyCommandBlock,
    ScrollUp,
    ScrollDown,
    LiveTail,
}

#[derive(Clone)]
struct WorkspaceBindings {
    commands: Vec<(Vec<u8>, WorkspaceCommand)>,
    labels: HashMap<String, String>,
}

impl WorkspaceBindings {
    fn from_config(config: &AgentConsoleConfig) -> Self {
        let mut commands = Vec::new();
        let mut labels = HashMap::new();
        let actions = [
            ("focus", WorkspaceCommand::ToggleFocus),
            ("new_shell", WorkspaceCommand::NewShell),
            ("previous_shell", WorkspaceCommand::PreviousShell),
            ("next_shell", WorkspaceCommand::NextShell),
            ("close_shell", WorkspaceCommand::CloseShell),
            ("dashboard", WorkspaceCommand::Dashboard),
            ("alert", WorkspaceCommand::Alert),
            ("search", WorkspaceCommand::Search),
            ("session_alert", WorkspaceCommand::Alert),
            ("help", WorkspaceCommand::Help),
            ("previous_session", WorkspaceCommand::PreviousSession),
            ("next_session", WorkspaceCommand::NextSession),
            ("maximize", WorkspaceCommand::ToggleMaximize),
            ("hide_shells", WorkspaceCommand::ToggleShellArea),
            ("grow_shell", WorkspaceCommand::GrowShell),
            ("shrink_shell", WorkspaceCommand::ShrinkShell),
            ("copy_command", WorkspaceCommand::CopyCommandBlock),
            ("scroll_up", WorkspaceCommand::ScrollUp),
            ("scroll_down", WorkspaceCommand::ScrollDown),
            ("live_tail", WorkspaceCommand::LiveTail),
        ];
        for (action, command) in actions {
            let configured = config.workspace_keys(action);
            if let Some(label) = configured.first() {
                labels.insert(action.to_owned(), format_key_label(label));
            }
            for label in configured {
                for sequence in workspace_key_sequences(&label) {
                    commands.push((sequence, command));
                }
            }
        }
        for index in 0..9 {
            let action = format!("select_shell_{}", index + 1);
            let configured = config.workspace_keys(&action);
            if let Some(label) = configured.first() {
                labels.insert(action, format_key_label(label));
            }
            for label in configured {
                for sequence in workspace_key_sequences(&label) {
                    commands.push((sequence, WorkspaceCommand::SelectShell(index)));
                }
            }
        }
        Self { commands, labels }
    }

    fn command(&self, input: &[u8]) -> Option<WorkspaceCommand> {
        self.commands
            .iter()
            .find_map(|(sequence, command)| (sequence == input).then_some(*command))
    }

    fn is_prefix(&self, input: &[u8]) -> bool {
        self.commands
            .iter()
            .any(|(sequence, _)| sequence.starts_with(input))
    }

    fn label(&self, action: &str) -> &str {
        self.label_opt(action).unwrap_or("unbound")
    }

    fn label_opt(&self, action: &str) -> Option<&str> {
        self.labels.get(action).map(String::as_str)
    }
}

fn workspace_key_sequences(label: &str) -> Vec<Vec<u8>> {
    let lower = label.to_ascii_lowercase();
    match lower.as_str() {
        "ctrl-up" => vec![b"\x1b[1;5A".to_vec()],
        "ctrl-down" => vec![b"\x1b[1;5B".to_vec()],
        "shift-pageup" => vec![b"\x1b[5;2~".to_vec()],
        "shift-pagedown" => vec![b"\x1b[6;2~".to_vec()],
        "shift-end" => vec![b"\x1b[1;2F".to_vec()],
        value if value.starts_with("alt-") && value[4..].chars().count() == 1 => {
            let character = value[4..].chars().next().unwrap();
            let mut sequence = vec![0x1b];
            sequence.extend_from_slice(character.to_string().as_bytes());
            vec![
                sequence,
                format!("\x1b[{};3u", u32::from(character)).into_bytes(),
                format!("\x1b[27;3;{}~", u32::from(character)).into_bytes(),
            ]
        }
        value if value.starts_with("ctrl-") && value[5..].chars().count() == 1 => {
            let character = value[5..].chars().next().unwrap();
            let Some(ascii) = character.is_ascii().then_some(character as u8) else {
                return Vec::new();
            };
            vec![
                vec![ascii & 0x1f],
                format!("\x1b[{};5u", u32::from(character)).into_bytes(),
                format!("\x1b[27;5;{}~", u32::from(character)).into_bytes(),
            ]
        }
        value if value.chars().count() == 1 => vec![value.as_bytes().to_vec()],
        _ => Vec::new(),
    }
}

#[cfg(test)]
fn workspace_command(input: &[u8]) -> Option<WorkspaceCommand> {
    WorkspaceBindings::from_config(&AgentConsoleConfig::default()).command(input)
}

enum WorkspaceInput {
    Forward(Vec<u8>),
    Command(WorkspaceCommand),
    Mouse(WorkspaceMouseEvent),
}

enum PolledTerminalInput {
    Pending,
    EndOfFile,
    Bytes(Vec<u8>),
}

#[cfg(unix)]
fn poll_terminal_input(timeout: Duration) -> io::Result<PolledTerminalInput> {
    let mut descriptor = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: descriptor points to one valid pollfd for the duration of the call.
    let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
    if ready < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(PolledTerminalInput::Pending);
        }
        return Err(error);
    }
    if ready == 0 || descriptor.revents & libc::POLLIN == 0 {
        return Ok(PolledTerminalInput::Pending);
    }
    let mut input = [0_u8; 4096];
    let read = io::stdin().read(&mut input)?;
    if read == 0 {
        Ok(PolledTerminalInput::EndOfFile)
    } else {
        Ok(PolledTerminalInput::Bytes(input[..read].to_vec()))
    }
}

#[cfg(not(unix))]
fn poll_terminal_input(timeout: Duration) -> io::Result<PolledTerminalInput> {
    if !event::poll(timeout)? {
        return Ok(PolledTerminalInput::Pending);
    }
    let bytes = terminal_event_bytes(event::read()?);
    if bytes.is_empty() {
        Ok(PolledTerminalInput::Pending)
    } else {
        Ok(PolledTerminalInput::Bytes(bytes))
    }
}

#[cfg(any(not(unix), test))]
fn terminal_event_bytes(event: Event) -> Vec<u8> {
    match event {
        Event::Key(key) if key.kind != KeyEventKind::Release => terminal_key_bytes(key),
        Event::Paste(text) => text.into_bytes(),
        Event::Mouse(mouse) => terminal_mouse_bytes(mouse),
        _ => Vec::new(),
    }
}

#[cfg(any(not(unix), test))]
fn terminal_key_bytes(key: KeyEvent) -> Vec<u8> {
    let modifier = 1
        + usize::from(key.modifiers.contains(KeyModifiers::SHIFT))
        + 2 * usize::from(key.modifiers.contains(KeyModifiers::ALT))
        + 4 * usize::from(key.modifiers.contains(KeyModifiers::CONTROL));
    let modified_csi = |final_byte: char| format!("\x1b[1;{modifier}{final_byte}").into_bytes();
    let mut bytes = match key.code {
        KeyCode::Char(character) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            control_character(character).map_or_else(Vec::new, |byte| vec![byte])
        }
        KeyCode::Char(character) => character.to_string().into_bytes(),
        KeyCode::Enter if modifier == 1 => vec![b'\r'],
        KeyCode::Enter => format!("\x1b[13;{modifier}u").into_bytes(),
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Left if modifier == 1 => b"\x1b[D".to_vec(),
        KeyCode::Right if modifier == 1 => b"\x1b[C".to_vec(),
        KeyCode::Up if modifier == 1 => b"\x1b[A".to_vec(),
        KeyCode::Down if modifier == 1 => b"\x1b[B".to_vec(),
        KeyCode::Left => modified_csi('D'),
        KeyCode::Right => modified_csi('C'),
        KeyCode::Up => modified_csi('A'),
        KeyCode::Down => modified_csi('B'),
        KeyCode::Home if modifier == 1 => b"\x1b[H".to_vec(),
        KeyCode::End if modifier == 1 => b"\x1b[F".to_vec(),
        KeyCode::Home => modified_csi('H'),
        KeyCode::End => modified_csi('F'),
        KeyCode::Insert => format!("\x1b[2;{modifier}~").into_bytes(),
        KeyCode::Delete => format!("\x1b[3;{modifier}~").into_bytes(),
        KeyCode::PageUp => format!("\x1b[5;{modifier}~").into_bytes(),
        KeyCode::PageDown => format!("\x1b[6;{modifier}~").into_bytes(),
        KeyCode::F(number) => function_key_bytes(number, modifier),
        KeyCode::Null => vec![0],
        _ => Vec::new(),
    };
    if key.modifiers.contains(KeyModifiers::ALT)
        && matches!(key.code, KeyCode::Char(_))
        && !key.modifiers.contains(KeyModifiers::CONTROL)
    {
        bytes.insert(0, 0x1b);
    }
    bytes
}

#[cfg(any(not(unix), test))]
fn control_character(character: char) -> Option<u8> {
    match character.to_ascii_uppercase() {
        '@' | ' ' => Some(0),
        value @ 'A'..='_' => Some(value as u8 & 0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

#[cfg(any(not(unix), test))]
fn function_key_bytes(number: u8, modifier: usize) -> Vec<u8> {
    let code = match number {
        1 => "P",
        2 => "Q",
        3 => "R",
        4 => "S",
        5 => "15~",
        6 => "17~",
        7 => "18~",
        8 => "19~",
        9 => "20~",
        10 => "21~",
        11 => "23~",
        12 => "24~",
        _ => return Vec::new(),
    };
    if modifier == 1 && number <= 4 {
        format!("\x1bO{code}").into_bytes()
    } else if number <= 4 {
        format!("\x1b[1;{modifier}{code}").into_bytes()
    } else if modifier == 1 {
        format!("\x1b[{code}").into_bytes()
    } else {
        format!("\x1b[{};{modifier}~", code.trim_end_matches('~')).into_bytes()
    }
}

#[cfg(any(not(unix), test))]
fn terminal_mouse_bytes(mouse: MouseEvent) -> Vec<u8> {
    let (button, pressed) = match mouse.kind {
        MouseEventKind::Down(button) => (mouse_button_code(button), true),
        MouseEventKind::Up(button) => (mouse_button_code(button), false),
        MouseEventKind::Drag(button) => (mouse_button_code(button) | 32, true),
        MouseEventKind::Moved => (35, true),
        MouseEventKind::ScrollUp => (64, true),
        MouseEventKind::ScrollDown => (65, true),
        MouseEventKind::ScrollLeft => (66, true),
        MouseEventKind::ScrollRight => (67, true),
    };
    let suffix = if pressed { 'M' } else { 'm' };
    format!(
        "\x1b[<{button};{};{}{suffix}",
        mouse.column.saturating_add(1),
        mouse.row.saturating_add(1)
    )
    .into_bytes()
}

#[cfg(any(not(unix), test))]
fn mouse_button_code(button: MouseButton) -> u16 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionListInput {
    Previous,
    Next,
    Activate,
    NewSession,
    OpenShell,
    ToggleArchive,
}

fn session_list_input(bytes: &[u8]) -> Option<SessionListInput> {
    match bytes {
        b"k" | b"\x1b[A" => Some(SessionListInput::Previous),
        b"j" | b"\x1b[B" => Some(SessionListInput::Next),
        b"\r" | b"\n" => Some(SessionListInput::Activate),
        b"n" => Some(SessionListInput::NewSession),
        b"s" => Some(SessionListInput::OpenShell),
        b"x" => Some(SessionListInput::ToggleArchive),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WorkspaceMouseEvent {
    button: u16,
    col: u16,
    row: u16,
    pressed: bool,
}

fn encoded_child_mouse_event(
    event: WorkspaceMouseEvent,
    cell: TerminalCell,
    encoding: vt100::MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    let col = cell.col.saturating_add(1);
    let row = cell.row.saturating_add(1);
    match encoding {
        vt100::MouseProtocolEncoding::Sgr => Some(
            format!(
                "\x1b[<{};{col};{row}{}",
                event.button,
                if event.pressed { 'M' } else { 'm' }
            )
            .into_bytes(),
        ),
        vt100::MouseProtocolEncoding::Default => {
            let button = u8::try_from(event.button.saturating_add(32)).ok()?;
            let col = u8::try_from(col.saturating_add(32)).ok()?;
            let row = u8::try_from(row.saturating_add(32)).ok()?;
            Some(vec![0x1b, b'[', b'M', button, col, row])
        }
        vt100::MouseProtocolEncoding::Utf8 => {
            let button = u8::try_from(event.button.saturating_add(32)).ok()?;
            let mut encoded = vec![0x1b, b'[', b'M', button];
            encoded.extend(
                char::from_u32(u32::from(col.saturating_add(32)))?
                    .to_string()
                    .bytes(),
            );
            encoded.extend(
                char::from_u32(u32::from(row.saturating_add(32)))?
                    .to_string()
                    .bytes(),
            );
            Some(encoded)
        }
    }
}

fn alternate_screen_scroll(button: u16) -> Option<Vec<u8>> {
    let arrow = match button {
        64 => b"\x1b[A".as_slice(),
        65 => b"\x1b[B".as_slice(),
        _ => return None,
    };
    Some(arrow.repeat(3))
}

struct WorkspaceInputRouter {
    pending: Vec<u8>,
    bindings: WorkspaceBindings,
}

impl Default for WorkspaceInputRouter {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
            bindings: WorkspaceBindings::from_config(&AgentConsoleConfig::default()),
        }
    }
}

impl WorkspaceInputRouter {
    fn route(&mut self, input: &[u8], focus: WorkspaceFocus) -> Vec<WorkspaceInput> {
        let mut routed = Vec::new();
        for &byte in input {
            self.pending.push(byte);
            loop {
                if let Some(event) = workspace_mouse_event(&self.pending) {
                    self.pending.clear();
                    routed.push(WorkspaceInput::Mouse(event));
                    break;
                }
                if is_workspace_mouse_prefix(&self.pending) {
                    break;
                }
                if let Some(command) = self.bindings.command(&self.pending) {
                    if workspace_command_active(command, focus, &self.pending) {
                        self.pending.clear();
                        routed.push(WorkspaceInput::Command(command));
                        break;
                    }
                    let byte = self.pending.remove(0);
                    match routed.last_mut() {
                        Some(WorkspaceInput::Forward(bytes)) => bytes.push(byte),
                        _ => routed.push(WorkspaceInput::Forward(vec![byte])),
                    }
                    if self.pending.is_empty() {
                        break;
                    }
                    continue;
                }
                if self.bindings.is_prefix(&self.pending) {
                    break;
                }
                let byte = self.pending.remove(0);
                match routed.last_mut() {
                    Some(WorkspaceInput::Forward(bytes)) => bytes.push(byte),
                    _ => routed.push(WorkspaceInput::Forward(vec![byte])),
                }
                if self.pending.is_empty() {
                    break;
                }
            }
        }
        routed
    }

    fn flush(&mut self) -> Option<Vec<u8>> {
        (!self.pending.is_empty()).then(|| std::mem::take(&mut self.pending))
    }
}

fn workspace_command_active(
    command: WorkspaceCommand,
    focus: WorkspaceFocus,
    input: &[u8],
) -> bool {
    if focus != WorkspaceFocus::Sessions && is_printable_character(input) {
        return false;
    }
    match command {
        WorkspaceCommand::ToggleFocus
        | WorkspaceCommand::NewShell
        | WorkspaceCommand::Dashboard
        | WorkspaceCommand::Alert
        | WorkspaceCommand::PreviousSession
        | WorkspaceCommand::NextSession => true,
        WorkspaceCommand::ScrollUp | WorkspaceCommand::ScrollDown | WorkspaceCommand::LiveTail => {
            focus != WorkspaceFocus::Sessions
        }
        WorkspaceCommand::NextShell | WorkspaceCommand::CloseShell => {
            focus == WorkspaceFocus::Shell
        }
        WorkspaceCommand::PreviousShell
        | WorkspaceCommand::Search
        | WorkspaceCommand::Help
        | WorkspaceCommand::SelectShell(_)
        | WorkspaceCommand::ToggleMaximize
        | WorkspaceCommand::ToggleShellArea
        | WorkspaceCommand::GrowShell
        | WorkspaceCommand::ShrinkShell
        | WorkspaceCommand::CopyCommandBlock => focus == WorkspaceFocus::Sessions,
    }
}

fn is_printable_character(input: &[u8]) -> bool {
    std::str::from_utf8(input).is_ok_and(|value| {
        let mut characters = value.chars();
        characters
            .next()
            .is_some_and(|character| !character.is_control())
            && characters.next().is_none()
    })
}

fn workspace_mouse_event(input: &[u8]) -> Option<WorkspaceMouseEvent> {
    if input.starts_with(b"\x1b[M") {
        if input.len() != 6 || input[3..].iter().any(|byte| *byte < 32) {
            return None;
        }
        return Some(WorkspaceMouseEvent {
            button: u16::from(input[3] - 32),
            col: u16::from(input[4] - 32),
            row: u16::from(input[5] - 32),
            pressed: true,
        });
    }
    if !input.starts_with(b"\x1b[<") {
        return None;
    }
    let pressed = match input.last() {
        Some(b'M') => true,
        Some(b'm') => false,
        _ => return None,
    };
    let body = std::str::from_utf8(&input[3..input.len().saturating_sub(1)]).ok()?;
    let mut fields = body.split(';');
    let button = fields.next()?.parse().ok()?;
    let col = fields.next()?.parse().ok()?;
    let row = fields.next()?.parse().ok()?;
    if fields.next().is_some() || col == 0 || row == 0 {
        return None;
    }
    Some(WorkspaceMouseEvent {
        button,
        col,
        row,
        pressed,
    })
}

fn is_workspace_mouse_prefix(input: &[u8]) -> bool {
    if b"\x1b[M".starts_with(input) {
        return true;
    }
    if input.starts_with(b"\x1b[M") {
        return input.len() < 6;
    }
    if b"\x1b[<".starts_with(input) {
        return true;
    }
    input.starts_with(b"\x1b[<")
        && input[3..]
            .iter()
            .all(|byte| byte.is_ascii_digit() || *byte == b';')
}

impl SessionTerminals {
    fn next_shell_name(&self) -> String {
        let mut number = 1;
        loop {
            let candidate = format!("shell {number}");
            if self.shells.iter().all(|shell| shell.name != candidate) {
                return candidate;
            }
            number += 1;
        }
    }

    fn shell_label(&self, index: usize) -> String {
        let Some(shell) = self.shells.get(index) else {
            return format!("shell {}", index + 1);
        };
        shell.terminal.exit_description().map_or_else(
            || shell.name.clone(),
            |exit| format!("{} · EXIT {exit}", shell.name),
        )
    }

    /// Starts a shell in the session's working directory, paired with the id that names it
    /// from outside this module. With the daemon up that id is the unique suffix of the
    /// daemon terminal id, which is what lets a shell started here and one started by another
    /// surface (a TUI, a browser) refer to the same terminal.
    fn spawn_shell(
        &self,
        session: &Session,
        size: (u16, u16),
    ) -> io::Result<(String, ManagedTerminal)> {
        let id = Uuid::new_v4().to_string();
        let spec = shell_command(&session.cwd);
        let terminal = if let Some(socket) = &self.daemon_socket {
            ManagedTerminal::ensure_remote(
                socket.clone(),
                format!("shell|{}|{id}", session.key),
                self.lease_owner_id.clone(),
                &spec,
                size,
            )?
        } else {
            spawn_shell(session, size)?
        };
        Ok((id, terminal))
    }

    fn toggle_workspace_focus(
        &mut self,
        _session: &Session,
        focus: WorkspaceFocus,
    ) -> io::Result<WorkspaceFocus> {
        Ok(match focus {
            WorkspaceFocus::Agent if self.shells.is_empty() => WorkspaceFocus::Sessions,
            WorkspaceFocus::Agent => WorkspaceFocus::Shell,
            WorkspaceFocus::Shell => WorkspaceFocus::Sessions,
            WorkspaceFocus::Sessions => WorkspaceFocus::Agent,
        })
    }

    fn handle_mouse(
        &mut self,
        layout: &WorkspaceLayout,
        event: WorkspaceMouseEvent,
    ) -> io::Result<()> {
        let col = event.col.saturating_sub(1);
        let row = event.row.saturating_sub(1);
        let button = event.button & !(4 | 8 | 16 | 32);
        let dragging = event.button & 32 != 0;
        let continuing_selection = button == 0 && (dragging || !event.pressed);
        if self.suppressed_mouse_buttons != 0 {
            if matches!(button, 0..=2) {
                let mask = 1_u8 << button;
                if event.pressed {
                    self.suppressed_mouse_buttons |= mask;
                } else {
                    self.suppressed_mouse_buttons &= !mask;
                }
            }
            return Ok(());
        }
        if button == 0
            && !event.pressed
            && let Some(capture) = &mut self.alternate_selection
        {
            capture.queued_scrolls.clear();
        }
        self.refresh_alternate_selection()?;
        if self.pending_alternate_copy.is_some() && !self.finish_pending_alternate_copy() {
            if matches!(button, 0..=2) && event.pressed {
                self.suppressed_mouse_buttons |= 1_u8 << button;
            }
            return Ok(());
        }
        let selection_pane = self
            .selection
            .map(|selection| selection.pane)
            .or_else(|| self.pending_agent_click.map(|_| PaneTarget::Agent));
        let hit = pane_at(layout, col, row);
        let (pane, cell, edge_scroll) = if continuing_selection
            && let Some(selection_pane) = selection_pane
            && hit.is_none_or(|(pane, _)| pane != selection_pane)
            && let Some(rect) = terminal_pane_rect(layout, selection_pane)
        {
            let edge_scroll = if dragging && row < rect.top {
                3
            } else if dragging && row >= rect.top.saturating_add(rect.height) {
                -3
            } else {
                0
            };
            (
                selection_pane,
                clamped_pane_cell(rect, col, row),
                edge_scroll,
            )
        } else if let Some((pane, cell)) = hit {
            (pane, cell, 0)
        } else {
            return Ok(());
        };
        if edge_scroll != 0 {
            let alternate = pane == PaneTarget::Agent
                && self
                    .terminal(pane)
                    .is_some_and(ManagedTerminal::alternate_screen);
            if alternate {
                let button = if edge_scroll > 0 { 64 } else { 65 };
                let direction = if edge_scroll > 0 {
                    AlternateScrollDirection::Older
                } else {
                    AlternateScrollDirection::Newer
                };
                if let Some(bytes) = alternate_screen_scroll(button) {
                    self.queue_alternate_scroll(pane, direction, bytes)?;
                }
            } else if let Some(terminal) = self.terminal(pane) {
                terminal.scroll_viewport(edge_scroll);
            }
        }
        if pane == PaneTarget::Agent
            && matches!(button, 64 | 65)
            && let Some(terminal) = self.terminal(pane)
        {
            let before = terminal.scrollback_offset();
            let amount = if button == 64 { 3 } else { -3 };
            let after = terminal.scroll_viewport(amount);
            let selection_end = self.selection_cell(pane, cell);
            if let Some(selection) = &mut self.selection
                && selection.pane == pane
                && let Some(selection_end) = selection_end
            {
                selection.end = selection_end;
            }
            if before > 0 || after > 0 {
                return Ok(());
            }
        }
        if button == 0
            && !event.pressed
            && (self.selection.is_some() || self.pending_agent_click.is_some())
            && self
                .alternate_selection
                .as_ref()
                .is_some_and(|capture| capture.pane == pane && capture.pending_scroll.is_some())
        {
            self.pending_alternate_copy = Some(PendingAlternateCopy { pane, cell });
            return Ok(());
        }
        if pane == PaneTarget::Agent && event.button & 4 == 0 {
            let mouse_protocol = self.terminal(pane).map(ManagedTerminal::mouse_protocol);
            if let Some((mode, encoding)) = mouse_protocol
                && mode != vt100::MouseProtocolMode::None
            {
                if matches!(button, 64 | 65)
                    && self
                        .alternate_selection
                        .as_ref()
                        .is_some_and(|capture| capture.pane == pane)
                    && self
                        .terminal(pane)
                        .is_some_and(ManagedTerminal::alternate_screen)
                    && let Some(bytes) = encoded_child_mouse_event(event, cell, encoding)
                {
                    let direction = if button == 64 {
                        AlternateScrollDirection::Older
                    } else {
                        AlternateScrollDirection::Newer
                    };
                    self.queue_alternate_scroll(pane, direction, bytes)?;
                    return Ok(());
                }
                if button == 0 && event.pressed && !dragging {
                    self.selection = None;
                    let Some(selection_start) = self.begin_selection(pane, cell) else {
                        return Ok(());
                    };
                    self.pending_agent_click = Some(PendingAgentClick {
                        event,
                        cell,
                        selection_start,
                    });
                    return Ok(());
                }
                if button == 0 && event.pressed && dragging {
                    if let Some(pending) = self.pending_agent_click.take() {
                        let Some(end) = self.selection_cell(pane, cell) else {
                            return Ok(());
                        };
                        self.selection = Some(TerminalSelection {
                            pane,
                            start: pending.selection_start,
                            end,
                        });
                        return Ok(());
                    }
                    let end = self.selection_cell(pane, cell);
                    if let Some(selection) = &mut self.selection
                        && selection.pane == pane
                        && let Some(end) = end
                    {
                        selection.end = end;
                        return Ok(());
                    }
                }
                if button == 0 && !event.pressed {
                    let end = self.selection_cell(pane, cell);
                    if let Some(selection) = &mut self.selection
                        && selection.pane == pane
                        && let Some(end) = end
                    {
                        selection.end = end;
                        self.pending_agent_click = None;
                        self.finish_selection_copy();
                        return Ok(());
                    }
                    if let Some(pending) = self.pending_agent_click.take() {
                        if let Some(terminal) = self.terminal(pane) {
                            if let Some(bytes) =
                                encoded_child_mouse_event(pending.event, pending.cell, encoding)
                            {
                                terminal.write(&bytes)?;
                            }
                            if let Some(bytes) = encoded_child_mouse_event(event, cell, encoding) {
                                terminal.write(&bytes)?;
                            }
                        }
                        return Ok(());
                    }
                }
                if let Some(bytes) = encoded_child_mouse_event(event, cell, encoding)
                    && let Some(terminal) = self.terminal(pane)
                {
                    terminal.write(&bytes)?;
                    return Ok(());
                }
            }
        }
        match button {
            64 | 65 => {
                let alternate = pane == PaneTarget::Agent
                    && self
                        .terminal(pane)
                        .is_some_and(ManagedTerminal::alternate_screen);
                if alternate {
                    let end = self.selection_cell(pane, cell);
                    if let Some(selection) = &mut self.selection
                        && selection.pane == pane
                        && let Some(end) = end
                    {
                        selection.end = end;
                    }
                    let direction = if button == 64 {
                        AlternateScrollDirection::Older
                    } else {
                        AlternateScrollDirection::Newer
                    };
                    if let Some(bytes) = alternate_screen_scroll(button) {
                        self.queue_alternate_scroll(pane, direction, bytes)?;
                    }
                    return Ok(());
                }
                if let Some(terminal) = self.terminal(pane) {
                    let amount = if button == 64 { 3 } else { -3 };
                    terminal.scroll_viewport(amount);
                    let selection_end = self.selection_cell(pane, cell);
                    if let Some(selection) = &mut self.selection
                        && selection.pane == pane
                        && let Some(selection_end) = selection_end
                    {
                        selection.end = selection_end;
                    }
                }
            }
            0 if event.pressed && event.button & 32 == 0 => {
                if let Some(cell) = self.begin_selection(pane, cell) {
                    self.selection = Some(TerminalSelection {
                        pane,
                        start: cell,
                        end: cell,
                    });
                }
            }
            0 if event.pressed && event.button & 32 != 0 => {
                let end = self.selection_cell(pane, cell);
                if let Some(selection) = &mut self.selection
                    && selection.pane == pane
                    && let Some(end) = end
                {
                    selection.end = end;
                }
            }
            0 if !event.pressed => {
                let end = self.selection_cell(pane, cell);
                if let Some(selection) = &mut self.selection
                    && selection.pane == pane
                    && let Some(end) = end
                {
                    selection.end = end;
                }
                self.finish_selection_copy();
            }
            _ => {}
        }
        Ok(())
    }

    fn begin_selection(
        &mut self,
        pane: PaneTarget,
        cell: TerminalCell,
    ) -> Option<TerminalBufferCell> {
        self.alternate_selection = None;
        self.pending_alternate_copy = None;
        let terminal = self.terminal(pane)?;
        if terminal.alternate_screen() {
            let capture = AlternateSelectionBuffer::new(pane, terminal.screen_view())?;
            let cell = capture.cell(cell);
            self.alternate_selection = Some(capture);
            Some(cell)
        } else {
            terminal.selection_cell(cell)
        }
    }

    fn selection_cell(
        &mut self,
        pane: PaneTarget,
        cell: TerminalCell,
    ) -> Option<TerminalBufferCell> {
        if let Some(capture) = self
            .alternate_selection
            .as_ref()
            .filter(|capture| capture.pane == pane)
        {
            Some(capture.cell(cell))
        } else {
            self.terminal(pane)?.selection_cell(cell)
        }
    }

    fn queue_alternate_scroll(
        &mut self,
        pane: PaneTarget,
        direction: AlternateScrollDirection,
        bytes: Vec<u8>,
    ) -> io::Result<()> {
        let generation = self.terminal(pane).map(ManagedTerminal::output_generation);
        let bytes = if let Some(capture) = self
            .alternate_selection
            .as_mut()
            .filter(|capture| capture.pane == pane)
            && let Some(generation) = generation
        {
            capture.queue_scroll(AlternateScrollRequest { direction, bytes }, generation)
        } else {
            Some(bytes)
        };
        if let Some(bytes) = bytes
            && let Some(terminal) = self.terminal(pane)
        {
            terminal.write(&bytes)?;
        }
        Ok(())
    }

    fn refresh_alternate_selection(&mut self) -> io::Result<bool> {
        let Some(pane) = self
            .alternate_selection
            .as_ref()
            .and_then(|capture| capture.pending_scroll.as_ref().map(|_| capture.pane))
        else {
            return Ok(false);
        };
        let Some(generation) = self.terminal(pane).map(ManagedTerminal::output_generation) else {
            self.alternate_selection = None;
            return Ok(true);
        };
        let now = Instant::now();
        let (candidate, timed_out) = {
            let pending = self
                .alternate_selection
                .as_mut()
                .and_then(|capture| capture.pending_scroll.as_mut())
                .expect("the alternate pane was resolved from a pending scroll");
            let timed_out = now.duration_since(pending.started_at) >= ALTERNATE_SCROLL_TIMEOUT;
            let ready = pending.candidate.as_ref().is_some_and(|candidate| {
                candidate.generation == generation
                    && (timed_out
                        || now.duration_since(candidate.stable_since) >= ALTERNATE_REPAINT_SETTLE)
            });
            (
                ready.then(|| pending.candidate.take().unwrap().view),
                timed_out,
            )
        };
        if let Some(view) = candidate {
            return self.commit_alternate_view(pane, generation, view);
        }

        let pending_generation = self
            .alternate_selection
            .as_ref()
            .and_then(|capture| capture.pending_scroll.as_ref())
            .map(|pending| pending.generation);
        if pending_generation == Some(generation) {
            if timed_out {
                return self.complete_alternate_scroll(pane, generation);
            }
            return Ok(false);
        }

        let Some(view) = self.terminal(pane).map(ManagedTerminal::screen_view) else {
            self.alternate_selection = None;
            return Ok(true);
        };
        if let Some(capture) = self.alternate_selection.as_mut() {
            if capture.view_matches(&view) {
                capture.replace_current_view(view);
                if let Some(pending) = &mut capture.pending_scroll {
                    pending.generation = generation;
                    pending.candidate = None;
                }
                if timed_out {
                    return self.complete_alternate_scroll(pane, generation);
                }
                return Ok(true);
            }
            let direction = capture
                .pending_scroll
                .as_ref()
                .map(|pending| pending.direction);
            if let Some(direction) = direction
                && capture.replace_chrome_update(view.clone(), direction)
            {
                if let Some(pending) = &mut capture.pending_scroll {
                    pending.generation = generation;
                    pending.candidate = None;
                }
                if timed_out {
                    return self.complete_alternate_scroll(pane, generation);
                }
                return Ok(true);
            }
            if timed_out {
                return self.commit_alternate_view(pane, generation, view);
            }
            if let Some(pending) = &mut capture.pending_scroll {
                pending.generation = generation;
                pending.candidate = Some(AlternateViewCandidate {
                    generation,
                    stable_since: now,
                    view,
                });
            }
        }
        Ok(false)
    }

    fn complete_alternate_scroll(&mut self, pane: PaneTarget, generation: u64) -> io::Result<bool> {
        if let Some(capture) = self
            .alternate_selection
            .as_mut()
            .filter(|capture| capture.pane == pane)
        {
            capture.pending_scroll = None;
        }
        self.start_next_alternate_scroll(pane, generation)?;
        Ok(true)
    }

    fn commit_alternate_view(
        &mut self,
        pane: PaneTarget,
        generation: u64,
        view: ScreenView,
    ) -> io::Result<bool> {
        let edit = self
            .alternate_selection
            .as_mut()
            .and_then(|capture| capture.merge_view(view));
        if let Some(edit) = edit {
            let rebase = |endpoint: &mut TerminalBufferCell| {
                endpoint.row = edit
                    .old_positions
                    .get(endpoint.row)
                    .copied()
                    .or_else(|| edit.old_positions.last().copied())
                    .unwrap_or(0);
            };
            if let Some(selection) = &mut self.selection
                && selection.pane == pane
            {
                rebase(&mut selection.start);
                rebase(&mut selection.end);
            }
            if let Some(pending_click) = &mut self.pending_agent_click
                && pane == PaneTarget::Agent
            {
                rebase(&mut pending_click.selection_start);
            }
        }
        self.start_next_alternate_scroll(pane, generation)?;
        Ok(true)
    }

    fn start_next_alternate_scroll(&mut self, pane: PaneTarget, generation: u64) -> io::Result<()> {
        let next = self
            .alternate_selection
            .as_mut()
            .and_then(|capture| capture.begin_next_scroll(generation));
        if let Some(bytes) = next
            && let Some(terminal) = self.terminal(pane)
        {
            terminal.write(&bytes)?;
        }
        Ok(())
    }

    fn copy_selection_to_clipboard(&mut self) {
        self.notice = match self.selected_text() {
            Some(text) => match clipboard::copy(&text) {
                Ok(()) => Some(format!(
                    "selection copied · {} chars · paste directly; Option-drag uses iTerm selection",
                    text.chars().count()
                )),
                Err(error) => Some(format!("copy failed: {error}")),
            },
            None => Some("nothing selected".into()),
        };
    }

    fn finish_selection_copy(&mut self) {
        self.pending_alternate_copy = None;
        if self
            .selection
            .is_some_and(|selection| selection.start == selection.end)
        {
            self.selection = None;
            self.alternate_selection = None;
            return;
        }
        if let Some(capture) = &mut self.alternate_selection {
            capture.pending_scroll = None;
            capture.queued_scrolls.clear();
        }
        self.copy_selection_to_clipboard();
    }

    fn finish_pending_alternate_copy(&mut self) -> bool {
        let Some(pending) = self.pending_alternate_copy else {
            return false;
        };
        let Some(capture) = self
            .alternate_selection
            .as_ref()
            .filter(|capture| capture.pane == pending.pane)
        else {
            self.pending_alternate_copy = None;
            return true;
        };
        if capture.pending_scroll.is_some() || !capture.queued_scrolls.is_empty() {
            return false;
        }
        let end = capture.cell(pending.cell);
        if let Some(selection) = &mut self.selection
            && selection.pane == pending.pane
        {
            selection.end = end;
        } else if let Some(click) = self.pending_agent_click.take() {
            self.selection = Some(TerminalSelection {
                pane: pending.pane,
                start: click.selection_start,
                end,
            });
        }
        self.finish_selection_copy();
        true
    }

    fn terminal(&self, pane: PaneTarget) -> Option<&ManagedTerminal> {
        match pane {
            PaneTarget::Agent => self.agent.as_deref(),
            PaneTarget::Shell(index) => self.shells.get(index).map(|pane| pane.terminal.as_ref()),
        }
    }

    fn selected_text(&self) -> Option<String> {
        let selection = self.selection?;
        let text = self
            .alternate_selection
            .as_ref()
            .filter(|capture| capture.pane == selection.pane)
            .map(|capture| capture.selected_text(selection.start, selection.end))
            .or_else(|| {
                self.terminal(selection.pane)
                    .map(|terminal| terminal.selected_text(selection.start, selection.end))
            })?;
        (!text.is_empty()).then_some(text)
    }

    fn selected_rows(&self, selection: TerminalSelection) -> Vec<(TerminalCell, String)> {
        self.alternate_selection
            .as_ref()
            .filter(|capture| capture.pane == selection.pane)
            .map(|capture| capture.selected_rows(selection.start, selection.end))
            .or_else(|| {
                self.terminal(selection.pane)
                    .map(|terminal| terminal.selected_rows(selection.start, selection.end))
            })
            .unwrap_or_default()
    }

    /// Opens a workspace attach, handing back the state its caller steps one frame at a time.
    ///
    /// This used to be one `attach_workspace` loop that ran until the user left the workspace,
    /// with every field below as a local. That shape forced whoever called it to keep the
    /// borrow -- in practice the shared `App` lock -- for as long as a session was open, which
    /// is exactly the state the embedded web server exists to report on. Keeping the state out
    /// here instead lets the caller re-acquire the lock per frame and drop it in between.
    fn begin_workspace(
        handle: SessionHandle,
        focus: WorkspaceFocus,
        bindings: WorkspaceBindings,
        lease: WorkspaceLease,
        chrome: WorkspaceChrome,
    ) -> io::Result<WorkspaceSession> {
        let owned = Arc::clone(&handle);
        let self_ = &mut *owned.lock().unwrap();
        let size = normalized_size(crossterm::terminal::size().unwrap_or((120, 40)));
        let mut stdout = io::stdout().lock();
        let mut keyboard_enhancement_enabled = false;
        stdout.write_all(ENABLE_MOUSE_REPORTING)?;
        sync_keyboard_enhancement(&mut stdout, &mut keyboard_enhancement_enabled, focus)?;
        stdout.write_all(b"\x1b[2J\x1b[H")?;
        stdout.flush()?;
        let mut layout =
            WorkspaceLayout::new(size.0, size.1, self_.shells.len(), self_.selected_shell);
        layout.apply_options(self_.maximized, self_.shell_height_adjust);
        Ok(WorkspaceSession {
            handle,
            lease,
            stdout,
            pending_input: None,
            size,
            keyboard_enhancement_enabled,
            exit: WorkspaceExit::Dashboard,
            focus,
            render_bindings: bindings.clone(),
            input_router: WorkspaceInputRouter {
                pending: Vec::new(),
                bindings,
            },
            last_signature: Vec::new(),
            last_layout_key: None,
            clear_next_frame: true,
            search: None,
            help_open: false,
            chrome,
            layout,
        })
    }

    /// The half of a frame that reacts to the keyboard and the mouse.
    fn apply_workspace_input(
        &mut self,
        state: &mut WorkspaceSession,
        session: &Session,
        input: PolledTerminalInput,
        outcome: &mut WorkspaceInputOutcome,
    ) -> io::Result<Option<WorkspaceExit>> {
        let input = match input {
            PolledTerminalInput::Pending => {
                if let Some(bytes) = state.input_router.flush() {
                    self.write_focused(state.focus, &bytes)?;
                }
                return Ok(None);
            }
            PolledTerminalInput::EndOfFile => return Ok(Some(state.exit)),
            PolledTerminalInput::Bytes(input) => input,
        };
        if state.help_open {
            if input == b"\x1b"
                || state.render_bindings.command(&input) == Some(WorkspaceCommand::Help)
            {
                state.help_open = false;
            }
            state.last_signature.clear();
            return Ok(None);
        }
        if let Some(active_search) = state.search.as_mut() {
            let (search_input, changed) = apply_workspace_search_input(active_search, &input);
            match search_input {
                WorkspaceSearchInput::Cancel => {
                    outcome.search = Some(WorkspaceSearchUpdate::Cancel {
                        query: active_search.original_query.clone(),
                        selected_session_key: active_search.original_selected_session_key.clone(),
                    });
                    state.exit = WorkspaceExit::RefreshSessions;
                    return Ok(Some(state.exit));
                }
                WorkspaceSearchInput::Commit => {
                    if changed {
                        outcome.search =
                            Some(WorkspaceSearchUpdate::Preview(active_search.value.clone()));
                    }
                    state.search = None;
                }
                WorkspaceSearchInput::Editing if changed => {
                    outcome.search =
                        Some(WorkspaceSearchUpdate::Preview(active_search.value.clone()));
                }
                WorkspaceSearchInput::Editing => {}
            }
            state.last_signature.clear();
            return Ok(None);
        }
        for routed in state.input_router.route(&input, state.focus) {
            if let WorkspaceInput::Mouse(event) = routed {
                self.handle_mouse(&state.layout, event)?;
                state.last_signature.clear();
                continue;
            }
            let WorkspaceInput::Command(command) = routed else {
                if let WorkspaceInput::Forward(bytes) = routed {
                    if state.focus == WorkspaceFocus::Sessions {
                        match session_list_input(&bytes) {
                            Some(SessionListInput::Previous) => {
                                state.exit = WorkspaceExit::PreviousSession(state.focus);
                                return Ok(Some(state.exit));
                            }
                            Some(SessionListInput::Next) => {
                                state.exit = WorkspaceExit::NextSession(state.focus);
                                return Ok(Some(state.exit));
                            }
                            Some(SessionListInput::Activate) => {
                                state.exit = WorkspaceExit::ActivateSession;
                                return Ok(Some(state.exit));
                            }
                            Some(SessionListInput::NewSession) => {
                                state.exit = WorkspaceExit::NewSession;
                                return Ok(Some(state.exit));
                            }
                            Some(SessionListInput::OpenShell) => {
                                state.exit = WorkspaceExit::OpenShell;
                                return Ok(Some(state.exit));
                            }
                            Some(SessionListInput::ToggleArchive) => {
                                state.exit = WorkspaceExit::ToggleArchive;
                                return Ok(Some(state.exit));
                            }
                            None => {}
                        }
                        continue;
                    }
                    if self
                        .focused_terminal(state.focus)
                        .is_some_and(|terminal| terminal.scrollback_offset() > 0)
                    {
                        self.focused_terminal(state.focus)
                            .unwrap()
                            .scroll_to_live_tail();
                    }
                    self.write_focused(state.focus, &bytes)?;
                }
                continue;
            };
            match command {
                WorkspaceCommand::Dashboard => return Ok(Some(state.exit)),
                WorkspaceCommand::Alert => {
                    state.exit = WorkspaceExit::Alert;
                    return Ok(Some(state.exit));
                }
                WorkspaceCommand::Search => {
                    state.search = Some(WorkspaceSearch {
                        value: state.chrome.search_query.clone(),
                        original_query: state.chrome.search_query.clone(),
                        original_selected_session_key: state.chrome.selected_session_key.clone(),
                    });
                }
                WorkspaceCommand::Help => {
                    state.help_open = true;
                }
                WorkspaceCommand::PreviousSession => {
                    state.exit = WorkspaceExit::PreviousSession(state.focus);
                    return Ok(Some(state.exit));
                }
                WorkspaceCommand::NextSession => {
                    state.exit = WorkspaceExit::NextSession(state.focus);
                    return Ok(Some(state.exit));
                }
                WorkspaceCommand::SelectShell(index) => {
                    if index < self.shells.len() {
                        self.selected_shell = index;
                        state.focus = WorkspaceFocus::Shell;
                        self.notice = Some(format!("selected {}", self.shells[index].name));
                        if self.maximized.is_some() {
                            self.maximized = Some(PaneTarget::Shell(index));
                        }
                    } else {
                        self.notice = Some(format!("shell {} does not exist", index + 1));
                    }
                }
                WorkspaceCommand::ToggleMaximize => {
                    if self.shells.is_empty() {
                        self.notice = Some("no shell is available to maximize".into());
                        continue;
                    }
                    self.maximized = Some(PaneTarget::Shell(self.selected_shell));
                    self.notice = Some(format!(
                        "shell maximized · {} returns to sessions",
                        state.render_bindings.label("focus")
                    ));
                    state.exit = WorkspaceExit::FocusShell;
                    return Ok(Some(state.exit));
                }
                WorkspaceCommand::ToggleShellArea => {
                    self.maximized = Some(PaneTarget::Agent);
                    self.notice = Some(format!(
                        "agent maximized · {} changes focus",
                        state.render_bindings.label("focus")
                    ));
                    state.exit = WorkspaceExit::ActivateSession;
                    return Ok(Some(state.exit));
                }
                WorkspaceCommand::GrowShell => {
                    if self.maximized.is_none() && !self.shells.is_empty() {
                        self.shell_height_adjust =
                            self.shell_height_adjust.saturating_add(2).min(20);
                        self.notice = Some("shell area enlarged".into());
                        state.clear_next_frame = true;
                    }
                }
                WorkspaceCommand::ShrinkShell => {
                    if self.maximized.is_none() && !self.shells.is_empty() {
                        self.shell_height_adjust =
                            self.shell_height_adjust.saturating_sub(2).max(-10);
                        self.notice = Some("shell area reduced".into());
                        state.clear_next_frame = true;
                    }
                }
                WorkspaceCommand::CopyCommandBlock => {
                    self.notice = self
                        .shells
                        .get(self.selected_shell)
                        .map(ShellPane::command_capture)
                        .filter(|capture| !capture.trim().is_empty())
                        .map_or_else(
                            || "current shell has no command block".into(),
                            |capture| match clipboard::copy(&capture) {
                                Ok(()) => "command block copied".into(),
                                Err(error) => format!("copy failed: {error}"),
                            },
                        )
                        .into();
                }
                WorkspaceCommand::ToggleFocus => {
                    if state.focus == WorkspaceFocus::Sessions {
                        state.exit = WorkspaceExit::ActivateSession;
                        return Ok(Some(state.exit));
                    }
                    state.focus = self.toggle_workspace_focus(session, state.focus)?;
                    if self.maximized.is_some() {
                        self.maximized = match state.focus {
                            WorkspaceFocus::Sessions => None,
                            WorkspaceFocus::Agent => Some(PaneTarget::Agent),
                            WorkspaceFocus::Shell => Some(PaneTarget::Shell(self.selected_shell)),
                        };
                    }
                }
                WorkspaceCommand::NewShell => {
                    if state.focus == WorkspaceFocus::Sessions {
                        state.exit = WorkspaceExit::OpenShell;
                        return Ok(Some(state.exit));
                    }
                    let name = self.next_shell_name();
                    let (id, terminal) = self.spawn_shell(session, (80, 12))?;
                    self.shells.push(ShellPane::new(id, terminal, name));
                    self.selected_shell = self.shells.len() - 1;
                    state.focus = WorkspaceFocus::Shell;
                    if self.maximized.is_some() {
                        self.maximized = Some(PaneTarget::Shell(self.selected_shell));
                    }
                }
                WorkspaceCommand::PreviousShell => {
                    if !self.shells.is_empty() {
                        self.selected_shell = self
                            .selected_shell
                            .checked_sub(1)
                            .unwrap_or(self.shells.len() - 1);
                        state.focus = WorkspaceFocus::Shell;
                        if self.maximized.is_some() {
                            self.maximized = Some(PaneTarget::Shell(self.selected_shell));
                        }
                    } else {
                        self.notice = Some(format!(
                            "no shell is open; press {} to create one",
                            state.render_bindings.label("new_shell")
                        ));
                    }
                }
                WorkspaceCommand::NextShell => {
                    if !self.shells.is_empty() {
                        self.selected_shell = (self.selected_shell + 1) % self.shells.len();
                        state.focus = WorkspaceFocus::Shell;
                        if self.maximized.is_some() {
                            self.maximized = Some(PaneTarget::Shell(self.selected_shell));
                        }
                    } else {
                        self.notice = Some(format!(
                            "no shell is open; press {} to create one",
                            state.render_bindings.label("new_shell")
                        ));
                    }
                }
                WorkspaceCommand::CloseShell => {
                    if self.shells.is_empty() {
                        self.notice = Some("no shell to close".into());
                        continue;
                    }
                    match shell_close_action(state.focus) {
                        ShellCloseAction::Ignore => {
                            self.notice = Some("focus a shell before closing it".into());
                        }
                        ShellCloseAction::Close => {
                            self.shells.remove(self.selected_shell).terminal.terminate();
                            if self.shells.is_empty() {
                                self.selected_shell = 0;
                                state.focus = WorkspaceFocus::Agent;
                                self.maximized = None;
                            } else {
                                self.selected_shell =
                                    self.selected_shell.min(self.shells.len() - 1);
                                if self.maximized.is_some() {
                                    self.maximized = Some(PaneTarget::Shell(self.selected_shell));
                                }
                            }
                        }
                    }
                }
                WorkspaceCommand::ScrollUp => {
                    if let Some(terminal) = self.focused_terminal(state.focus) {
                        terminal.scroll_viewport(focused_viewport_height(
                            &state.layout,
                            self,
                            state.focus,
                        ) as isize);
                    }
                }
                WorkspaceCommand::ScrollDown => {
                    if let Some(terminal) = self.focused_terminal(state.focus) {
                        terminal.scroll_viewport(
                            -(focused_viewport_height(&state.layout, self, state.focus) as isize),
                        );
                    }
                }
                WorkspaceCommand::LiveTail => {
                    if let Some(terminal) = self.focused_terminal(state.focus) {
                        terminal.scroll_to_live_tail();
                    }
                }
            }
        }
        state.last_signature.clear();
        Ok(None)
    }

    /// The half of a frame that repaints, and the only place a workspace decides on its own
    /// that it is over -- when the agent it was showing has gone.
    fn render_workspace(
        &mut self,
        state: &mut WorkspaceSession,
        chrome: WorkspaceChrome,
    ) -> io::Result<Option<WorkspaceExit>> {
        if state.focus == WorkspaceFocus::Agent {
            match self.agent.as_ref() {
                Some(agent) if agent.is_alive() => {}
                Some(agent) => {
                    self.notice = Some(agent.exit_description().map_or_else(
                        || "agent exited; showing the latest session preview".into(),
                        |exit| format!("agent exited ({exit}); showing the latest session preview"),
                    ));
                    state.focus = WorkspaceFocus::Sessions;
                    state.last_signature.clear();
                }
                None => {
                    state.exit = WorkspaceExit::ActivateSession;
                    return Ok(Some(state.exit));
                }
            }
        }
        sync_keyboard_enhancement(
            &mut state.stdout,
            &mut state.keyboard_enhancement_enabled,
            state.focus,
        )?;
        if chrome != state.chrome {
            state.chrome = chrome;
            state.last_signature.clear();
        }
        if self.shells.is_empty() {
            self.selected_shell = 0;
            if state.focus == WorkspaceFocus::Shell {
                state.focus = if self.agent.is_some() {
                    WorkspaceFocus::Agent
                } else {
                    WorkspaceFocus::Sessions
                };
            }
        } else {
            self.selected_shell = self.selected_shell.min(self.shells.len() - 1);
        }

        let new_size = normalized_size(crossterm::terminal::size().unwrap_or(state.size));
        if new_size != state.size {
            state.size = new_size;
            state.last_signature.clear();
            state.clear_next_frame = true;
        }
        let mut layout = WorkspaceLayout::new(
            state.size.0,
            state.size.1,
            self.shells.len(),
            self.selected_shell,
        );
        layout.apply_options(self.maximized, self.shell_height_adjust);
        state.layout = layout;
        let layout_key = (
            state.size,
            self.shells.len(),
            self.selected_shell,
            self.maximized,
            self.shell_height_adjust,
        );
        if state.last_layout_key != Some(layout_key) {
            self.resize_workspace(&state.layout)?;
            state.last_layout_key = Some(layout_key);
            state.last_signature.clear();
            state.clear_next_frame = true;
        }
        let alternate_changed = self.refresh_alternate_selection()?;
        let selection_copied = self.finish_pending_alternate_copy();
        if alternate_changed || selection_copied {
            state.last_signature.clear();
        }
        let signature = self.render_signature(state.size, &state.layout, state.focus);
        if signature != state.last_signature {
            render_workspace_with_bindings(
                &mut state.stdout,
                self,
                &state.chrome,
                &state.layout,
                WorkspaceRenderState {
                    focus: state.focus,
                    search: state
                        .search
                        .as_ref()
                        .map(|search: &WorkspaceSearch| search.value.as_str()),
                    help: state.help_open,
                },
                &state.render_bindings,
                state.clear_next_frame,
            )?;
            state.last_signature = signature;
            state.clear_next_frame = false;
        }
        Ok(None)
    }

    /// This attach's name in each terminal's viewer registry.
    ///
    /// One per process rather than one per pane: the registry is per terminal already, and a
    /// dashboard only ever has one workspace open, so the process's lease instance is enough
    /// to tell this window apart from a browser's -- and stable across every relayout, which
    /// is what makes a resize update this viewer instead of adding another.
    fn workspace_viewer(&self) -> String {
        format!("{}:tui:{}", std::process::id(), self.lease_owner_id)
    }

    /// Sizes the session's terminals for this workspace's panes.
    ///
    /// Reported as a viewer rather than imposed: a browser may be looking at the same PTY,
    /// and the size it ends up at is the smallest of the two. A pane with room left over
    /// letterboxes -- `render_terminal` clears each row to the pane's width and writes the
    /// screen's shorter row into it, so the spare columns stay blank rather than being filled
    /// with reflowed output.
    fn resize_workspace(&mut self, layout: &WorkspaceLayout) -> io::Result<()> {
        self.selection = None;
        self.alternate_selection = None;
        self.pending_alternate_copy = None;
        self.pending_agent_click = None;
        let viewer = self.workspace_viewer();
        if let Some(agent) = &self.agent {
            agent.resize_viewer(&viewer, layout.agent.width, layout.agent.height)?;
        }
        for (index, rect) in &layout.shell_panes {
            self.shells[*index].terminal.resize_viewer(
                &viewer,
                rect.width,
                rect.height.saturating_sub(1),
            )?;
        }
        Ok(())
    }

    /// Stops this workspace counting as a viewer, so a terminal it was keeping small grows
    /// back to whatever browser is still looking at it.
    fn release_workspace_viewer(&self) {
        let viewer = self.workspace_viewer();
        if let Some(agent) = &self.agent {
            let _ = agent.detach_viewer(&viewer);
        }
        for shell in &self.shells {
            let _ = shell.terminal.detach_viewer(&viewer);
        }
    }

    fn write_focused(&mut self, focus: WorkspaceFocus, bytes: &[u8]) -> io::Result<()> {
        match focus {
            WorkspaceFocus::Sessions => {}
            WorkspaceFocus::Agent => {
                if let Some(agent) = &self.agent {
                    agent.write(bytes)?;
                }
            }
            WorkspaceFocus::Shell => {
                if let Some(shell) = self.shells.get_mut(self.selected_shell) {
                    if bytes.iter().any(|byte| matches!(byte, b'\r' | b'\n')) {
                        shell.mark_command_start();
                    }
                    shell.terminal.write(bytes)?;
                }
            }
        }
        Ok(())
    }

    fn focused_terminal(&self, focus: WorkspaceFocus) -> Option<&ManagedTerminal> {
        match focus {
            WorkspaceFocus::Sessions => None,
            WorkspaceFocus::Agent => self.agent.as_deref(),
            WorkspaceFocus::Shell => self
                .shells
                .get(self.selected_shell)
                .map(|pane| pane.terminal.as_ref()),
        }
    }

    fn render_signature(
        &self,
        size: (u16, u16),
        layout: &WorkspaceLayout,
        focus: WorkspaceFocus,
    ) -> Vec<u64> {
        let mut signature = vec![
            u64::from(size.0),
            u64::from(size.1),
            self.shells.len() as u64,
            self.selected_shell as u64,
            match focus {
                WorkspaceFocus::Sessions => 0,
                WorkspaceFocus::Agent => 1,
                WorkspaceFocus::Shell => 2,
            },
            self.agent
                .as_ref()
                .map_or(0, |agent| agent.output_generation()),
        ];
        signature.extend(
            layout
                .shell_panes
                .iter()
                .map(|(index, _)| self.shells[*index].terminal.output_generation()),
        );
        signature
    }
}

fn spawn_shell(session: &Session, size: (u16, u16)) -> io::Result<ManagedTerminal> {
    ManagedTerminal::spawn(&shell_command(&session.cwd), size)
}

/// The part of a daemon terminal id that names one shell: the `<id>` of
/// `shell|<session key>|<id>`. Session keys themselves contain `|`, so this splits from the
/// right, the same way `terminal_session_key` does.
fn shell_id_of(terminal_id: &str) -> Option<&str> {
    terminal_id
        .strip_prefix("shell|")
        .and_then(|rest| rest.rsplit_once('|'))
        .map(|(_, id)| id)
}

/// Connects to every shell the daemon is holding for this session that `terminals` has not
/// adopted yet. Shell terminals live in the daemon, not in any one process, so this is what
/// makes a shell opened in the TUI and one opened in a browser the same terminal.
fn adopt_daemon_shells(
    terminals: &mut SessionTerminals,
    socket: &Path,
    session_key: &str,
    owner_id: &str,
    size: (u16, u16),
) -> io::Result<()> {
    let DaemonResponse::List(ids) = daemon_request(
        socket,
        &DaemonRequest::List {
            prefix: format!("shell|{session_key}|"),
        },
    )?
    else {
        return Ok(());
    };
    for id in ids {
        let Some(shell_id) = shell_id_of(&id) else {
            continue;
        };
        if terminals.shells.iter().any(|shell| shell.id == shell_id) {
            continue;
        }
        let shell_id = shell_id.to_owned();
        let name = terminals.next_shell_name();
        terminals.shells.push(ShellPane::new(
            shell_id,
            ManagedTerminal::connect_remote(socket.to_path_buf(), id, owner_id.to_owned(), size)?,
            name,
        ));
    }
    Ok(())
}

fn shell_command(cwd: &Path) -> CommandSpec {
    #[cfg(unix)]
    {
        let shell = env::var_os("SHELL").unwrap_or_else(|| OsString::from("/bin/sh"));
        CommandSpec::new(shell, cwd).arg("-l")
    }
    #[cfg(not(unix))]
    {
        let shell = env::var_os("COMSPEC").unwrap_or_else(|| OsString::from("cmd.exe"));
        CommandSpec::new(shell, cwd)
    }
}

#[cfg(test)]
fn render_workspace(
    stdout: &mut impl Write,
    terminals: &SessionTerminals,
    chrome: &WorkspaceChrome,
    layout: &WorkspaceLayout,
    focus: WorkspaceFocus,
    clear: bool,
) -> io::Result<()> {
    render_workspace_with_bindings(
        stdout,
        terminals,
        chrome,
        layout,
        WorkspaceRenderState {
            focus,
            search: None,
            help: false,
        },
        &WorkspaceBindings::from_config(&AgentConsoleConfig::default()),
        clear,
    )
}

fn render_workspace_with_bindings(
    stdout: &mut impl Write,
    terminals: &SessionTerminals,
    chrome: &WorkspaceChrome,
    layout: &WorkspaceLayout,
    state: WorkspaceRenderState<'_>,
    bindings: &WorkspaceBindings,
    clear: bool,
) -> io::Result<()> {
    stdout.sync_update(|stdout| {
        render_workspace_frame(stdout, terminals, chrome, layout, state, bindings, clear)
    })?
}

fn render_workspace_frame(
    stdout: &mut impl Write,
    terminals: &SessionTerminals,
    chrome: &WorkspaceChrome,
    layout: &WorkspaceLayout,
    state: WorkspaceRenderState<'_>,
    bindings: &WorkspaceBindings,
    clear: bool,
) -> io::Result<()> {
    let WorkspaceRenderState {
        focus,
        search,
        help,
    } = state;
    if clear {
        stdout.write_all(b"\x1b[0m\x1b[?25l\x1b[H\x1b[2J")?;
    } else {
        stdout.write_all(b"\x1b[0m\x1b[?25l\x1b[H")?;
    }
    render_sidebar(stdout, chrome, layout, focus == WorkspaceFocus::Sessions)?;
    if !matches!(terminals.maximized, Some(PaneTarget::Shell(_))) {
        let selected_label = chrome
            .sessions
            .get(chrome.selected)
            .cloned()
            .unwrap_or_else(|| "session".into());
        let show_live_agent = focus != WorkspaceFocus::Sessions
            || terminals.maximized == Some(PaneTarget::Agent) && terminals.agent.is_some();
        let agent_label = if show_live_agent {
            pane_label_with_scrollback(
                &format!("AGENT · {selected_label}"),
                terminals.agent.as_deref(),
            )
        } else {
            format!("SESSION PREVIEW · {selected_label}")
        };
        render_pane_title(
            stdout,
            0,
            layout.agent.left,
            layout.agent.width,
            &agent_label,
            focus == WorkspaceFocus::Agent,
        )?;
        if show_live_agent {
            if let Some(agent) = &terminals.agent {
                render_terminal(stdout, agent, layout.agent)?;
                if let Some(selection) = terminals
                    .selection
                    .filter(|selection| selection.pane == PaneTarget::Agent)
                {
                    render_selection(stdout, terminals.selected_rows(selection), layout.agent)?;
                }
            }
        } else {
            render_session_preview(stdout, &chrome.preview, layout.agent)?;
        }
    }
    if let Some(row) = layout.shell_divider_row {
        write_at(
            stdout,
            row,
            layout.agent.left,
            format!(
                "\x1b[2m{}\x1b[0m",
                "─".repeat(usize::from(layout.agent.width))
            )
            .as_bytes(),
        )?;
        write_at(
            stdout,
            row,
            layout.agent.left + 1,
            b"\x1b[1m SHELLS \x1b[0m",
        )?;
    }

    for (position, (index, rect)) in layout.shell_panes.iter().enumerate() {
        let selected = *index == terminals.selected_shell;
        render_pane_title(
            stdout,
            rect.top,
            rect.left,
            rect.width,
            &pane_label_with_scrollback(
                &terminals.shell_label(*index),
                terminals
                    .shells
                    .get(*index)
                    .map(|pane| pane.terminal.as_ref()),
            ),
            focus == WorkspaceFocus::Shell && selected,
        )?;
        if let Some(shell) = terminals.shells.get(*index) {
            let terminal_rect = PaneRect {
                top: rect.top + 1,
                left: rect.left,
                width: rect.width,
                height: rect.height.saturating_sub(1),
            };
            render_terminal(stdout, &shell.terminal, terminal_rect)?;
            if let Some(selection) = terminals
                .selection
                .filter(|selection| selection.pane == PaneTarget::Shell(*index))
            {
                render_selection(stdout, terminals.selected_rows(selection), terminal_rect)?;
            }
        }
        if position + 1 < layout.shell_panes.len() {
            render_vertical_line(stdout, rect.left + rect.width, rect.top, rect.height)?;
        }
    }
    if let Some(list) = layout.shell_list {
        render_vertical_line(stdout, list.left - 1, list.top, list.height)?;
        write_at(stdout, list.top, list.left, b"\x1b[1m SHELLS \x1b[0m")?;
        let visible = usize::from(list.height.saturating_sub(1));
        let first = terminals
            .selected_shell
            .saturating_add(1)
            .saturating_sub(visible);
        for (row, (index, _)) in terminals
            .shells
            .iter()
            .enumerate()
            .skip(first)
            .take(visible)
            .enumerate()
        {
            let label = fit_text(&format!("  {}", terminals.shell_label(index)), list.width);
            let line = if index == terminals.selected_shell {
                format!("\x1b[7m{label}\x1b[0m")
            } else {
                label
            };
            write_at(
                stdout,
                list.top + 1 + row as u16,
                list.left,
                line.as_bytes(),
            )?;
        }
    }

    if help {
        render_workspace_help(stdout, layout, bindings)?;
    }

    let (badge, controls_text) = if help {
        (
            " WORKSPACE HELP ".to_owned(),
            format!("  {} or Esc close", bindings.label("help")),
        )
    } else if let Some(query) = search {
        (
            " SEARCH SESSIONS ".to_owned(),
            format!(
                "  {} {query}█  ·  Enter keep  Esc cancel  Backspace edit",
                bindings.label("search")
            ),
        )
    } else {
        let search_label = if chrome.search_query.is_empty() {
            format!("{} search", bindings.label("search"))
        } else {
            format!(
                "{} search={}",
                bindings.label("search"),
                chrome.search_query
            )
        };
        let focus_name = match focus {
            WorkspaceFocus::Sessions => "FOCUS SESSIONS".to_owned(),
            WorkspaceFocus::Agent => "FOCUS AGENT".to_owned(),
            WorkspaceFocus::Shell => format!(
                "FOCUS SHELL {}/{}",
                terminals.selected_shell.saturating_add(1),
                terminals.shells.len()
            ),
        };
        let badge = format!(" {focus_name} ");
        let shortcuts = match focus {
            WorkspaceFocus::Sessions => format!(
                "{} dashboard  {} focus  {search_label}  {} alert  {} help  ↑↓/j/k select  Enter agent  {} agent  {} shell  n new  s +shell  x archive",
                bindings.label("dashboard"),
                bindings.label("focus"),
                bindings.label("session_alert"),
                bindings.label("help"),
                bindings.label("hide_shells"),
                bindings.label("maximize")
            ),
            WorkspaceFocus::Agent => format!(
                "{} dashboard  {} focus  {} new shell  ·  keys pass through  ·  Shift-PageUp/Down scroll",
                bindings.label("dashboard"),
                bindings.label("focus"),
                bindings.label("new_shell")
            ),
            WorkspaceFocus::Shell => format!(
                "{} dashboard  {} focus  {} new  {} next  {} close  ·  Shift-PageUp/Down scroll",
                bindings.label("dashboard"),
                bindings.label("focus"),
                bindings.label("new_shell"),
                bindings.label("next_shell"),
                bindings.label("close_shell")
            ),
        };
        let controls_text = terminals.notice.as_deref().map_or_else(
            || format!("  {shortcuts}"),
            |notice| {
                let essentials = match focus {
                    WorkspaceFocus::Sessions => format!(
                        "{} dashboard  {} focus  {} agent  {} shell  n new  s +shell  x archive",
                        bindings.label("dashboard"),
                        bindings.label("focus"),
                        bindings.label("hide_shells"),
                        bindings.label("maximize")
                    ),
                    WorkspaceFocus::Agent => format!(
                        "{} dashboard  {} focus  {} new shell",
                        bindings.label("dashboard"),
                        bindings.label("focus"),
                        bindings.label("new_shell")
                    ),
                    WorkspaceFocus::Shell => format!(
                        "{} dashboard  {} focus  {} new  {} next  {} close",
                        bindings.label("dashboard"),
                        bindings.label("focus"),
                        bindings.label("new_shell"),
                        bindings.label("next_shell"),
                        bindings.label("close_shell")
                    ),
                };
                format!("  {essentials}  ·  {notice}")
            },
        );
        (badge, controls_text)
    };
    let controls = fit_text(
        &controls_text,
        layout
            .agent
            .left
            .saturating_add(layout.agent.width)
            .saturating_sub(badge.chars().count() as u16),
    );
    write_at(
        stdout,
        layout.status_row,
        0,
        format!("\x1b[30;46;1m{badge}\x1b[0m\x1b[2m{controls}\x1b[0m").as_bytes(),
    )?;
    let notification_text = chrome
        .notification
        .as_ref()
        .map_or_else(String::new, |notification| {
            let alert = match (bindings.label_opt("alert"), focus) {
                (Some(direct), WorkspaceFocus::Sessions) => {
                    format!("{direct}/{}", bindings.label("session_alert"))
                }
                (Some(direct), _) => direct.to_owned(),
                (None, WorkspaceFocus::Sessions) => bindings.label("session_alert").to_owned(),
                (None, _) => format!(
                    "{} then {}",
                    bindings.label("focus"),
                    bindings.label("session_alert")
                ),
            };
            format!(
                " ALERT · {notification}  ·  {alert} jump  {} dashboard",
                bindings.label("dashboard")
            )
        });
    let footer_width = layout.agent.left.saturating_add(layout.agent.width);
    write_at(
        stdout,
        layout.notification_row,
        0,
        format!(
            "\x1b[33m{}\x1b[0m",
            fit_text(&notification_text, footer_width)
        )
        .as_bytes(),
    )?;
    position_workspace_cursor(stdout, terminals, layout, focus)?;
    Ok(())
}

fn render_workspace_help(
    stdout: &mut impl Write,
    layout: &WorkspaceLayout,
    bindings: &WorkspaceBindings,
) -> io::Result<()> {
    let left = layout.agent.left;
    let width = layout.agent.width;
    for row in 0..layout.status_row {
        write_at(stdout, row, left, fit_text("", width).as_bytes())?;
    }
    write_at(
        stdout,
        0,
        left,
        format!(
            "\x1b[30;46;1m{}\x1b[0m",
            fit_text(" WORKSPACE KEY BINDINGS ", width)
        )
        .as_bytes(),
    )?;
    for (offset, line) in workspace_help_lines(bindings)
        .into_iter()
        .take(usize::from(layout.status_row.saturating_sub(2)))
        .enumerate()
    {
        let style = if line.starts_with("WORKSPACE ·") {
            "\x1b[1;36m"
        } else {
            "\x1b[0m"
        };
        write_at(
            stdout,
            offset as u16 + 2,
            left + 1,
            format!("{style}{}\x1b[0m", fit_text(&line, width.saturating_sub(2))).as_bytes(),
        )?;
    }
    Ok(())
}

fn workspace_help_lines(bindings: &WorkspaceBindings) -> Vec<String> {
    let direct_alert = bindings
        .label_opt("alert")
        .map(|label| format!("{:<24} {}", "next unread alert", label));
    let live_tail = bindings.label_opt("live_tail").map_or_else(
        || format!("{:<24} {}", "return to live tail", "any key"),
        |label| format!("{:<24} {label}", "return to live tail"),
    );
    let mut lines = vec![
        "WORKSPACE · DIRECT".into(),
        format!("{:<24} {}", "cycle focus", bindings.label("focus")),
        format!("{:<24} {}", "new shell", bindings.label("new_shell")),
        format!("{:<24} {}", "next shell", bindings.label("next_shell")),
        format!("{:<24} {}", "close shell", bindings.label("close_shell")),
        format!("{:<24} {}", "dashboard", bindings.label("dashboard")),
    ];
    lines.extend(direct_alert);
    lines.extend([
        String::new(),
        "WORKSPACE · SESSIONS".into(),
        format!("{:<24} {}", "select session", "↑/↓, J/K"),
        format!("{:<24} {}", "open agent", "Enter"),
        format!("{:<24} {}", "search sessions", bindings.label("search")),
        format!(
            "{:<24} {}",
            "next unread alert",
            bindings.label("session_alert")
        ),
        format!("{:<24} {}", "new session / shell", "N / S"),
        format!("{:<24} {}", "archive / restore", "X"),
        format!("{:<24} {}", "focus agent", bindings.label("hide_shells")),
        format!("{:<24} {}", "focus last shell", bindings.label("maximize")),
        format!(
            "{:<24} {} / {}",
            "resize shell area",
            bindings.label("grow_shell"),
            bindings.label("shrink_shell")
        ),
        format!(
            "{:<24} {}",
            "copy command output",
            bindings.label("copy_command")
        ),
        format!(
            "{:<24} {}…9",
            "focus numbered shell",
            bindings.label("select_shell_1")
        ),
        String::new(),
        "WORKSPACE · CHILD VIEWPORT".into(),
        format!(
            "{:<24} {} / {}",
            "scroll viewport",
            bindings.label("scroll_up"),
            bindings.label("scroll_down")
        ),
        live_tail,
        format!("{:<24} {} / Esc", "close help", bindings.label("help")),
    ]);
    lines
}

fn pane_label_with_scrollback(label: &str, terminal: Option<&ManagedTerminal>) -> String {
    let offset = terminal.map_or(0, ManagedTerminal::scrollback_offset);
    if offset == 0 {
        label.to_owned()
    } else {
        format!("{label} · SCROLL +{offset}")
    }
}

fn render_sidebar(
    stdout: &mut impl Write,
    chrome: &WorkspaceChrome,
    layout: &WorkspaceLayout,
    focused: bool,
) -> io::Result<()> {
    let title = fit_text(" SESSIONS", layout.sidebar_width);
    write_at(stdout, 0, 0, format!("\x1b[1;36m{title}\x1b[0m").as_bytes())?;
    render_sidebar_status_summary(stdout, chrome.status_counts, layout.sidebar_width)?;
    let list_top = 3;
    let blank = fit_text("", layout.sidebar_width);
    for row in list_top..layout.status_row {
        write_at(stdout, row, 0, blank.as_bytes())?;
    }
    let visible = usize::from(layout.status_row.saturating_sub(list_top));
    let first = chrome.selected.saturating_add(1).saturating_sub(visible);
    for (row, (index, session)) in chrome
        .sessions
        .iter()
        .enumerate()
        .skip(first)
        .take(visible)
        .enumerate()
    {
        if session.starts_with("▾ ") {
            let label = fit_text(session, layout.sidebar_width);
            write_at(
                stdout,
                row as u16 + list_top,
                0,
                format!("\x1b[1;36m{label}\x1b[0m").as_bytes(),
            )?;
            continue;
        }
        let marker = if index == chrome.selected { "▸" } else { " " };
        let label = fit_text(&format!("{marker} {session}"), layout.sidebar_width);
        let line = if index == chrome.selected && focused {
            format!("\x1b[30;46;1m{label}\x1b[0m")
        } else if index == chrome.selected {
            style_sidebar_provider(&label, "\x1b[48;2;45;53;72m")
        } else {
            style_sidebar_provider(&label, "")
        };
        write_at(stdout, row as u16 + list_top, 0, line.as_bytes())?;
    }
    render_vertical_line(stdout, layout.sidebar_width, 0, layout.status_row)
}

fn render_sidebar_status_summary(
    stdout: &mut impl Write,
    counts: (usize, usize, usize, usize),
    width: u16,
) -> io::Result<()> {
    let (working, waiting, idle, failed) = counts;
    let expanded = width >= 26;
    let working = format!("● {working} {}", if expanded { "working" } else { "work" });
    let waiting = format!("◐ {waiting} {}", if expanded { "waiting" } else { "wait" });
    let idle = format!("○ {idle} idle");
    let failed = format!("× {failed} {}", if expanded { "failed" } else { "fail" });
    render_sidebar_status_pair(
        stdout,
        1,
        width,
        (&working, "\x1b[32m"),
        (&waiting, "\x1b[33m"),
    )?;
    render_sidebar_status_pair(stdout, 2, width, (&idle, "\x1b[90m"), (&failed, "\x1b[31m"))
}

fn render_sidebar_status_pair(
    stdout: &mut impl Write,
    row: u16,
    width: u16,
    left: (&str, &str),
    right: (&str, &str),
) -> io::Result<()> {
    write_at(stdout, row, 0, fit_text("", width).as_bytes())?;
    let (left_text, left_style) = left;
    let (right_text, right_style) = right;
    let left_width = left_text.width().min(usize::from(width));
    let right_width = right_text.width().min(usize::from(width));
    let right_col = usize::from(width).saturating_sub(right_width);
    let left = fit_text(left_text, left_width as u16);
    write_at(
        stdout,
        row,
        0,
        format!("{left_style}{left}\x1b[0m").as_bytes(),
    )?;
    if right_col > left_width {
        let right = fit_text(right_text, right_width as u16);
        write_at(
            stdout,
            row,
            right_col as u16,
            format!("{right_style}{right}\x1b[0m").as_bytes(),
        )?;
    }
    Ok(())
}

fn style_sidebar_provider(label: &str, base_style: &str) -> String {
    let provider = [(" Cdx ", "\x1b[36m"), (" Cla ", "\x1b[38;2;219;126;82m")]
        .into_iter()
        .find_map(|(token, style)| label.find(token).map(|start| (start + 1, style)));
    let Some((start, provider_style)) = provider else {
        return if base_style.is_empty() {
            label.to_owned()
        } else {
            format!("{base_style}{label}\x1b[0m")
        };
    };
    let end = start + 3;
    format!(
        "{base_style}{}{provider_style}{}\x1b[0m{base_style}{}\x1b[0m",
        &label[..start],
        &label[start..end],
        &label[end..]
    )
}

fn render_session_preview(
    stdout: &mut impl Write,
    lines: &[String],
    rect: PaneRect,
) -> io::Result<()> {
    let clear_row = format!("\x1b[0m\x1b[{}X", rect.width);
    for offset in 0..rect.height {
        let row = rect.top + offset;
        write_at(stdout, row, rect.left, clear_row.as_bytes())?;
        let Some(line) = lines.get(usize::from(offset)) else {
            continue;
        };
        let line = fit_text(line, rect.width);
        let style = if offset == 0 {
            "\x1b[1;36m"
        } else if line.trim() == "RECENT TRANSCRIPT" {
            "\x1b[1;35m"
        } else {
            "\x1b[0m"
        };
        write_at(
            stdout,
            row,
            rect.left,
            format!("{style}{line}\x1b[0m").as_bytes(),
        )?;
    }
    Ok(())
}

fn render_terminal(
    stdout: &mut impl Write,
    terminal: &ManagedTerminal,
    rect: PaneRect,
) -> io::Result<()> {
    let view = terminal.screen_view();
    let clear_row = format!("\x1b[0m\x1b[{}X", rect.width);
    for offset in 0..rect.height {
        let row = rect.top + offset;
        write_at(stdout, row, rect.left, clear_row.as_bytes())?;
        if let Some(contents) = view.rows.get(usize::from(offset)) {
            write_at(stdout, row, rect.left, contents)?;
        }
    }
    Ok(())
}

fn render_selection(
    stdout: &mut impl Write,
    rows: Vec<(TerminalCell, String)>,
    rect: PaneRect,
) -> io::Result<()> {
    for (cell, text) in rows {
        if cell.row >= rect.height || cell.col >= rect.width {
            continue;
        }
        write_at(
            stdout,
            rect.top + cell.row,
            rect.left + cell.col,
            format!("\x1b[7m{text}\x1b[0m").as_bytes(),
        )?;
    }
    Ok(())
}

fn render_vertical_line(
    stdout: &mut impl Write,
    col: u16,
    top: u16,
    height: u16,
) -> io::Result<()> {
    for row in top..top.saturating_add(height) {
        write_at(stdout, row, col, b"\x1b[2m\xe2\x94\x82\x1b[0m")?;
    }
    Ok(())
}

fn position_workspace_cursor(
    stdout: &mut impl Write,
    terminals: &SessionTerminals,
    layout: &WorkspaceLayout,
    focus: WorkspaceFocus,
) -> io::Result<()> {
    let target = match focus {
        WorkspaceFocus::Sessions => None,
        WorkspaceFocus::Agent => terminals
            .agent
            .as_ref()
            .map(|terminal| (terminal, layout.agent)),
        WorkspaceFocus::Shell => layout
            .shell_panes
            .iter()
            .find(|(index, _)| *index == terminals.selected_shell)
            .and_then(|(_, rect)| {
                terminals.shells.get(terminals.selected_shell).map(|pane| {
                    (
                        &pane.terminal,
                        PaneRect {
                            top: rect.top + 1,
                            left: rect.left,
                            width: rect.width,
                            height: rect.height.saturating_sub(1),
                        },
                    )
                })
            }),
    };
    let Some((terminal, rect)) = target else {
        return stdout.write_all(b"\x1b[?25l");
    };
    let view = terminal.screen_view();
    if view.hide_cursor || rect.height == 0 || rect.width == 0 {
        return stdout.write_all(b"\x1b[?25l");
    }
    let row = rect.top + view.cursor.0.min(rect.height - 1);
    let col = rect.left + view.cursor.1.min(rect.width - 1);
    write_at(stdout, row, col, b"\x1b[?25h")
}

fn write_at(stdout: &mut impl Write, row: u16, col: u16, bytes: &[u8]) -> io::Result<()> {
    write!(stdout, "\x1b[{};{}H", row + 1, col + 1)?;
    stdout.write_all(bytes)
}

fn render_pane_title(
    stdout: &mut impl Write,
    row: u16,
    col: u16,
    width: u16,
    label: &str,
    focused: bool,
) -> io::Result<()> {
    let label = fit_text(&format!(" {label} "), width);
    let style = if focused {
        "\x1b[30;46;1m"
    } else {
        "\x1b[1;37;100m"
    };
    write_at(
        stdout,
        row,
        col,
        format!("{style}{label}\x1b[0m").as_bytes(),
    )
}

fn fit_text(value: &str, width: u16) -> String {
    let width = usize::from(width);
    let mut output = String::new();
    let mut columns = 0;
    for character in value.chars() {
        let character_width = character.width().unwrap_or(0);
        if columns + character_width > width {
            break;
        }
        output.push(character);
        columns += character_width;
    }
    if columns < width {
        output.push_str(&" ".repeat(width - columns));
    }
    output
}

#[derive(Default)]
pub struct TerminalManager {
    config: AgentConsoleConfig,
    /// One lock per session rather than one for the map's contents.
    ///
    /// A workspace frame is almost entirely terminal work -- polling the daemon for new
    /// output, feeding it through vt100, writing a screen to a terminal that may be applying
    /// back-pressure -- and under a busy agent that adds up to hundreds of milliseconds. None
    /// of it reads the session list, the notifications or anything else the `App` owns, so
    /// none of it should be holding the `App` mutex that the web server needs to answer at
    /// all. Handing the attach its own handle is what takes that work off that lock.
    ///
    /// Lock order is always `App` first, then a session: `TerminalManager`'s own methods are
    /// reached through the `App` and take a session lock inside it, while an attached
    /// workspace takes only the session lock. Nothing takes the `App` lock while holding a
    /// session's.
    terminals: HashMap<String, SessionHandle>,
    use_daemon: bool,
    daemon_socket: Option<PathBuf>,
    lease_owner: LeaseOwner,
}

impl TerminalManager {
    pub fn new(config: AgentConsoleConfig) -> Self {
        Self {
            config,
            terminals: HashMap::new(),
            use_daemon: cfg!(unix) && env::var("AGENT_CONSOLE_PTY_MODE").as_deref() != Ok("local"),
            daemon_socket: None,
            lease_owner: LeaseOwner::new(),
        }
    }

    #[cfg(test)]
    pub fn new_local(config: AgentConsoleConfig) -> Self {
        let mut manager = Self::new(config);
        manager.use_daemon = false;
        manager
    }

    fn ensure_daemon(&mut self, current_exe: &Path) -> io::Result<Option<PathBuf>> {
        if !self.use_daemon {
            return Ok(None);
        }
        if let Some(socket) = &self.daemon_socket
            && matches!(
                daemon_request(socket, &DaemonRequest::Ping),
                Ok(DaemonResponse::Ok)
            )
        {
            return Ok(Some(socket.clone()));
        }
        let state_dir = crate::store::state_dir().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "cannot resolve state directory")
        })?;
        ensure_private_dir(&state_dir)?;
        let socket = state_dir.join("pty-daemon.sock");
        check_socket_path(&socket)?;
        if !matches!(
            daemon_request(&socket, &DaemonRequest::Ping),
            Ok(DaemonResponse::Ok)
        ) {
            let _ = fs::remove_file(&socket);
            let mut command = ProcessCommand::new(current_exe);
            command
                .arg("pty-daemon")
                .arg(&socket)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            #[cfg(unix)]
            {
                // SAFETY: this callback runs in the child after fork and calls only the
                // async-signal-safe setsid syscall before exec.
                unsafe {
                    command.pre_exec(|| {
                        if libc::setsid() == -1 {
                            Err(io::Error::last_os_error())
                        } else {
                            Ok(())
                        }
                    });
                }
            }
            command.spawn()?;
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(3) {
                if matches!(
                    daemon_request(&socket, &DaemonRequest::Ping),
                    Ok(DaemonResponse::Ok)
                ) {
                    self.daemon_socket = Some(socket.clone());
                    return Ok(Some(socket));
                }
                thread::sleep(Duration::from_millis(25));
            }
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "PTY daemon did not become ready",
            ));
        }
        self.daemon_socket = Some(socket.clone());
        Ok(Some(socket))
    }

    /// A handle on a session's agent terminal.
    ///
    /// Owned rather than borrowed, so a caller can go on using it -- polling the daemon,
    /// writing a keystroke -- with every lock released. That is what keeps a websocket's
    /// round-trips off the locks the dashboard needs for a repaint.
    pub fn agent(&self, key: &str) -> Option<Arc<ManagedTerminal>> {
        self.terminals.get(key)?.lock().unwrap().agent.clone()
    }

    pub fn shell_capture(&self, key: &str) -> Option<String> {
        let terminals = self.terminals.get(key)?.lock().unwrap();
        terminals
            .shells
            .get(terminals.selected_shell)
            .map(ShellPane::command_capture)
    }

    pub fn shell_count(&self, key: &str) -> usize {
        self.terminals
            .get(key)
            .map_or(0, |terminals| terminals.lock().unwrap().shells.len())
    }

    /// Every shell this process has open for a session, in the order they are shown.
    pub fn shells(&self, key: &str) -> Vec<ShellInfo> {
        self.terminals.get(key).map_or_else(Vec::new, |terminals| {
            terminals
                .lock()
                .unwrap()
                .shells
                .iter()
                .map(ShellPane::info)
                .collect()
        })
    }

    /// One session's shell, for a caller that streams it with `ManagedTerminal::poll_raw`.
    pub fn shell(&self, key: &str, id: &str) -> Option<Arc<ManagedTerminal>> {
        self.terminals
            .get(key)?
            .lock()
            .unwrap()
            .shells
            .iter()
            .find(|shell| shell.id == id)
            .map(|shell| Arc::clone(&shell.terminal))
    }

    /// Kills a shell and drops its pane, mirroring the TUI's own close: the daemon forgets
    /// the terminal, so no surface sees it again. Reports whether there was one to close.
    pub fn close_shell(&mut self, key: &str, id: &str) -> bool {
        let Some(terminals) = self.terminals.get(key) else {
            return false;
        };
        let mut terminals = terminals.lock().unwrap();
        let Some(index) = terminals.shells.iter().position(|shell| shell.id == id) else {
            return false;
        };
        terminals.shells.remove(index).terminal.terminate();
        terminals.selected_shell = terminals
            .selected_shell
            .min(terminals.shells.len().saturating_sub(1));
        true
    }

    /// Adopts shells another surface opened for this session, then reports the full list.
    ///
    /// Costs a daemon round trip, so it is for a caller that is about to show the list --
    /// `shells` alone answers from what this process already knows.
    pub fn refresh_shells(
        &mut self,
        session: &Session,
        current_exe: &Path,
        size: (u16, u16),
    ) -> io::Result<Vec<ShellInfo>> {
        self.ensure_session_view(session, current_exe, size)?;
        if let Some(socket) = self.daemon_socket.clone() {
            let owner_id = self.lease_owner.instance_id.clone();
            let handle = Arc::clone(self.terminals.entry(session.key.clone()).or_default());
            let mut terminals = handle.lock().unwrap();
            adopt_daemon_shells(&mut terminals, &socket, &session.key, &owner_id, size)?;
        }
        Ok(self.shells(&session.key))
    }

    pub fn set_notice(&mut self, key: &str, notice: String) {
        self.terminals
            .entry(key.to_owned())
            .or_default()
            .lock()
            .unwrap()
            .notice = Some(notice);
    }

    pub fn terminate_agent(&mut self, key: &str) {
        if let Some(agent) = self
            .terminals
            .get(key)
            .and_then(|terminals| terminals.lock().unwrap().agent.take())
        {
            agent.terminate();
        }
    }

    pub fn ensure_session_view(
        &mut self,
        session: &Session,
        current_exe: &Path,
        size: (u16, u16),
    ) -> io::Result<()> {
        let daemon_socket = self.ensure_daemon(current_exe)?;
        let handle = Arc::clone(self.terminals.entry(session.key.clone()).or_default());
        let terminals = &mut *handle.lock().unwrap();
        terminals.daemon_socket.clone_from(&daemon_socket);
        terminals
            .lease_owner_id
            .clone_from(&self.lease_owner.instance_id);
        let Some(socket) = &daemon_socket else {
            return Ok(());
        };

        // Only when nothing is open yet: this runs on every session activation, and a daemon
        // round trip per call would sit in the path of every prompt and screen read. A caller
        // that is about to *show* the list pays for the refresh explicitly (`refresh_shells`).
        if terminals.shells.is_empty() {
            adopt_daemon_shells(
                &mut *terminals,
                socket,
                &session.key,
                &self.lease_owner.instance_id,
                size,
            )?;
        }

        if terminals.agent.is_none()
            && let DaemonResponse::List(ids) = daemon_request(
                socket,
                &DaemonRequest::List {
                    prefix: format!("agent|{}", session.key),
                },
            )?
            && let Some(id) = ids
                .into_iter()
                .find(|id| id == &format!("agent|{}", session.key))
        {
            terminals.agent = Some(Arc::new(ManagedTerminal::connect_remote(
                socket.clone(),
                id,
                self.lease_owner.instance_id.clone(),
                size,
            )?));
        }
        Ok(())
    }

    pub fn ensure_agent(
        &mut self,
        session: &Session,
        current_exe: &Path,
        new_session: bool,
        size: (u16, u16),
    ) -> io::Result<Arc<ManagedTerminal>> {
        self.ensure_session_view(session, current_exe, size)?;
        let daemon_socket = self.daemon_socket.clone();
        let agent_id = format!("agent|{}", session.key);
        let handle = Arc::clone(&self.terminals[&session.key]);
        let terminals = &mut *handle.lock().unwrap();
        let needs_spawn = terminals
            .agent
            .as_ref()
            .is_none_or(|value| !value.is_alive());
        if needs_spawn {
            let spec = agent_command(&self.config, session, current_exe, new_session);
            terminals.agent = Some(Arc::new(if let Some(socket) = daemon_socket {
                ManagedTerminal::ensure_remote(
                    socket,
                    agent_id,
                    self.lease_owner.instance_id.clone(),
                    &spec,
                    size,
                )?
            } else {
                ManagedTerminal::spawn(&spec, size)?
            }));
        }
        Ok(Arc::clone(terminals.agent.as_ref().unwrap()))
    }

    pub fn add_shell(&mut self, session: &Session, size: (u16, u16)) -> io::Result<ShellInfo> {
        let handle = Arc::clone(self.terminals.entry(session.key.clone()).or_default());
        let terminals = &mut *handle.lock().unwrap();
        let name = terminals.next_shell_name();
        let (id, shell) = terminals.spawn_shell(session, size)?;
        terminals.shells.push(ShellPane::new(id, shell, name));
        terminals.selected_shell = terminals.shells.len() - 1;
        Ok(terminals.shells[terminals.selected_shell].info())
    }

    /// Claims the session's input lease for this process, so its writes stop losing to
    /// whoever holds it.
    ///
    /// This is the piece of `attach_workspace`'s takeover that a surface without a
    /// full-screen attach loop still needs: the web server never calls `attach_workspace`,
    /// so a browser had no way past a lease a TUI was holding. `force` is the same flag the
    /// TUI's takeover key sets.
    ///
    /// A non-forced call is also the only way to *ask* who holds the lease -- the daemon has
    /// no read-only lease query -- and it is safe to ask, because the daemon only denies
    /// while the holder is alive and has validated within `LEASE_STALE_AFTER`.
    pub fn acquire_lease(
        &mut self,
        session_key: &str,
        current_exe: &Path,
        force: bool,
    ) -> io::Result<LeaseOutcome> {
        // No daemon means every terminal is process-local, so there is nothing to contend
        // for and nothing that could have denied a write in the first place.
        let Some(socket) = self.ensure_daemon(current_exe)? else {
            return Ok(LeaseOutcome::Granted);
        };
        match daemon_request(
            &socket,
            &DaemonRequest::Acquire {
                session_key: session_key.to_owned(),
                owner: self.lease_owner.clone(),
                force,
            },
        )? {
            DaemonResponse::LeaseGranted => Ok(LeaseOutcome::Granted),
            DaemonResponse::LeaseDenied { owner } => Ok(LeaseOutcome::Denied(LeaseHolder {
                pid: owner.pid,
                instance_id: owner.instance_id,
                started_at: owner.started_at,
            })),
            DaemonResponse::Error(error) => Err(io::Error::other(error)),
            other => Err(io::Error::other(format!(
                "unexpected lease response: {other:?}"
            ))),
        }
    }

    /// Claims the session lease and opens a workspace on it.
    ///
    /// The returned [`WorkspaceSession`] is stepped by the caller rather than run to
    /// completion here, so that the caller can release whatever it locked to reach this
    /// manager between frames.
    pub fn begin_workspace(
        &mut self,
        session: &Session,
        focus: WorkspaceFocus,
        force_takeover: bool,
        chrome: WorkspaceChrome,
    ) -> io::Result<WorkspaceSession> {
        let bindings = WorkspaceBindings::from_config(&self.config);
        let needs_lease = focus != WorkspaceFocus::Sessions;
        let held = if needs_lease && let Some(socket) = &self.daemon_socket {
            match daemon_request(
                socket,
                &DaemonRequest::Acquire {
                    session_key: session.key.clone(),
                    owner: self.lease_owner.clone(),
                    force: force_takeover,
                },
            )? {
                DaemonResponse::LeaseGranted => true,
                DaemonResponse::LeaseDenied { owner } => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!(
                            "session is open in PID {} (instance {}, started {}); return to Dashboard and press t to force takeover",
                            owner.pid,
                            owner.instance_id.chars().take(8).collect::<String>(),
                            owner.started_at
                        ),
                    ));
                }
                DaemonResponse::Error(error) => return Err(io::Error::other(error)),
                response => {
                    return Err(io::Error::other(format!(
                        "unexpected lease response: {response:?}"
                    )));
                }
            }
        } else {
            false
        };
        let lease = WorkspaceLease {
            socket: self.daemon_socket.clone(),
            session_key: session.key.clone(),
            owner_id: self.lease_owner.instance_id.clone(),
            held,
        };
        let handle = self
            .terminals
            .get(&session.key)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "agent terminal is not open"))
            .map(Arc::clone)?;
        SessionTerminals::begin_workspace(handle, focus, bindings, lease, chrome)
    }

    pub fn alive_keys(&self) -> Vec<String> {
        self.terminals
            .iter()
            .filter(|(_, terminals)| {
                terminals
                    .lock()
                    .unwrap()
                    .agent
                    .as_ref()
                    .is_some_and(|agent| agent.is_alive())
            })
            .map(|(key, _)| key.clone())
            .collect()
    }

    pub fn agent_alive(&self, key: &str) -> bool {
        self.agent(key).is_some_and(|agent| agent.is_alive())
    }

    pub fn shutdown(&mut self) {
        self.terminals.clear();
    }

    pub fn rekey(&mut self, old_key: &str, new_key: String) {
        if old_key == new_key || self.terminals.contains_key(&new_key) {
            return;
        }
        if let Some(terminals) = self.terminals.remove(old_key) {
            if let Some(socket) = &self.daemon_socket {
                let old_agent = format!("agent|{old_key}");
                let new_agent = format!("agent|{new_key}");
                let old_shell = format!("shell|{old_key}|");
                let new_shell = format!("shell|{new_key}|");
                let _ = daemon_request(
                    socket,
                    &DaemonRequest::Rekey {
                        old_prefix: old_agent.clone(),
                        new_prefix: new_agent.clone(),
                    },
                );
                let _ = daemon_request(
                    socket,
                    &DaemonRequest::Rekey {
                        old_prefix: old_shell.clone(),
                        new_prefix: new_shell.clone(),
                    },
                );
                let terminals = terminals.lock().unwrap();
                if let Some(agent) = &terminals.agent {
                    agent.rekey_prefix(&old_agent, &new_agent);
                }
                for shell in &terminals.shells {
                    shell.terminal.rekey_prefix(&old_shell, &new_shell);
                }
            }
            self.terminals.insert(new_key, terminals);
        }
    }
}

pub fn agent_command(
    config: &AgentConsoleConfig,
    session: &Session,
    current_exe: &Path,
    new_session: bool,
) -> CommandSpec {
    let hook_command = format!(
        "{} hook {}",
        shell_quote(current_exe.as_os_str()),
        session.agent.label()
    );
    let spec = match session.agent {
        AgentKind::Codex => {
            let mut spec = CommandSpec::new("codex", &session.cwd);
            if !new_session {
                spec = spec.arg("resume");
            }
            spec = spec
                .arg("--no-alt-screen")
                .arg("-C")
                .arg(session.cwd.as_os_str());
            if !new_session {
                spec = spec.arg(&session.provider_session_id);
            }
            for event in [
                "SessionStart",
                "UserPromptSubmit",
                "PreToolUse",
                "PermissionRequest",
                "PostToolUse",
                "Stop",
            ] {
                let command = toml_string(&hook_command);
                spec = spec.arg("-c").arg(format!(
                    "hooks.{event}=[{{hooks=[{{type=\"command\",command={command}}}]}}]"
                ));
            }
            spec
        }
        AgentKind::Claude => {
            let hooks = claude_hook_settings(&hook_command);
            // For resume, claude looks up the session in the project dir derived from cwd
            // (replacing '/' with '-'). If the session entered a worktree mid-run, session.cwd
            // reflects the worktree path, not the project root where the JSONL was created.
            // Walk up from session.cwd until we find the ancestor that encodes to the same
            // project dir as the transcript file, so --resume finds the session.
            let resume_cwd = if new_session {
                session.cwd.clone()
            } else {
                claude_resume_cwd(session)
            };
            let mut spec = CommandSpec::new("claude", &resume_cwd)
                .arg("--settings")
                .arg(hooks);
            // Codex is asked for this with `--no-alt-screen`; Claude Code's switch is an
            // environment variable. Either way the point is the same: an agent that takes
            // the alternate screen has no scrollback at all, so its terminal shows the
            // current screen and nothing above it -- in the dashboard and in a browser alike
            // -- and every wheel notch has to be answered by the agent itself instead of by
            // the buffer this console already keeps. Left alone if the user set it: they
            // asked for the alternate screen on purpose.
            if env::var_os(CLAUDE_ALTERNATE_SCREEN_VAR).is_none() {
                spec = spec.env(CLAUDE_ALTERNATE_SCREEN_VAR, "1");
            }
            if new_session {
                spec = spec
                    .arg("--session-id")
                    .arg(&session.provider_session_id)
                    .arg("--name")
                    .arg(&session.name);
            } else {
                spec = spec.arg("--resume").arg(&session.provider_session_id);
            }
            spec
        }
    };
    let CommandSpec { args, cwd, env, .. } = spec;
    let command = config.provider_command(session.agent, args);
    CommandSpec {
        program: command.program,
        args: command.args,
        cwd,
        env,
    }
}

pub fn staged_shell_text(cwd: &Path, capture: &str) -> Option<String> {
    let trimmed = capture.trim();
    if trimmed.is_empty() {
        return None;
    }
    let bounded = if trimmed.len() <= CAPTURE_BYTES {
        trimmed.to_owned()
    } else {
        let mut start = trimmed.len() - CAPTURE_BYTES;
        while !trimmed.is_char_boundary(start) {
            start += 1;
        }
        trimmed[start..].to_owned()
    };
    Some(format!(
        "\nShell output from {}:\n<shell-output>\n{}\n</shell-output>\n",
        cwd.display(),
        bounded
    ))
}

/// Rejects a daemon socket path the kernel cannot bind.
///
/// The path is copied into a fixed-size `sun_path` buffer, and a path over the limit makes
/// `bind` fail inside the freshly spawned daemon -- where nothing is watching. The parent then
/// waits out its readiness timeout and every session silently streams nothing. Failing here
/// turns that into one message that names the fix.
fn check_socket_path(path: &Path) -> io::Result<()> {
    let length = path.as_os_str().len();
    if length < SUN_PATH_CAPACITY {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "PTY daemon socket path is {length} bytes, over this platform's {} byte limit: {}\n\
             Set AGENT_CONSOLE_STATE_DIR to a shorter directory and start again.",
            SUN_PATH_CAPACITY - 1,
            path.display()
        ),
    ))
}

pub fn bracketed_paste(value: &str) -> Vec<u8> {
    let safe = value.replace('\x1b', "");
    let mut output = Vec::with_capacity(safe.len() + 12);
    output.extend_from_slice(b"\x1b[200~");
    output.extend_from_slice(safe.as_bytes());
    output.extend_from_slice(b"\x1b[201~");
    output
}

/// Decodes raw terminal output into the plain, de-duplicated, bounded text the TUI copies
/// and stages. Public so a caller holding raw bytes of its own -- the web server, which
/// collects them through `ManagedTerminal::poll_raw` rather than the shared parser -- renders
/// them identically instead of growing a second, drifting decoder.
pub fn plain_text(bytes: &[u8]) -> String {
    let mut stripped = Vec::with_capacity(bytes.len());
    let mut state = 0_u8;
    for &byte in bytes {
        match state {
            0 if byte == 0x1b => state = 1,
            0 if byte == 0x08 || byte == 0x7f => {
                stripped.pop();
            }
            0 if byte == b'\t' => stripped.extend_from_slice(b"    "),
            0 if byte == b'\r' || byte == b'\n' || byte >= 0x20 => stripped.push(byte),
            0 => {}
            1 if byte == b'[' => state = 2,
            1 if byte == b']' => state = 3,
            1 => state = 0,
            2 if (0x40..=0x7e).contains(&byte) => state = 0,
            2 => {}
            3 if byte == 0x07 => state = 0,
            3 if byte == 0x1b => state = 4,
            3 => {}
            4 if byte == b'\\' => state = 0,
            4 => state = 3,
            _ => state = 0,
        }
    }
    let text = String::from_utf8_lossy(&stripped)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let mut lines = VecDeque::new();
    for line in text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
    {
        if lines
            .back()
            .is_none_or(|previous: &String| previous != line)
        {
            lines.push_back(line.to_owned());
        }
        while lines.len() > CAPTURE_LINES {
            lines.pop_front();
        }
    }
    let mut output = lines.into_iter().collect::<Vec<_>>().join("\n");
    if output.len() > CAPTURE_BYTES {
        let mut start = output.len() - CAPTURE_BYTES;
        while !output.is_char_boundary(start) {
            start += 1;
        }
        output = output[start..].to_owned();
    }
    output
}

/// Find the cwd to use for `claude --resume` for a Claude session.
///
/// `claude --resume <id>` searches for the session in the project directory derived from
/// the current working directory (path with '/' replaced by '-'). If session.cwd is a
/// worktree or subdirectory entered mid-session, it won't match the project dir where the
/// JSONL was created. Walk up from session.cwd to find the ancestor that encodes to the
/// same project dir as the transcript file.
fn claude_resume_cwd(session: &crate::model::Session) -> std::path::PathBuf {
    let project_dir_name = session
        .transcript_path
        .as_deref()
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(str::to_owned);

    if let Some(project_dir) = project_dir_name {
        let mut candidate = session.cwd.as_path();
        loop {
            let encoded = candidate.to_string_lossy().replace('/', "-");
            if encoded == project_dir {
                return candidate.to_path_buf();
            }
            match candidate.parent() {
                Some(parent) if parent != candidate => candidate = parent,
                _ => break,
            }
        }
    }
    session.cwd.clone()
}

fn claude_hook_settings(command: &str) -> String {
    let hook = serde_json::json!({"hooks": [{"type": "command", "command": command}]});
    let mut events = serde_json::Map::new();
    for name in [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PermissionRequest",
        "PostToolUse",
        "PostToolUseFailure",
        "Notification",
        "Stop",
        "StopFailure",
        "SessionEnd",
    ] {
        events.insert(name.into(), ValueArray::one(hook.clone()));
    }
    serde_json::json!({"hooks": events}).to_string()
}

struct ValueArray;

impl ValueArray {
    fn one(value: serde_json::Value) -> serde_json::Value {
        serde_json::Value::Array(vec![value])
    }
}

fn shell_quote(value: &OsStr) -> String {
    let value = value.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;
    use crate::model::{SessionStatus, SessionSummary};

    fn session(agent: AgentKind, cwd: &Path) -> Session {
        Session {
            key: Session::stable_key(agent, "id"),
            provider_session_id: "id".into(),
            name: "repo".into(),
            search_terms: Vec::new(),
            first_prompt: None,
            agent,
            status: SessionStatus::Idle,
            cwd: cwd.to_owned(),
            branch: None,
            transcript_path: None,
            transcript_modified_at: 0,
            transcript_fingerprint: String::new(),
            summary_fingerprint: String::new(),
            summary_updated_at: None,
            summary_error: None,
            summary: SessionSummary::default(),
            recent_activity: Vec::new(),
            pending_decisions: Vec::new(),
            pending_shell_injection: None,
            managed_alive: false,
            unavailable_reason: None,
            discovered_after_startup: false,
        }
    }

    fn wait_for_capture(terminal: &ManagedTerminal, needle: &str) -> String {
        let start = Instant::now();
        loop {
            let output = terminal.plain_capture();
            let compact = output.split_whitespace().collect::<Vec<_>>().join(" ");
            if compact.contains(needle) || start.elapsed() >= Duration::from_secs(5) {
                return output;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn command_construction_uses_provider_resume_contracts() {
        let cwd = Path::new("/tmp/repo");
        let executable = Path::new("/tmp/agent console");
        let config = AgentConsoleConfig::default();
        let codex = agent_command(&config, &session(AgentKind::Codex, cwd), executable, false);
        assert_eq!(codex.program, "codex");
        assert_eq!(codex.args[0], "resume");
        assert!(codex.args.iter().any(|arg| arg == "id"));
        assert!(codex.args.iter().any(|arg| arg == "--no-alt-screen"));

        let claude = agent_command(&config, &session(AgentKind::Claude, cwd), executable, false);
        assert_eq!(claude.program, "claude");
        assert!(
            claude
                .args
                .windows(2)
                .any(|pair| pair == ["--resume", "id"])
        );
    }

    /// Both providers are asked for the same thing in the shape each one offers: Codex takes
    /// a flag, Claude Code takes an environment variable. An agent on the alternate screen
    /// keeps no scrollback, so its terminal opens on the current screen with nothing above it
    /// -- the report that a phone cannot swipe back to a session's earlier output, and the
    /// same reason scrolling the dashboard's agent pane has to be answered by the agent
    /// instead of by the buffer this console already keeps.
    #[test]
    fn both_providers_are_kept_off_the_alternate_screen() {
        let _guard = ALTERNATE_SCREEN_ENV
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let cwd = Path::new("/tmp/repo");
        let executable = Path::new("/tmp/agent console");
        let config = AgentConsoleConfig::default();

        let codex = agent_command(&config, &session(AgentKind::Codex, cwd), executable, true);
        assert!(codex.args.iter().any(|arg| arg == "--no-alt-screen"));

        let claude = agent_command(&config, &session(AgentKind::Claude, cwd), executable, true);
        assert!(
            claude
                .env
                .iter()
                .any(|(name, value)| name == CLAUDE_ALTERNATE_SCREEN_VAR && value == "1"),
            "claude has no flag for it, so the spec has to carry the variable: {:?}",
            claude.env
        );
    }

    /// The spawn environment has to survive the trip to the daemon, which is another process
    /// and the one that actually starts the agent.
    #[test]
    fn a_spawn_environment_survives_the_daemon_wire() {
        let spec = CommandSpec::new("claude", Path::new("/tmp/repo"))
            .arg("--resume")
            .env(CLAUDE_ALTERNATE_SCREEN_VAR, "1");

        let wire = WireCommandSpec::from(&spec);
        let round_tripped: WireCommandSpec =
            serde_json::from_str(&serde_json::to_string(&wire).unwrap()).unwrap();

        assert_eq!(
            round_tripped.command_spec().env,
            vec![(
                OsString::from(CLAUDE_ALTERNATE_SCREEN_VAR),
                OsString::from("1")
            )]
        );
    }

    /// A daemon older than this build left the field out entirely; a spec with no environment
    /// has to stay parseable rather than failing the spawn.
    #[test]
    fn a_spec_from_before_the_environment_existed_still_parses() {
        let wire: WireCommandSpec =
            serde_json::from_str(r#"{"program":"claude","args":[],"cwd":"/tmp"}"#).unwrap();
        assert!(wire.command_spec().env.is_empty());
    }

    /// The process environment is shared by every test in this binary, and two of them read
    /// this variable, so the one that *writes* it has to exclude the one that reads it.
    static ALTERNATE_SCREEN_ENV: Mutex<()> = Mutex::new(());

    /// A user who set it themselves gets to keep whatever they chose, including "off".
    #[test]
    fn an_explicit_setting_of_its_own_is_left_alone() {
        let _guard = ALTERNATE_SCREEN_ENV
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // SAFETY: no other thread reads this variable while the guard above is held.
        unsafe { env::set_var(CLAUDE_ALTERNATE_SCREEN_VAR, "0") };
        let claude = agent_command(
            &AgentConsoleConfig::default(),
            &session(AgentKind::Claude, Path::new("/tmp/repo")),
            Path::new("/tmp/agent console"),
            true,
        );
        let carried = claude
            .env
            .iter()
            .any(|(name, _)| name == CLAUDE_ALTERNATE_SCREEN_VAR);
        unsafe { env::remove_var(CLAUDE_ALTERNATE_SCREEN_VAR) };
        assert!(
            !carried,
            "the console must not overwrite a choice the user made deliberately"
        );
    }

    #[test]
    fn agent_command_uses_configured_provider_prefix() {
        let config = AgentConsoleConfig::parse(
            "[providers]\ncodex = [\"proxychains4\", \"codex\", \"--profile\", \"work\"]\n",
            Path::new("config.toml"),
        )
        .unwrap();
        let command = agent_command(
            &config,
            &session(AgentKind::Codex, Path::new("/tmp/repo")),
            Path::new("/tmp/agent-console"),
            false,
        );

        assert_eq!(command.program, "proxychains4");
        assert_eq!(command.args[0], "codex");
        assert_eq!(command.args[1], "--profile");
        assert_eq!(command.args[2], "work");
        assert_eq!(command.args[3], "resume");
        assert!(command.args.iter().any(|arg| arg == "id"));
    }

    #[test]
    fn claude_resume_cwd_walks_up_to_project_root_when_in_worktree() {
        use tempfile::tempdir;
        let root = tempdir().unwrap();
        // Simulate: session created in /project, then entered worktree at /project/worktrees/branch
        let project = root.path().join("project");
        let worktree = root.path().join("project/worktrees/branch");
        std::fs::create_dir_all(&worktree).unwrap();

        // Transcript is stored under the project dir encoding
        let transcript_dir = root
            .path()
            .join(project.to_string_lossy().replace('/', "-"));
        std::fs::create_dir_all(&transcript_dir).unwrap();
        let transcript = transcript_dir.join("abc.jsonl");
        std::fs::write(&transcript, "").unwrap();

        let mut s = session(AgentKind::Claude, &worktree);
        s.transcript_path = Some(transcript);

        let resume = claude_resume_cwd(&s);
        assert_eq!(
            resume, project,
            "should walk up from worktree to project root"
        );
    }

    #[test]
    fn claude_resume_cwd_returns_cwd_when_no_worktree() {
        use tempfile::tempdir;
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir_all(&project).unwrap();

        let transcript_dir = root
            .path()
            .join(project.to_string_lossy().replace('/', "-"));
        std::fs::create_dir_all(&transcript_dir).unwrap();
        let transcript = transcript_dir.join("abc.jsonl");
        std::fs::write(&transcript, "").unwrap();

        let mut s = session(AgentKind::Claude, &project);
        s.transcript_path = Some(transcript);

        let resume = claude_resume_cwd(&s);
        assert_eq!(
            resume, project,
            "should return cwd unchanged when already correct"
        );
    }

    #[test]
    fn a_spawned_agent_does_not_inherit_the_launching_session_markers() {
        // Launching the console from inside an agent session used to leak these into every
        // agent it spawned. CLAUDE_CODE_CHILD_SESSION turns the provider's transcript saving
        // off, which leaves discovery -- and the conversation view -- silently empty.
        let root = tempdir().unwrap();
        let script = root.path().join("env.sh");
        fs::write(
            &script,
            "#!/bin/sh\nprintf 'child=[%s] id=[%s] keep=[%s]\\n' \
             \"$CLAUDE_CODE_CHILD_SESSION\" \"$CLAUDE_CODE_SESSION_ID\" \"$AGENT_CONSOLE_KEEP\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&script).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&script, permissions).unwrap();
        }
        // SAFETY: single-threaded test setup, before any spawn observes the environment.
        unsafe {
            env::set_var("CLAUDE_CODE_CHILD_SESSION", "1");
            env::set_var("CLAUDE_CODE_SESSION_ID", "parent-session");
            env::set_var("AGENT_CONSOLE_KEEP", "kept");
        }
        let terminal =
            ManagedTerminal::spawn(&CommandSpec::new(script.as_os_str(), root.path()), (0, 0))
                .unwrap();
        let capture = wait_for_capture(&terminal, "child=");
        unsafe {
            env::remove_var("CLAUDE_CODE_CHILD_SESSION");
            env::remove_var("CLAUDE_CODE_SESSION_ID");
            env::remove_var("AGENT_CONSOLE_KEEP");
        }
        assert!(
            capture.contains("child=[] id=[]"),
            "session markers leaked into the spawned agent: {capture}"
        );
        assert!(
            capture.contains("keep=[kept]"),
            "unrelated environment should still be inherited: {capture}"
        );
    }

    #[test]
    fn pty_captures_output_accepts_input_and_outlives_detach_state() {
        let root = tempdir().unwrap();
        let script = root.path().join("echo.sh");
        fs::write(
            &script,
            "#!/bin/sh\nprintf 'ready\\n'\nread line\nprintf 'got:%s\\n' \"$line\"\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(0o755);
            fs::set_permissions(&script, permissions).unwrap();
        }
        let terminal =
            ManagedTerminal::spawn(&CommandSpec::new(script.as_os_str(), root.path()), (0, 0))
                .unwrap();
        assert!(wait_for_capture(&terminal, "ready").contains("ready"));
        terminal.write(b"hello\n").unwrap();
        assert!(wait_for_capture(&terminal, "got:hello").contains("got:hello"));
    }

    /* ------------------------------------------------ one PTY, several viewers

    A session is often open in more than one window at once -- a desktop browser, a phone,
    the dashboard's own workspace -- and they share one PTY, which has exactly one size.
    Applying whichever window resized last is what squashed a 180-column desktop the moment
    a 40-column phone attached, and, because resizing a PTY reflows its scrollback, mangled
    the history the desktop was reading at the same time. The rule below is the one every
    terminal multiplexer settled on. */

    /// A stand-in for an agent that answers with the size it is running under, so a test can
    /// observe what the PTY actually did rather than what it was asked to do.
    fn size_probe_script(root: &Path) -> std::path::PathBuf {
        let script = root.join("size-probe.sh");
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

    /// Prods the probe through the daemon and waits for *that* probe's answer, as `rowsxcols`.
    fn probe_daemon_size(state: &mut PtyDaemonState, id: &str, probe: usize) -> String {
        state.handle(DaemonRequest::Write {
            id: id.to_owned(),
            owner_id: "test".into(),
            bytes: b"\n".to_vec(),
        });
        let marker = format!("probe{probe}=");
        let mut offset = 0;
        let mut output = Vec::new();
        let start = Instant::now();
        loop {
            if let DaemonResponse::Poll { end, bytes, .. } = state
                .handle(DaemonRequest::Poll {
                    id: id.to_owned(),
                    offset,
                    scrollback: false,
                })
                .0
            {
                offset = end;
                output.extend(bytes);
            }
            let text = plain_text(&output);
            if let Some(size) = text
                .split_whitespace()
                .find_map(|word| word.strip_prefix(marker.as_str()))
            {
                return size.to_owned();
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "probe {probe} never answered: {text}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn viewers(entries: &[(&str, (u16, u16))]) -> ViewerSizes {
        entries
            .iter()
            .map(|(id, size)| ((*id).to_owned(), *size))
            .collect()
    }

    /// The rule itself: the PTY fits inside every window, taking each dimension on its own so
    /// a tall narrow phone beside a short wide desktop still leaves both able to show it all.
    #[test]
    fn the_pty_is_sized_to_the_smallest_of_every_viewer_in_each_dimension() {
        assert_eq!(
            smallest_viewer(&viewers(&[("desktop", (180, 50)), ("phone", (40, 20))])),
            Some((40, 20))
        );
        assert_eq!(
            smallest_viewer(&viewers(&[("wide", (200, 20)), ("tall", (60, 80))])),
            Some((60, 20)),
            "each dimension is taken from whichever viewer is smaller in it"
        );
    }

    /// A single viewer must behave exactly as it did before viewers existed -- including the
    /// defect this replaced, where attaching to an already-running terminal kept the size the
    /// first window had picked and rendered into a strip of the new one.
    #[test]
    fn one_viewer_gets_exactly_the_size_it_asked_for() {
        assert_eq!(
            smallest_viewer(&viewers(&[("only", (180, 50))])),
            Some((180, 50))
        );
    }

    /// Nothing attached is not a size. Resizing for nobody would only make the agent repaint.
    #[test]
    fn no_viewers_at_all_leaves_the_terminal_where_it_is() {
        assert_eq!(smallest_viewer(&ViewerSizes::new()), None);
    }

    /// The leak to watch for: a socket that dies without a close frame, or a whole process
    /// killed outright, cannot detach itself. A viewer nobody can remove would hold the
    /// terminal at a dead window's size for every other reader, forever.
    #[cfg(unix)]
    #[test]
    fn a_viewer_whose_process_is_gone_stops_holding_the_terminal_small() {
        // A pid that has certainly exited: a child we reaped ourselves.
        let mut dead = std::process::Command::new("/usr/bin/true").spawn().unwrap();
        let dead_pid = dead.id();
        dead.wait().unwrap();

        let mut registry = viewers(&[("phone", (40, 20)), ("no-pid-at-all", (200, 60))]);
        registry.insert(format!("{dead_pid}:ws:abandoned"), (40, 20));
        registry.insert(format!("{}:ws:live", std::process::id()), (180, 50));
        drop_dead_viewers(&mut registry);

        assert!(
            !registry.contains_key(&format!("{dead_pid}:ws:abandoned")),
            "a viewer from a process that is gone has to stop being counted"
        );
        assert!(
            registry.contains_key(&format!("{}:ws:live", std::process::id())),
            "this process's own viewers are live"
        );
        assert!(
            registry.contains_key("no-pid-at-all"),
            "a name that carries no pid says nothing about liveness, so it is kept"
        );
    }

    /// The measurement this whole change exists to fix, end to end through the daemon that
    /// owns the PTY: a desktop attaches wide, a phone joins narrow, the phone leaves.
    #[cfg(unix)]
    #[test]
    fn a_second_viewer_shrinks_the_pty_and_its_leaving_gives_the_size_back() {
        let root = tempdir().unwrap();
        let script = size_probe_script(root.path());
        let mut state = PtyDaemonState::default();
        let id = "agent|sizing";
        assert!(matches!(
            state
                .handle(DaemonRequest::Ensure {
                    id: id.into(),
                    spec: WireCommandSpec::from(&CommandSpec::new(script.as_os_str(), root.path())),
                    cols: 80,
                    rows: 24,
                })
                .0,
            DaemonResponse::Ok
        ));

        let resize = |state: &mut PtyDaemonState, viewer: &str, cols, rows| match state
            .handle(DaemonRequest::Resize {
                id: id.into(),
                cols,
                rows,
                viewer: Some(viewer.into()),
            })
            .0
        {
            DaemonResponse::Size { cols, rows } => (cols, rows),
            other => panic!("a viewer's resize has to answer with a size: {other:?}"),
        };

        // A desktop, on its own: it gets exactly what it asked for.
        assert_eq!(resize(&mut state, "desktop", 180, 50), (180, 50));
        assert_eq!(probe_daemon_size(&mut state, id, 1), "50x180");

        // A phone joins. The PTY has to fit inside it -- and the desktop is told so, rather
        // than being left believing it still has 180 columns.
        assert_eq!(resize(&mut state, "phone", 40, 20), (40, 20));
        assert_eq!(probe_daemon_size(&mut state, id, 2), "20x40");

        // The phone leaves. The desktop must get its width back, not stay squashed.
        let grown = match state
            .handle(DaemonRequest::Detach {
                id: id.into(),
                viewer: "phone".into(),
            })
            .0
        {
            DaemonResponse::Size { cols, rows } => (cols, rows),
            other => panic!("detaching has to answer with the size left behind: {other:?}"),
        };
        assert_eq!(grown, (180, 50));
        assert_eq!(probe_daemon_size(&mut state, id, 3), "50x180");

        state.handle(DaemonRequest::Terminate { id: id.into() });
    }

    /// Polls carry the size so a window learns about a change it did not cause: the phone
    /// attaching is what shrinks the desktop, and nothing happens on the desktop's own socket.
    #[cfg(unix)]
    #[test]
    fn a_poll_reports_the_size_the_pty_is_running_at() {
        let root = tempdir().unwrap();
        let mut state = PtyDaemonState::default();
        let id = "agent|poll-size";
        state.handle(DaemonRequest::Ensure {
            id: id.into(),
            spec: WireCommandSpec::from(
                &CommandSpec::new("/bin/sh", root.path())
                    .arg("-c")
                    .arg("sleep 5"),
            ),
            cols: 80,
            rows: 24,
        });
        state.handle(DaemonRequest::Resize {
            id: id.into(),
            cols: 180,
            rows: 50,
            viewer: Some("desktop".into()),
        });
        state.handle(DaemonRequest::Resize {
            id: id.into(),
            cols: 40,
            rows: 20,
            viewer: Some("phone".into()),
        });

        let DaemonResponse::Poll { cols, rows, .. } = state
            .handle(DaemonRequest::Poll {
                id: id.into(),
                offset: 0,
                scrollback: false,
            })
            .0
        else {
            panic!("a poll has to answer with a poll");
        };
        assert_eq!((cols, rows), (40, 20));

        state.handle(DaemonRequest::Terminate { id: id.into() });
    }

    /// A local terminal -- no daemon in the picture -- follows the same rule, because the
    /// registry lives with the PTY rather than with whichever process is talking to it.
    #[cfg(unix)]
    #[test]
    fn a_process_local_terminal_is_sized_by_its_viewers_too() {
        let root = tempdir().unwrap();
        let script = size_probe_script(root.path());
        let terminal =
            ManagedTerminal::spawn(&CommandSpec::new(script.as_os_str(), root.path()), (80, 24))
                .unwrap();

        assert_eq!(
            terminal.resize_viewer("desktop", 180, 50).unwrap(),
            (180, 50)
        );
        assert_eq!(terminal.resize_viewer("phone", 40, 20).unwrap(), (40, 20));
        assert_eq!(terminal.size(), (40, 20));
        assert_eq!(terminal.detach_viewer("phone").unwrap(), (180, 50));
        assert_eq!(terminal.size(), (180, 50));
        // The last viewer leaving changes nothing: nobody is looking.
        assert_eq!(terminal.detach_viewer("desktop").unwrap(), (180, 50));

        terminal.terminate();
    }

    #[test]
    fn daemon_ensure_is_idempotent_and_keeps_terminal_state_between_clients() {
        let root = tempdir().unwrap();
        let mut state = PtyDaemonState::default();
        let spec = WireCommandSpec::from(&CommandSpec::new("/bin/sh", root.path()).arg("-c").arg(
            "printf 'daemon-ready\\n'; read line; printf 'daemon-got:%s\\n' \"$line\"; sleep 1",
        ));
        assert!(matches!(
            state
                .handle(DaemonRequest::Ensure {
                    id: "agent|test".into(),
                    spec: spec.clone(),
                    cols: 80,
                    rows: 24,
                })
                .0,
            DaemonResponse::Ok
        ));
        assert!(matches!(
            state
                .handle(DaemonRequest::Ensure {
                    id: "agent|test".into(),
                    spec: WireCommandSpec::from(
                        &CommandSpec::new("/bin/sh", root.path())
                            .arg("-c")
                            .arg("printf 'should-not-respawn\\n'")
                    ),
                    cols: 80,
                    rows: 24,
                })
                .0,
            DaemonResponse::Ok
        ));

        let mut offset = 0;
        let mut output = Vec::new();
        let start = Instant::now();
        while !plain_text(&output).contains("daemon-ready")
            && start.elapsed() < Duration::from_secs(2)
        {
            if let DaemonResponse::Poll { end, bytes, .. } = state
                .handle(DaemonRequest::Poll {
                    id: "agent|test".into(),
                    offset,
                    scrollback: false,
                })
                .0
            {
                offset = end;
                output.extend(bytes);
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(plain_text(&output).contains("daemon-ready"));
        assert!(!plain_text(&output).contains("should-not-respawn"));

        assert!(matches!(
            state
                .handle(DaemonRequest::Write {
                    id: "agent|test".into(),
                    owner_id: String::new(),
                    bytes: b"hello\n".to_vec(),
                })
                .0,
            DaemonResponse::Ok
        ));
        let start = Instant::now();
        while !plain_text(&output).contains("daemon-got:hello")
            && start.elapsed() < Duration::from_secs(2)
        {
            if let DaemonResponse::Poll { end, bytes, .. } = state
                .handle(DaemonRequest::Poll {
                    id: "agent|test".into(),
                    offset,
                    scrollback: false,
                })
                .0
            {
                offset = end;
                output.extend(bytes);
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(plain_text(&output).contains("daemon-got:hello"));
    }

    #[test]
    fn daemon_ensure_restarts_a_terminal_after_the_previous_process_exits() {
        let root = tempdir().unwrap();
        let launches = root.path().join("launches");
        let command = format!(
            "printf x >> '{}'; printf 'ready\\n'; sleep 0.05",
            launches.display()
        );
        let spec = WireCommandSpec::from(
            &CommandSpec::new("/bin/sh", root.path())
                .arg("-c")
                .arg(command),
        );
        let mut state = PtyDaemonState::default();

        assert!(matches!(
            state
                .handle(DaemonRequest::Ensure {
                    id: "agent|restart".into(),
                    spec: spec.clone(),
                    cols: 80,
                    rows: 24,
                })
                .0,
            DaemonResponse::Ok
        ));
        let start = Instant::now();
        while state
            .terminals
            .get("agent|restart")
            .is_some_and(LocalTerminal::is_alive)
            && start.elapsed() < Duration::from_secs(2)
        {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(fs::read_to_string(&launches).unwrap(), "x");

        assert!(matches!(
            state
                .handle(DaemonRequest::Ensure {
                    id: "agent|restart".into(),
                    spec,
                    cols: 80,
                    rows: 24,
                })
                .0,
            DaemonResponse::Ok
        ));
        let start = Instant::now();
        while fs::read_to_string(&launches).unwrap_or_default() != "xx"
            && start.elapsed() < Duration::from_secs(2)
        {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            fs::read_to_string(&launches).unwrap(),
            "xx",
            "Ensure must replace a retained dead terminal instead of reconnecting to it"
        );
    }

    #[test]
    fn daemon_replay_preserves_screen_after_raw_history_rollover() {
        let root = tempdir().unwrap();
        let terminal = LocalTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path()).arg("-c").arg(
                "printf '\\033]0;'; dd if=/dev/zero bs=1024 count=132 2>/dev/null | tr '\\000' x; printf '\\007FINAL_SCREEN'; sleep 2",
            ),
            (40, 6),
        )
        .unwrap();

        let start = Instant::now();
        loop {
            let state = terminal.output.lock().unwrap();
            let rolled_over = state.base_offset > 0;
            let reached_final = state
                .raw
                .iter()
                .copied()
                .collect::<Vec<_>>()
                .windows(b"FINAL_SCREEN".len())
                .any(|window| window == b"FINAL_SCREEN");
            drop(state);
            if rolled_over && reached_final {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "terminal did not produce enough output to roll over raw history"
            );
            thread::sleep(Duration::from_millis(20));
        }

        let delta = terminal.output_since(0, Scrollback::Omit);
        assert!(delta.start > 0);
        let checkpoint = delta
            .checkpoint
            .expect("rolled-over history must return a screen checkpoint");
        assert_eq!(delta.status_bar_rows, Some(Vec::new()));
        let mut replay_parser = vt100::Parser::new(6, 40, CAPTURE_LINES);
        let mut replay_scrollback = StatusBarScrollback::default();
        process_terminal_output(&mut replay_parser, &mut replay_scrollback, &checkpoint);
        process_terminal_output(&mut replay_parser, &mut replay_scrollback, &delta.bytes);

        let expected = terminal.parser.lock().unwrap().screen().contents();
        assert_eq!(replay_parser.screen().contents(), expected);
    }

    #[test]
    fn terminal_checkpoint_restores_screen_modes_and_scroll_region() {
        let mut parser = vt100::Parser::new(8, 40, CAPTURE_LINES);
        let mut scrollback = StatusBarScrollback::default();
        process_terminal_output(
            &mut parser,
            &mut scrollback,
            b"\x1b[?1049h\x1b[2;7r\x1b[?1002h\x1b[?1006h\x1b[4;6Hcheckpoint",
        );

        let checkpoint = terminal_state_checkpoint(&parser, &scrollback);
        let mut restored = vt100::Parser::new(8, 40, CAPTURE_LINES);
        let mut restored_scrollback = StatusBarScrollback::default();
        process_terminal_output(&mut restored, &mut restored_scrollback, &checkpoint);

        assert_eq!(restored.screen().contents(), parser.screen().contents());
        assert_eq!(
            restored.screen().cursor_position(),
            parser.screen().cursor_position()
        );
        assert!(restored.screen().alternate_screen());
        assert_eq!(
            restored.screen().mouse_protocol_mode(),
            parser.screen().mouse_protocol_mode()
        );
        assert_eq!(restored_scrollback.scroll_region, Some((1, 6)));
    }

    #[test]
    fn daemon_checkpoint_carries_codex_scrollback_beyond_the_raw_tail() {
        let root = tempdir().unwrap();
        let terminal = LocalTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path()).arg("-c").arg(
                "printf '\\033]0;'; dd if=/dev/zero bs=1024 count=132 2>/dev/null | tr '\\000' x; printf '\\007done'; sleep 2",
            ),
            (40, 6),
        )
        .unwrap();
        let start = Instant::now();
        while terminal.output.lock().unwrap().base_offset == 0 {
            assert!(start.elapsed() < Duration::from_secs(2));
            thread::sleep(Duration::from_millis(20));
        }
        terminal
            .status_bar_scrollback
            .lock()
            .unwrap()
            .rows
            .push_back(b"retained-before-128-kib".to_vec());

        let delta = terminal.output_since(0, Scrollback::Omit);
        assert!(delta.checkpoint.is_some());
        assert_eq!(
            delta.status_bar_rows,
            Some(vec![b"retained-before-128-kib".to_vec()])
        );
    }

    #[test]
    fn managed_terminal_poll_raw_reports_offsets_and_bytes_since_the_cursor() {
        let root = tempdir().unwrap();
        let terminal = ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path())
                .arg("-c")
                .arg("printf hello; sleep 10"),
            (40, 6),
        )
        .unwrap();
        terminal.wait_for_first_output(Duration::from_secs(5));

        let first = terminal.poll_raw(0, Scrollback::Omit).unwrap();
        assert_eq!(first.start, 0);
        assert!(first.end > first.start);
        assert!(String::from_utf8_lossy(&first.bytes).contains("hello"));
        assert!(first.checkpoint.is_none());
        assert!(first.alive);
        assert!(first.exit.is_none());

        let second = terminal.poll_raw(first.end, Scrollback::Omit).unwrap();
        assert_eq!(second.start, first.end);
        assert!(second.bytes.is_empty());

        terminal.terminate();
    }

    #[test]
    fn remote_terminal_marks_a_lost_daemon_dead_so_resume_can_respawn() {
        let root = tempdir().unwrap();
        let terminal = RemoteTerminal {
            socket: root.path().join("missing.sock"),
            id: Mutex::new("agent|codex:test".into()),
            owner_id: "owner".into(),
            offset: Mutex::new(0),
            output: Mutex::new(OutputState::default()),
            output_generation: AtomicU64::new(0),
            parser: Mutex::new(vt100::Parser::new(24, 80, CAPTURE_LINES)),
            status_bar_scrollback: Mutex::new(StatusBarScrollback::default()),
            size: Mutex::new((80, 24)),
        };

        assert!(!terminal.is_alive());
        let output = terminal.output.lock().unwrap();
        assert!(output.exited);
        assert_eq!(
            output.exit_description.as_deref(),
            Some("daemon disconnected")
        );
    }

    #[test]
    fn daemon_lease_refuses_a_second_owner_and_force_takeover_revokes_the_first() {
        let mut state = PtyDaemonState::default();
        let first = LeaseOwner::new_for_test("first", 101);
        let second = LeaseOwner::new_for_test("second", 202);

        assert!(matches!(
            state
                .handle(DaemonRequest::Acquire {
                    session_key: "codex:test".into(),
                    owner: first.clone(),
                    force: false,
                })
                .0,
            DaemonResponse::LeaseGranted
        ));
        assert!(matches!(
            state
                .handle(DaemonRequest::Acquire {
                    session_key: "codex:test".into(),
                    owner: second.clone(),
                    force: false,
                })
                .0,
            DaemonResponse::LeaseDenied { owner } if owner.instance_id == "first"
        ));
        assert!(matches!(
            state
                .handle(DaemonRequest::Acquire {
                    session_key: "codex:test".into(),
                    owner: second,
                    force: true,
                })
                .0,
            DaemonResponse::LeaseGranted
        ));
        assert!(!state.owner_can_write("codex:test", "first"));
        assert!(state.owner_can_write("codex:test", "second"));
        assert!(matches!(
            state
                .handle(DaemonRequest::Write {
                    id: "agent|codex:test".into(),
                    owner_id: "first".into(),
                    bytes: b"must be rejected".to_vec(),
                })
                .0,
            DaemonResponse::Error(error) if error.contains("another TUI")
        ));
    }

    #[test]
    fn a_socket_path_within_sun_path_is_accepted() {
        let short = PathBuf::from("/tmp/agent-console/pty-daemon.sock");
        assert!(short.as_os_str().len() < SUN_PATH_CAPACITY);
        assert!(check_socket_path(&short).is_ok());
    }

    /// A deep `AGENT_CONSOLE_STATE_DIR` used to spawn a daemon that could never bind, leaving
    /// every session streaming nothing with no error anywhere.
    #[test]
    fn an_over_long_socket_path_fails_with_the_length_the_limit_and_the_fix() {
        let long = PathBuf::from(format!("/tmp/{}/pty-daemon.sock", "deep".repeat(30)));
        let error = check_socket_path(&long).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        let message = error.to_string();
        for expected in [
            &long.as_os_str().len().to_string(),
            &(SUN_PATH_CAPACITY - 1).to_string(),
            &long.display().to_string(),
            &"AGENT_CONSOLE_STATE_DIR".to_owned(),
        ] {
            assert!(
                message.contains(expected.as_str()),
                "the message has to name {expected}: {message}"
            );
        }
    }

    #[test]
    fn shell_capture_is_bounded_and_bracketed_paste_does_not_submit() {
        let capture = "line\n".repeat(10_000);
        let staged = staged_shell_text(Path::new("/tmp/repo"), &capture).unwrap();
        assert!(staged.len() < CAPTURE_BYTES + 200);
        let pasted = bracketed_paste(&staged);
        assert!(pasted.starts_with(b"\x1b[200~"));
        assert!(pasted.ends_with(b"\x1b[201~"));
        assert_ne!(pasted.last(), Some(&b'\n'));
        assert_eq!(plain_text("中文输出\r\n".as_bytes()), "中文输出");
    }

    #[test]
    fn removed_function_and_ctrl_arrow_keys_are_forwarded() {
        for sequence in [
            b"\x1b[17~".as_slice(),
            b"\x1b[18~".as_slice(),
            b"\x1b[19~".as_slice(),
            b"\x1b[20~".as_slice(),
            b"\x1b[24~".as_slice(),
            b"\x1b[1;5A".as_slice(),
            b"\x1b[1;5B".as_slice(),
        ] {
            let mut router = WorkspaceInputRouter::default();
            assert!(matches!(
                router.route(sequence, WorkspaceFocus::Agent).as_slice(),
                [WorkspaceInput::Forward(bytes)] if bytes == sequence
            ));
        }
    }

    #[test]
    fn every_workspace_shortcut_routes_in_its_allowed_focus() {
        let cases: &[(&[u8], WorkspaceCommand, WorkspaceFocus)] = &[
            (
                b"\x1c",
                WorkspaceCommand::ToggleFocus,
                WorkspaceFocus::Agent,
            ),
            (b"\x11", WorkspaceCommand::Dashboard, WorkspaceFocus::Shell),
            (b"a", WorkspaceCommand::Alert, WorkspaceFocus::Sessions),
            (b"\x1e", WorkspaceCommand::NewShell, WorkspaceFocus::Agent),
            (b"\x0e", WorkspaceCommand::NextShell, WorkspaceFocus::Shell),
            (b"\x18", WorkspaceCommand::CloseShell, WorkspaceFocus::Shell),
            (
                b"1",
                WorkspaceCommand::SelectShell(0),
                WorkspaceFocus::Sessions,
            ),
            (
                b"9",
                WorkspaceCommand::SelectShell(8),
                WorkspaceFocus::Sessions,
            ),
            (
                b"m",
                WorkspaceCommand::ToggleMaximize,
                WorkspaceFocus::Sessions,
            ),
            (
                b"h",
                WorkspaceCommand::ToggleShellArea,
                WorkspaceFocus::Sessions,
            ),
            (b"+", WorkspaceCommand::GrowShell, WorkspaceFocus::Sessions),
            (
                b"_",
                WorkspaceCommand::ShrinkShell,
                WorkspaceFocus::Sessions,
            ),
            (
                b"y",
                WorkspaceCommand::CopyCommandBlock,
                WorkspaceFocus::Sessions,
            ),
        ];

        for (sequence, expected, focus) in cases {
            for split in 0..=sequence.len() {
                let mut router = WorkspaceInputRouter::default();
                let mut routed = router.route(&sequence[..split], *focus);
                routed.extend(router.route(&sequence[split..], *focus));
                assert_eq!(routed.len(), 1, "sequence {sequence:?}, split {split}");
                assert!(matches!(
                    routed.first(),
                    Some(WorkspaceInput::Command(command)) if command == expected
                ));
            }
        }
    }

    #[test]
    fn workspace_scroll_shortcuts_route_to_viewport_commands() {
        assert!(matches!(
            workspace_command(b"\x1b[5;2~"),
            Some(WorkspaceCommand::ScrollUp)
        ));
        assert!(matches!(
            workspace_command(b"\x1b[6;2~"),
            Some(WorkspaceCommand::ScrollDown)
        ));
        // Shift-End stays free for Claude Code's selection:extendLineEnd; child
        // input already returns the pane to the live tail.
        assert!(workspace_command(b"\x1b[1;2F").is_none());
    }

    #[test]
    fn workspace_router_uses_configured_bindings_instead_of_defaults() {
        let config = AgentConsoleConfig::parse(
            "[keys.workspace]\nfocus = [\"alt-f\"]\n",
            Path::new("config.toml"),
        )
        .unwrap();
        let mut router = WorkspaceInputRouter {
            pending: Vec::new(),
            bindings: WorkspaceBindings::from_config(&config),
        };

        assert!(matches!(
            router.route(b"\x1bf", WorkspaceFocus::Agent).as_slice(),
            [WorkspaceInput::Command(WorkspaceCommand::ToggleFocus)]
        ));
        assert!(matches!(
            router.route(b"\x0f", WorkspaceFocus::Agent).as_slice(),
            [WorkspaceInput::Forward(bytes)] if bytes == b"\x0f"
        ));
    }

    #[test]
    fn printable_custom_bindings_are_never_stolen_from_a_child() {
        let config = AgentConsoleConfig::parse(
            "[keys.workspace]\nnew_shell = [\"s\"]\n",
            Path::new("config.toml"),
        )
        .unwrap();

        let mut agent_router = WorkspaceInputRouter {
            pending: Vec::new(),
            bindings: WorkspaceBindings::from_config(&config),
        };
        assert!(matches!(
            agent_router.route(b"s", WorkspaceFocus::Agent).as_slice(),
            [WorkspaceInput::Forward(bytes)] if bytes == b"s"
        ));

        let mut session_router = WorkspaceInputRouter {
            pending: Vec::new(),
            bindings: WorkspaceBindings::from_config(&config),
        };
        assert!(matches!(
            session_router
                .route(b"s", WorkspaceFocus::Sessions)
                .as_slice(),
            [WorkspaceInput::Command(WorkspaceCommand::NewShell)]
        ));

        for focus in [WorkspaceFocus::Agent, WorkspaceFocus::Shell] {
            for input in [b"/".as_slice(), b"a".as_slice(), b"?".as_slice()] {
                let mut router = WorkspaceInputRouter::default();
                assert!(matches!(
                    router.route(input, focus).as_slice(),
                    [WorkspaceInput::Forward(bytes)] if bytes == input
                ));
            }
        }
    }

    #[test]
    fn workspace_input_routes_sgr_mouse_wheel_with_coordinates() {
        let mut router = WorkspaceInputRouter::default();
        let routed = router.route(b"\x1b[<64;40;10M", WorkspaceFocus::Agent);

        assert!(matches!(
            routed.as_slice(),
            [WorkspaceInput::Mouse(event)]
                if event.button == 64 && event.col == 40 && event.row == 10 && event.pressed
        ));
    }

    #[test]
    fn workspace_input_routes_legacy_x10_mouse_wheel_with_coordinates() {
        let mut router = WorkspaceInputRouter::default();
        // X10 encodes button, column, and row as single bytes offset by 32.
        let routed = router.route(b"\x1b[M`H*", WorkspaceFocus::Agent);

        assert!(matches!(
            routed.as_slice(),
            [WorkspaceInput::Mouse(event)]
                if event.button == 64 && event.col == 40 && event.row == 10 && event.pressed
        ));
    }

    #[test]
    fn mouse_wheel_scrolls_the_pane_under_the_pointer() {
        let root = tempdir().unwrap();
        let agent = ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path())
                .arg("-c")
                .arg("i=1; while [ $i -le 20 ]; do echo line-$i; i=$((i+1)); done; sleep 2"),
            (30, 5),
        )
        .unwrap();
        agent.wait_for_first_output(Duration::from_secs(1));
        let mut terminals = SessionTerminals {
            agent: Some(Arc::new(agent)),
            ..SessionTerminals::default()
        };
        let layout = WorkspaceLayout::new(120, 30, 0, 0);

        terminals
            .handle_mouse(
                &layout,
                WorkspaceMouseEvent {
                    button: 64,
                    col: layout.agent.left + 1,
                    row: layout.agent.top + 1,
                    pressed: true,
                },
            )
            .unwrap();

        assert_eq!(terminals.agent.as_ref().unwrap().scrollback_offset(), 3);
    }

    #[test]
    fn mouse_wheel_scrolls_codex_style_history_above_a_status_bar() {
        let root = tempdir().unwrap();
        let agent = ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path()).arg("-c").arg(
                "printf 'oldest\\r\\nsecond\\r\\nthird\\r\\nfourth'; \
                 printf '\\033[1;4r\\033[4;1H\\r\\nnewest'; sleep 2",
            ),
            (30, 6),
        )
        .unwrap();
        agent.wait_for_first_output(Duration::from_secs(1));
        let mut terminals = SessionTerminals {
            agent: Some(Arc::new(agent)),
            ..SessionTerminals::default()
        };
        let layout = WorkspaceLayout::new(120, 30, 0, 0);

        terminals
            .handle_mouse(
                &layout,
                WorkspaceMouseEvent {
                    button: 64,
                    col: layout.agent.left + 1,
                    row: layout.agent.top + 1,
                    pressed: true,
                },
            )
            .unwrap();

        let agent = terminals.agent.as_ref().unwrap();
        assert_eq!(agent.scrollback_offset(), 1);
        assert!(
            String::from_utf8_lossy(&agent.screen_view().rows[0]).contains("oldest"),
            "the line removed by Codex's partial scroll region should remain visible"
        );
    }

    #[test]
    fn mouse_wheel_prefers_outer_history_when_agent_requests_mouse_reporting() {
        let root = tempdir().unwrap();
        let agent = ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path()).arg("-c").arg(
                "printf 'oldest\\r\\nsecond\\r\\nthird\\r\\nfourth'; \
                 printf '\\033[1;4r\\033[4;1H\\r\\nnewest'; \
                 printf '\\033[?1000h\\033[?1006h'; sleep 2",
            ),
            (30, 6),
        )
        .unwrap();
        agent.wait_for_first_output(Duration::from_secs(1));
        let start = Instant::now();
        while agent.mouse_protocol().0 == vt100::MouseProtocolMode::None
            && start.elapsed() < Duration::from_secs(1)
        {
            thread::sleep(Duration::from_millis(10));
        }
        let mut terminals = SessionTerminals {
            agent: Some(Arc::new(agent)),
            ..SessionTerminals::default()
        };
        let layout = WorkspaceLayout::new(120, 30, 0, 0);

        terminals
            .handle_mouse(
                &layout,
                WorkspaceMouseEvent {
                    button: 64,
                    col: layout.agent.left + 1,
                    row: layout.agent.top + 1,
                    pressed: true,
                },
            )
            .unwrap();

        assert_eq!(terminals.agent.as_ref().unwrap().scrollback_offset(), 1);
    }

    #[test]
    fn selection_uses_the_displayed_codex_scrollback_viewport() {
        let root = tempdir().unwrap();
        let agent = ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path()).arg("-c").arg(
                "printf 'oldest\\r\\nsecond\\r\\nthird\\r\\nfourth'; \
                 printf '\\033[1;4r\\033[4;1H\\r\\nnewest'; sleep 2",
            ),
            (30, 6),
        )
        .unwrap();
        agent.wait_for_first_output(Duration::from_secs(1));
        assert_eq!(agent.scroll_viewport(1), 1);

        let first = agent
            .selection_cell(TerminalCell { row: 0, col: 0 })
            .unwrap();
        let last = agent
            .selection_cell(TerminalCell { row: 0, col: 5 })
            .unwrap();
        assert_eq!(agent.selected_text(first, last), "oldest");
        assert_eq!(agent.selected_rows(first, last)[0].1, "oldest");
    }

    #[test]
    fn codex_style_scrollback_retains_more_than_two_hundred_lines() {
        let root = tempdir().unwrap();
        let agent = ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path()).arg("-c").arg(
                "printf '\\033[1;4r\\033[4;1H'; \
                 i=1; while [ $i -le 260 ]; do printf 'history-%03d\\r\\n' $i; i=$((i+1)); done; \
                 printf 'live'; sleep 2",
            ),
            (30, 6),
        )
        .unwrap();
        agent.wait_for_first_output(Duration::from_secs(1));

        let retained = agent.scroll_viewport(isize::MAX);
        assert!(
            retained >= 250,
            "expected long session history, retained only {retained} rows"
        );
    }

    #[test]
    fn clicks_outside_terminal_panes_are_ignored_without_coordinate_underflow() {
        let layout = WorkspaceLayout::new(120, 30, 1, 0);

        assert_eq!(pane_at(&layout, 0, 5), None);
        assert_eq!(pane_at(&layout, layout.agent.left + 1, 0), None);
    }

    #[test]
    fn mouse_wheel_is_forwarded_to_an_agent_tui_that_requested_mouse_reporting() {
        let root = tempdir().unwrap();
        let agent = ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path()).arg("-c").arg(
                r"stty raw -echo; printf '\033[?1000h\033[?1006h'; dd bs=1 count=10 2>/dev/null | od -An -tx1; sleep 1",
            ),
            (80, 24),
        )
        .unwrap();
        agent.wait_for_first_output(Duration::from_secs(1));
        let start = Instant::now();
        while agent.mouse_protocol().1 != vt100::MouseProtocolEncoding::Sgr
            && start.elapsed() < Duration::from_secs(1)
        {
            thread::sleep(Duration::from_millis(10));
        }
        let mut terminals = SessionTerminals {
            agent: Some(Arc::new(agent)),
            ..SessionTerminals::default()
        };
        let layout = WorkspaceLayout::new(120, 30, 0, 0);

        terminals
            .handle_mouse(
                &layout,
                WorkspaceMouseEvent {
                    button: 64,
                    col: layout.agent.left + 2,
                    row: 3,
                    pressed: true,
                },
            )
            .unwrap();
        let output = wait_for_capture(
            terminals.agent.as_ref().unwrap(),
            "1b 5b 3c 36 34 3b 32 3b 32 4d",
        );
        let compact = output.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            compact.contains("1b 5b 3c 36 34 3b 32 3b 32 4d"),
            "agent received: {output:?}"
        );
    }

    #[test]
    fn mouse_click_is_forwarded_to_an_agent_tui_that_requested_mouse_reporting() {
        let root = tempdir().unwrap();
        let agent = ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path()).arg("-c").arg(
                r"stty raw -echo; printf '\033[?1000h\033[?1006h'; dd bs=1 count=18 2>/dev/null | od -An -tx1; sleep 1",
            ),
            (80, 24),
        )
        .unwrap();
        agent.wait_for_first_output(Duration::from_secs(1));
        let start = Instant::now();
        while agent.mouse_protocol().1 != vt100::MouseProtocolEncoding::Sgr
            && start.elapsed() < Duration::from_secs(1)
        {
            thread::sleep(Duration::from_millis(10));
        }
        let mut terminals = SessionTerminals {
            agent: Some(Arc::new(agent)),
            ..SessionTerminals::default()
        };
        let layout = WorkspaceLayout::new(120, 30, 0, 0);

        terminals
            .handle_mouse(
                &layout,
                WorkspaceMouseEvent {
                    button: 0,
                    col: layout.agent.left + 2,
                    row: 3,
                    pressed: true,
                },
            )
            .unwrap();
        terminals
            .handle_mouse(
                &layout,
                WorkspaceMouseEvent {
                    button: 0,
                    col: layout.agent.left + 2,
                    row: 3,
                    pressed: false,
                },
            )
            .unwrap();
        let output = wait_for_capture(
            terminals.agent.as_ref().unwrap(),
            "1b 5b 3c 30 3b 32 3b 32 4d 1b 5b 3c 30 3b 32 3b 32 6d",
        );
        let compact = output.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            compact.contains("1b 5b 3c 30 3b 32 3b 32 4d 1b 5b 3c 30 3b 32 3b 32 6d"),
            "agent received: {output:?}"
        );
    }

    #[test]
    fn ordinary_drag_selects_text_even_when_agent_requested_mouse_reporting() {
        let root = tempdir().unwrap();
        let agent = ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path())
                .arg("-c")
                .arg("printf 'select-me\\033[?1002h\\033[?1006h'; sleep 2"),
            (20, 3),
        )
        .unwrap();
        agent.wait_for_first_output(Duration::from_secs(1));
        let start = Instant::now();
        while agent.mouse_protocol().0 == vt100::MouseProtocolMode::None
            && start.elapsed() < Duration::from_secs(1)
        {
            thread::sleep(Duration::from_millis(10));
        }
        let mut terminals = SessionTerminals {
            agent: Some(Arc::new(agent)),
            ..SessionTerminals::default()
        };
        let layout = WorkspaceLayout::new(120, 30, 0, 0);

        terminals
            .handle_mouse(
                &layout,
                WorkspaceMouseEvent {
                    button: 0,
                    col: layout.agent.left + 1,
                    row: layout.agent.top + 1,
                    pressed: true,
                },
            )
            .unwrap();
        terminals
            .handle_mouse(
                &layout,
                WorkspaceMouseEvent {
                    button: 32,
                    col: layout.agent.left + 6,
                    row: layout.agent.top + 1,
                    pressed: true,
                },
            )
            .unwrap();

        assert_eq!(terminals.selected_text().as_deref(), Some("select"));
    }

    #[test]
    fn mouse_wheel_uses_alternate_scroll_for_an_agent_tui_without_mouse_reporting() {
        let root = tempdir().unwrap();
        let agent = ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path()).arg("-c").arg(
                r"stty raw -echo; printf '\033[?1049h'; dd bs=1 count=9 2>/dev/null | od -An -tx1; sleep 1",
            ),
            (80, 24),
        )
        .unwrap();
        agent.wait_for_first_output(Duration::from_secs(1));
        let mut terminals = SessionTerminals {
            agent: Some(Arc::new(agent)),
            ..SessionTerminals::default()
        };
        let layout = WorkspaceLayout::new(120, 30, 0, 0);

        terminals
            .handle_mouse(
                &layout,
                WorkspaceMouseEvent {
                    button: 64,
                    col: layout.agent.left + 2,
                    row: 3,
                    pressed: true,
                },
            )
            .unwrap();
        let output = wait_for_capture(
            terminals.agent.as_ref().unwrap(),
            "1b 5b 41 1b 5b 41 1b 5b 41",
        );
        let compact = output.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            compact.contains("1b 5b 41 1b 5b 41 1b 5b 41"),
            "agent received: {output:?}"
        );
    }

    #[test]
    fn terminal_selection_extracts_visible_cells_across_rows() {
        let root = tempdir().unwrap();
        let terminal = ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path())
                .arg("-c")
                .arg("printf 'alpha beta\\ngamma delta\\n'; sleep 2"),
            (20, 3),
        )
        .unwrap();
        terminal.wait_for_first_output(Duration::from_secs(1));

        let selected = terminal.selected_text(
            terminal
                .selection_cell(TerminalCell { row: 0, col: 6 })
                .unwrap(),
            terminal
                .selection_cell(TerminalCell { row: 1, col: 4 })
                .unwrap(),
        );

        assert_eq!(selected, "beta\ngamma");
    }

    #[test]
    fn terminal_selection_keeps_its_history_anchor_across_viewport_scrolling() {
        let mut parser = vt100::Parser::new(3, 12, 20);
        let mut scrollback = StatusBarScrollback::default();
        process_terminal_output(
            &mut parser,
            &mut scrollback,
            b"line-1\r\nline-2\r\nline-3\r\nline-4\r\nline-5",
        );
        parser.screen_mut().set_scrollback(usize::MAX);
        let start = terminal_buffer_cell(&mut parser, &scrollback, TerminalCell { row: 0, col: 0 })
            .unwrap();
        parser.screen_mut().set_scrollback(0);
        let end = terminal_buffer_cell(&mut parser, &scrollback, TerminalCell { row: 2, col: 5 })
            .unwrap();

        assert_eq!(
            terminal_selected_text(&mut parser, &scrollback, start, end),
            "line-1\nline-2\nline-3\nline-4\nline-5"
        );
        assert_eq!(
            terminal_selected_rows(&mut parser, &scrollback, start, end)
                .into_iter()
                .map(|(cell, text)| (cell.row, text.trim_end().to_owned()))
                .collect::<Vec<_>>(),
            vec![
                (0, "line-3".into()),
                (1, "line-4".into()),
                (2, "line-5".into()),
            ]
        );
    }

    #[test]
    fn terminal_selection_collects_more_than_two_native_viewports() {
        let mut parser = vt100::Parser::new(3, 12, 20);
        let scrollback = StatusBarScrollback::default();
        let output = (1..=11)
            .map(|line| format!("line-{line:02}"))
            .collect::<Vec<_>>()
            .join("\r\n");
        parser.process(output.as_bytes());
        parser.screen_mut().set_scrollback(usize::MAX);
        let start = terminal_buffer_cell(&mut parser, &scrollback, TerminalCell { row: 0, col: 0 })
            .unwrap();
        parser.screen_mut().set_scrollback(0);
        let end = terminal_buffer_cell(&mut parser, &scrollback, TerminalCell { row: 2, col: 6 })
            .unwrap();

        assert_eq!(
            terminal_selected_text(&mut parser, &scrollback, start, end),
            (1..=11)
                .map(|line| format!("line-{line:02}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn terminal_selection_spans_codex_status_bar_scrollback() {
        let mut parser = vt100::Parser::new(3, 12, 20);
        parser.process(b"line-4\r\nline-5\r\nline-6");
        let mut scrollback = StatusBarScrollback::default();
        scrollback
            .rows
            .extend([b"line-1".to_vec(), b"line-2".to_vec(), b"line-3".to_vec()]);
        scrollback.offset = 3;
        let start = terminal_buffer_cell(&mut parser, &scrollback, TerminalCell { row: 0, col: 0 })
            .unwrap();
        scrollback.offset = 0;
        let end = terminal_buffer_cell(&mut parser, &scrollback, TerminalCell { row: 2, col: 5 })
            .unwrap();

        assert_eq!(
            terminal_selected_text(&mut parser, &scrollback, start, end),
            "line-1\nline-2\nline-3\nline-4\nline-5\nline-6"
        );
    }

    #[test]
    fn mouse_wheel_extends_an_active_selection_across_history() {
        let root = tempdir().unwrap();
        let agent = ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path())
                .arg("-c")
                .arg("printf 'line-1\r\nline-2\r\nline-3\r\nline-4\r\nline-5\r\nline-6'; sleep 2"),
            (12, 3),
        )
        .unwrap();
        agent.wait_for_first_output(Duration::from_secs(1));
        let mut terminals = SessionTerminals {
            agent: Some(Arc::new(agent)),
            ..SessionTerminals::default()
        };
        let layout = WorkspaceLayout::new(120, 30, 0, 0);

        terminals
            .handle_mouse(
                &layout,
                WorkspaceMouseEvent {
                    button: 0,
                    col: layout.agent.left + 6,
                    row: layout.agent.top + 3,
                    pressed: true,
                },
            )
            .unwrap();
        terminals
            .handle_mouse(
                &layout,
                WorkspaceMouseEvent {
                    button: 64,
                    col: layout.agent.left + 1,
                    row: layout.agent.top + 1,
                    pressed: true,
                },
            )
            .unwrap();

        assert_eq!(
            terminals.selected_text().as_deref(),
            Some("line-1\nline-2\nline-3\nline-4\nline-5\nline-6")
        );
    }

    #[test]
    fn dragging_past_the_pane_edge_scrolls_and_keeps_selecting() {
        let root = tempdir().unwrap();
        let agent = ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path())
                .arg("-c")
                .arg("printf 'line-1\r\nline-2\r\nline-3\r\nline-4\r\nline-5\r\nline-6'; sleep 2"),
            (12, 3),
        )
        .unwrap();
        agent.wait_for_first_output(Duration::from_secs(1));
        let mut terminals = SessionTerminals {
            agent: Some(Arc::new(agent)),
            ..SessionTerminals::default()
        };
        let layout = WorkspaceLayout::new(120, 30, 0, 0);

        for event in [
            WorkspaceMouseEvent {
                button: 0,
                col: layout.agent.left + 6,
                row: layout.agent.top + 3,
                pressed: true,
            },
            WorkspaceMouseEvent {
                button: 32,
                col: layout.agent.left + 1,
                row: layout.agent.top,
                pressed: true,
            },
        ] {
            terminals.handle_mouse(&layout, event).unwrap();
        }

        assert_eq!(terminals.agent.as_ref().unwrap().scrollback_offset(), 3);
        assert_eq!(
            terminals.selected_text().as_deref(),
            Some("line-1\nline-2\nline-3\nline-4\nline-5\nline-6")
        );
    }

    #[test]
    fn mouse_reporting_keeps_the_pending_selection_anchor_while_scrolling() {
        let root = tempdir().unwrap();
        let agent = ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path()).arg("-c").arg(
                r"printf 'line-1\r\nline-2\r\nline-3\r\nline-4\r\nline-5\r\nline-6'; \
                  printf '\033[?1002h\033[?1006h'; sleep 2",
            ),
            (12, 3),
        )
        .unwrap();
        agent.wait_for_first_output(Duration::from_secs(1));
        let start = Instant::now();
        while agent.mouse_protocol().0 == vt100::MouseProtocolMode::None
            && start.elapsed() < Duration::from_secs(1)
        {
            thread::sleep(Duration::from_millis(10));
        }
        let mut terminals = SessionTerminals {
            agent: Some(Arc::new(agent)),
            ..SessionTerminals::default()
        };
        let layout = WorkspaceLayout::new(120, 30, 0, 0);

        for event in [
            WorkspaceMouseEvent {
                button: 0,
                col: layout.agent.left + 6,
                row: layout.agent.top + 3,
                pressed: true,
            },
            WorkspaceMouseEvent {
                button: 64,
                col: layout.agent.left + 1,
                row: layout.agent.top + 1,
                pressed: true,
            },
            WorkspaceMouseEvent {
                button: 32,
                col: layout.agent.left + 1,
                row: layout.agent.top + 1,
                pressed: true,
            },
        ] {
            terminals.handle_mouse(&layout, event).unwrap();
        }

        assert_eq!(
            terminals.selected_text().as_deref(),
            Some("line-1\nline-2\nline-3\nline-4\nline-5\nline-6")
        );
    }

    #[test]
    fn selection_uses_the_claude_style_alternate_screen_viewport() {
        let root = tempdir().unwrap();
        let terminal = ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path())
                .arg("-c")
                .arg("printf '\\033[?1049hclaude-old\\033[2J\\033[Hclaude-visible'; sleep 2"),
            (24, 4),
        )
        .unwrap();
        terminal.wait_for_first_output(Duration::from_secs(1));

        assert!(terminal.alternate_screen());
        assert_eq!(terminal.mouse_protocol().0, vt100::MouseProtocolMode::None);
        assert_eq!(
            terminal.selected_text(
                terminal
                    .selection_cell(TerminalCell { row: 0, col: 0 })
                    .unwrap(),
                terminal
                    .selection_cell(TerminalCell { row: 0, col: 13 })
                    .unwrap(),
            ),
            "claude-visible"
        );
    }

    #[test]
    fn alternate_screen_selection_keeps_pages_repainted_by_claude() {
        let root = tempdir().unwrap();
        let agent = ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path()).arg("-c").arg(
                r"stty raw -echo; \
                  printf '\033[?1049hline-4\r\nline-5\r\nline-6'; \
                  dd bs=1 count=9 of=/dev/null 2>/dev/null; \
                  printf '\033[?25l'; sleep 0.08; \
                  printf '\033[2J\033[Hline-1\r\nline-2\r\nline-3'; sleep 2",
            ),
            (12, 3),
        )
        .unwrap();
        agent.wait_for_first_output(Duration::from_secs(1));
        let mut terminals = SessionTerminals {
            agent: Some(Arc::new(agent)),
            ..SessionTerminals::default()
        };
        let layout = WorkspaceLayout::new(120, 30, 0, 0);

        terminals
            .handle_mouse(
                &layout,
                WorkspaceMouseEvent {
                    button: 0,
                    col: layout.agent.left + 6,
                    row: layout.agent.top + 3,
                    pressed: true,
                },
            )
            .unwrap();
        terminals
            .handle_mouse(
                &layout,
                WorkspaceMouseEvent {
                    button: 64,
                    col: layout.agent.left + 1,
                    row: layout.agent.top + 1,
                    pressed: true,
                },
            )
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            terminals.refresh_alternate_selection().unwrap();
            if terminals
                .alternate_selection
                .as_ref()
                .is_some_and(|capture| capture.pending_scroll.is_none())
            {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            terminals
                .alternate_selection
                .as_ref()
                .is_some_and(|capture| capture.pending_scroll.is_none()),
            "the same-view cursor update must not consume the pending page capture"
        );
        terminals
            .handle_mouse(
                &layout,
                WorkspaceMouseEvent {
                    button: 32,
                    col: layout.agent.left + 1,
                    row: layout.agent.top + 1,
                    pressed: true,
                },
            )
            .unwrap();

        assert_eq!(
            terminals.selected_text().as_deref(),
            Some("line-1\nline-2\nline-3\nline-4\nline-5\nline-6")
        );
    }

    #[test]
    fn alternate_scroll_timeout_releases_a_boundary_and_sends_the_queued_reverse() {
        let root = tempdir().unwrap();
        let agent = ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path())
                .arg("-c")
                .arg(r"stty raw -echo; printf '\033[?1049hstatic'; dd bs=1 count=2 of=/dev/null 2>/dev/null; sleep 2"),
            (12, 3),
        )
        .unwrap();
        agent.wait_for_first_output(Duration::from_secs(1));
        let mut terminals = SessionTerminals {
            agent: Some(Arc::new(agent)),
            ..SessionTerminals::default()
        };
        terminals
            .begin_selection(PaneTarget::Agent, TerminalCell { row: 0, col: 0 })
            .unwrap();
        terminals
            .queue_alternate_scroll(
                PaneTarget::Agent,
                AlternateScrollDirection::Older,
                b"u".to_vec(),
            )
            .unwrap();
        terminals
            .queue_alternate_scroll(
                PaneTarget::Agent,
                AlternateScrollDirection::Newer,
                b"d".to_vec(),
            )
            .unwrap();
        terminals
            .alternate_selection
            .as_mut()
            .unwrap()
            .pending_scroll
            .as_mut()
            .unwrap()
            .started_at = Instant::now() - ALTERNATE_SCROLL_TIMEOUT;

        assert!(terminals.refresh_alternate_selection().unwrap());
        let capture = terminals.alternate_selection.as_ref().unwrap();
        assert_eq!(capture.queued_scrolls.len(), 0);
        assert_eq!(
            capture
                .pending_scroll
                .as_ref()
                .map(|pending| pending.direction),
            Some(AlternateScrollDirection::Newer)
        );

        terminals
            .alternate_selection
            .as_mut()
            .unwrap()
            .pending_scroll
            .as_mut()
            .unwrap()
            .started_at = Instant::now() - ALTERNATE_SCROLL_TIMEOUT;
        assert!(terminals.refresh_alternate_selection().unwrap());
        assert!(
            terminals
                .alternate_selection
                .as_ref()
                .unwrap()
                .pending_scroll
                .is_none()
        );

        terminals
            .queue_alternate_scroll(
                PaneTarget::Agent,
                AlternateScrollDirection::Older,
                b"x".to_vec(),
            )
            .unwrap();
        let generation = terminals.agent.as_ref().unwrap().output_generation();
        let pending = terminals
            .alternate_selection
            .as_mut()
            .unwrap()
            .pending_scroll
            .as_mut()
            .unwrap();
        pending.started_at = Instant::now() - ALTERNATE_SCROLL_TIMEOUT;
        pending.candidate = Some(AlternateViewCandidate {
            generation,
            stable_since: Instant::now(),
            view: ScreenView {
                rows: ["candidate-1", "candidate-2", "candidate-3"]
                    .map(|line| line.as_bytes().to_vec())
                    .to_vec(),
                size: (3, 12),
                cursor: (0, 0),
                hide_cursor: false,
            },
        });
        assert!(terminals.refresh_alternate_selection().unwrap());
        assert!(
            alternate_row_keys(&terminals.alternate_selection.as_ref().unwrap().rows, 12,)
                .iter()
                .any(|row| row == "candidate-1"),
            "an overall timeout must commit the latest candidate, even before its quiet window"
        );
    }

    #[test]
    fn rapid_alternate_wheels_wait_for_each_repaint() {
        let root = tempdir().unwrap();
        let agent = ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path()).arg("-c").arg(
                r"stty raw -echo; printf '\033[?1049hstatic'; dd bs=1 count=18 of=/dev/null 2>/dev/null; sleep 2",
            ),
            (12, 3),
        )
        .unwrap();
        agent.wait_for_first_output(Duration::from_secs(1));
        let mut terminals = SessionTerminals {
            agent: Some(Arc::new(agent)),
            ..SessionTerminals::default()
        };
        let layout = WorkspaceLayout::new(120, 30, 0, 0);
        for event in [
            WorkspaceMouseEvent {
                button: 0,
                col: layout.agent.left + 1,
                row: layout.agent.top + 1,
                pressed: true,
            },
            WorkspaceMouseEvent {
                button: 64,
                col: layout.agent.left + 1,
                row: layout.agent.top + 1,
                pressed: true,
            },
            WorkspaceMouseEvent {
                button: 64,
                col: layout.agent.left + 1,
                row: layout.agent.top + 1,
                pressed: true,
            },
        ] {
            terminals.handle_mouse(&layout, event).unwrap();
        }
        let capture = terminals.alternate_selection.as_ref().unwrap();
        assert_eq!(capture.queued_scrolls.len(), 1);
        assert_eq!(
            capture
                .pending_scroll
                .as_ref()
                .map(|pending| pending.direction),
            Some(AlternateScrollDirection::Older)
        );
        terminals
            .handle_mouse(
                &layout,
                WorkspaceMouseEvent {
                    button: 0,
                    col: layout.agent.left + 1,
                    row: layout.agent.top + 1,
                    pressed: false,
                },
            )
            .unwrap();
        assert_eq!(
            terminals
                .alternate_selection
                .as_ref()
                .unwrap()
                .queued_scrolls
                .len(),
            0,
            "release must cancel scrolls that have not been sent to the child"
        );
        assert!(terminals.pending_alternate_copy.is_some());

        terminals
            .handle_mouse(
                &layout,
                WorkspaceMouseEvent {
                    button: 65,
                    col: layout.agent.left + 1,
                    row: layout.agent.top + 1,
                    pressed: true,
                },
            )
            .unwrap();
        let capture = terminals.alternate_selection.as_ref().unwrap();
        assert_eq!(capture.queued_scrolls.len(), 0);
        assert_eq!(
            capture
                .pending_scroll
                .as_ref()
                .map(|pending| pending.direction),
            Some(AlternateScrollDirection::Older),
            "paging after release must not change the range waiting to be copied"
        );

        terminals
            .handle_mouse(
                &layout,
                WorkspaceMouseEvent {
                    button: 0,
                    col: layout.agent.left + 2,
                    row: layout.agent.top + 2,
                    pressed: true,
                },
            )
            .unwrap();
        assert!(terminals.pending_alternate_copy.is_some());
        assert!(
            terminals
                .alternate_selection
                .as_ref()
                .is_some_and(|capture| capture.pending_scroll.is_some()),
            "a new click must not cancel a release that is still waiting to copy"
        );

        let selection = terminals.selection;
        terminals.pending_alternate_copy = None;
        terminals
            .alternate_selection
            .as_mut()
            .unwrap()
            .pending_scroll = None;
        terminals
            .handle_mouse(
                &layout,
                WorkspaceMouseEvent {
                    button: 0,
                    col: layout.agent.left + 2,
                    row: layout.agent.top + 2,
                    pressed: false,
                },
            )
            .unwrap();
        assert_eq!(
            terminals.selection, selection,
            "the release paired with a suppressed press must also be ignored"
        );

        for button in [1, 2] {
            let generation = terminals.agent.as_ref().unwrap().output_generation();
            terminals.pending_alternate_copy = Some(PendingAlternateCopy {
                pane: PaneTarget::Agent,
                cell: TerminalCell { row: 0, col: 0 },
            });
            terminals
                .alternate_selection
                .as_mut()
                .unwrap()
                .pending_scroll = Some(PendingAlternateScroll {
                direction: AlternateScrollDirection::Older,
                generation,
                started_at: Instant::now(),
                candidate: None,
            });
            terminals
                .handle_mouse(
                    &layout,
                    WorkspaceMouseEvent {
                        button,
                        col: layout.agent.left + 1,
                        row: layout.agent.top + 1,
                        pressed: true,
                    },
                )
                .unwrap();
            assert_eq!(terminals.suppressed_mouse_buttons, 1_u8 << button);
            terminals.pending_alternate_copy = None;
            terminals
                .alternate_selection
                .as_mut()
                .unwrap()
                .pending_scroll = None;
            terminals
                .handle_mouse(
                    &layout,
                    WorkspaceMouseEvent {
                        button,
                        col: layout.agent.left + 1,
                        row: layout.agent.top + 1,
                        pressed: false,
                    },
                )
                .unwrap();
            assert_eq!(terminals.suppressed_mouse_buttons, 0);
        }

        let generation = terminals.agent.as_ref().unwrap().output_generation();
        terminals.pending_alternate_copy = Some(PendingAlternateCopy {
            pane: PaneTarget::Agent,
            cell: TerminalCell { row: 0, col: 0 },
        });
        terminals
            .alternate_selection
            .as_mut()
            .unwrap()
            .pending_scroll = Some(PendingAlternateScroll {
            direction: AlternateScrollDirection::Older,
            generation,
            started_at: Instant::now(),
            candidate: None,
        });
        for button in [2, 0] {
            terminals
                .handle_mouse(
                    &layout,
                    WorkspaceMouseEvent {
                        button,
                        col: layout.agent.left + 1,
                        row: layout.agent.top + 1,
                        pressed: true,
                    },
                )
                .unwrap();
        }
        assert_eq!(terminals.suppressed_mouse_buttons, 0b101);
        terminals.pending_alternate_copy = None;
        terminals
            .alternate_selection
            .as_mut()
            .unwrap()
            .pending_scroll = None;
        for (button, remaining) in [(2, 0b001), (0, 0)] {
            terminals
                .handle_mouse(
                    &layout,
                    WorkspaceMouseEvent {
                        button,
                        col: layout.agent.left + 1,
                        row: layout.agent.top + 1,
                        pressed: false,
                    },
                )
                .unwrap();
            assert_eq!(terminals.suppressed_mouse_buttons, remaining);
        }
    }

    #[test]
    fn alternate_selection_merges_overlapping_repaints_without_duplicates() {
        let page = |lines: &[&str]| ScreenView {
            rows: lines.iter().map(|line| line.as_bytes().to_vec()).collect(),
            size: (3, 8),
            cursor: (0, 0),
            hide_cursor: false,
        };
        let mut capture =
            AlternateSelectionBuffer::new(PaneTarget::Agent, page(&["line-3", "line-4", "line-5"]))
                .unwrap();
        let mut anchor = capture.cell(TerminalCell { row: 2, col: 5 });
        assert_eq!(
            capture.queue_scroll(
                AlternateScrollRequest {
                    direction: AlternateScrollDirection::Older,
                    bytes: Vec::new(),
                },
                1,
            ),
            Some(Vec::new())
        );
        let edit = capture
            .merge_view(page(&["line-1", "line-2", "line-3"]))
            .unwrap();
        anchor.row = edit.old_positions[anchor.row];
        let oldest = capture.cell(TerminalCell { row: 0, col: 0 });

        assert_eq!(
            capture.selected_text(anchor, oldest),
            "line-1\nline-2\nline-3\nline-4\nline-5"
        );

        assert_eq!(
            capture.queue_scroll(
                AlternateScrollRequest {
                    direction: AlternateScrollDirection::Newer,
                    bytes: Vec::new(),
                },
                2,
            ),
            Some(Vec::new())
        );
        capture
            .merge_view(page(&["line-3", "line-4", "line-5"]))
            .unwrap();
        assert_eq!(capture.viewport_positions, [2, 3, 4]);
        assert_eq!(capture.rows.len(), 5);

        assert_eq!(
            capture.queue_scroll(
                AlternateScrollRequest {
                    direction: AlternateScrollDirection::Older,
                    bytes: Vec::new(),
                },
                3,
            ),
            Some(Vec::new())
        );
        capture
            .merge_view(page(&["line-1", "line-2", "line-3"]))
            .unwrap();
        assert_eq!(capture.viewport_positions, [0, 1, 2]);

        assert_eq!(
            capture.queue_scroll(
                AlternateScrollRequest {
                    direction: AlternateScrollDirection::Newer,
                    bytes: Vec::new(),
                },
                4,
            ),
            Some(Vec::new())
        );
        capture
            .merge_view(page(&["line-4", "line-5", "line-6"]))
            .unwrap();
        assert_eq!(capture.viewport_positions, [3, 4, 5]);
        assert_eq!(
            alternate_row_keys(&capture.rows, capture.cols),
            ["line-1", "line-2", "line-3", "line-4", "line-5", "line-6"]
        );
    }

    #[test]
    fn alternate_selection_aligns_fixed_chrome_and_distant_captured_rows() {
        let page = |lines: &[&str]| ScreenView {
            rows: lines.iter().map(|line| line.as_bytes().to_vec()).collect(),
            size: (lines.len() as u16, 16),
            cursor: (0, 0),
            hide_cursor: false,
        };
        let mut capture = AlternateSelectionBuffer::new(
            PaneTarget::Agent,
            page(&["HEADER", "line-4", "line-5", "line-6", "STATUS"]),
        )
        .unwrap();
        capture.queue_scroll(
            AlternateScrollRequest {
                direction: AlternateScrollDirection::Older,
                bytes: Vec::new(),
            },
            1,
        );
        capture
            .merge_view(page(&["HEADER", "line-1", "line-2", "line-4", "STATUS"]))
            .unwrap();
        assert_eq!(
            alternate_row_keys(&capture.rows, capture.cols),
            [
                "HEADER", "line-1", "line-2", "line-4", "line-5", "line-6", "STATUS"
            ]
        );
        assert_eq!(capture.viewport_positions, [0, 1, 2, 3, 6]);

        let mut capture =
            AlternateSelectionBuffer::new(PaneTarget::Agent, page(&["line-0", "line-1", "line-2"]))
                .unwrap();
        capture.rows = (0..9)
            .map(|line| format!("line-{line}").into_bytes())
            .collect();
        capture.viewport_positions = vec![0, 1, 2];
        capture.queue_scroll(
            AlternateScrollRequest {
                direction: AlternateScrollDirection::Newer,
                bytes: Vec::new(),
            },
            2,
        );
        capture
            .merge_view(page(&["line-7", "line-8", "line-9"]))
            .unwrap();
        assert_eq!(
            alternate_row_keys(&capture.rows, capture.cols),
            (0..10)
                .map(|line| format!("line-{line}"))
                .collect::<Vec<_>>()
        );
        assert_eq!(capture.viewport_positions, [7, 8, 9]);

        let mut capture =
            AlternateSelectionBuffer::new(PaneTarget::Agent, page(&["line-7", "line-8", "line-9"]))
                .unwrap();
        capture.rows = (1..10)
            .map(|line| format!("line-{line}").into_bytes())
            .collect();
        capture.viewport_positions = vec![6, 7, 8];
        capture.queue_scroll(
            AlternateScrollRequest {
                direction: AlternateScrollDirection::Older,
                bytes: Vec::new(),
            },
            3,
        );
        capture
            .merge_view(page(&["line-0", "line-1", "line-2"]))
            .unwrap();
        assert_eq!(
            alternate_row_keys(&capture.rows, capture.cols),
            (0..10)
                .map(|line| format!("line-{line}"))
                .collect::<Vec<_>>()
        );
        assert_eq!(capture.viewport_positions, [0, 1, 2]);
    }

    #[test]
    fn alternate_selection_keeps_repeated_content_and_replaces_dynamic_chrome() {
        let page = |lines: &[&str]| ScreenView {
            rows: lines.iter().map(|line| line.as_bytes().to_vec()).collect(),
            size: (lines.len() as u16, 16),
            cursor: (0, 0),
            hide_cursor: false,
        };
        let merge = |old: &[&str], new: &[&str], direction| {
            let mut capture = AlternateSelectionBuffer::new(PaneTarget::Agent, page(old)).unwrap();
            capture.queue_scroll(
                AlternateScrollRequest {
                    direction,
                    bytes: Vec::new(),
                },
                1,
            );
            capture.merge_view(page(new)).unwrap();
            alternate_row_keys(&capture.rows, capture.cols)
        };

        assert_eq!(
            merge(
                &["H", "old-1", "", "old-2", "S"],
                &["H", "new-1", "", "new-2", "S"],
                AlternateScrollDirection::Older,
            ),
            ["H", "new-1", "", "new-2", "old-1", "", "old-2", "S"]
        );
        assert_eq!(
            merge(
                &["H", "A", "B", "A", "S"],
                &["H", "A", "B", "C", "S"],
                AlternateScrollDirection::Newer,
            ),
            ["H", "A", "B", "A", "B", "C", "S"]
        );
        assert_eq!(
            merge(
                &["H", "A", "B", "C", "STATUS-1"],
                &["H", "X", "Y", "A", "STATUS-2"],
                AlternateScrollDirection::Older,
            ),
            ["H", "X", "Y", "A", "B", "C", "STATUS-2"]
        );
        assert_eq!(
            merge(
                &["H", "X", "Y", "A", "STATUS-1"],
                &["H", "A", "B", "C", "STATUS-2"],
                AlternateScrollDirection::Newer,
            ),
            ["H", "X", "Y", "A", "B", "C", "STATUS-2"]
        );
        assert_eq!(
            merge(
                &["H1", "H2", "line-4", "line-5", "S1", "S2"],
                &["H1", "H2", "line-2", "line-3", "S1", "S2"],
                AlternateScrollDirection::Older,
            ),
            [
                "H1", "H2", "line-2", "line-3", "line-4", "line-5", "S1", "S2"
            ]
        );
        assert_eq!(
            merge(
                &["H1", "H2", "line-4", "line-5", "line-6", "S1-old", "S2-old",],
                &["H1", "H2", "line-2", "line-3", "line-4", "S1-new", "S2-new",],
                AlternateScrollDirection::Older,
            ),
            [
                "H1", "H2", "line-2", "line-3", "line-4", "line-5", "line-6", "S1-new", "S2-new"
            ]
        );
        assert_eq!(
            merge(
                &["H1-old", "H2-old", "line-2", "line-3", "line-4", "S1", "S2",],
                &["H1-new", "H2-new", "line-4", "line-5", "line-6", "S1", "S2",],
                AlternateScrollDirection::Newer,
            ),
            [
                "H1-new", "H2-new", "line-2", "line-3", "line-4", "line-5", "line-6", "S1", "S2"
            ]
        );

        let mut capture = AlternateSelectionBuffer::new(
            PaneTarget::Agent,
            page(&["H", "A", "B", "C", "STATUS-1"]),
        )
        .unwrap();
        capture.queue_scroll(
            AlternateScrollRequest {
                direction: AlternateScrollDirection::Older,
                bytes: Vec::new(),
            },
            2,
        );
        assert!(capture.replace_chrome_update(
            page(&["H", "A", "B", "C", "STATUS-2"]),
            AlternateScrollDirection::Older,
        ));
        assert!(capture.pending_scroll.is_some());
        capture
            .merge_view(page(&["H", "X", "Y", "A", "STATUS-3"]))
            .unwrap();
        assert_eq!(
            alternate_row_keys(&capture.rows, capture.cols),
            ["H", "X", "Y", "A", "B", "C", "STATUS-3"]
        );

        let mut capture =
            AlternateSelectionBuffer::new(PaneTarget::Agent, page(&["H", "A", "B", "C", "S1"]))
                .unwrap();
        capture.chrome_prefix = 1;
        capture.chrome_suffix = 1;
        let mut anchor = capture.cell(TerminalCell { row: 1, col: 0 });
        capture.queue_scroll(
            AlternateScrollRequest {
                direction: AlternateScrollDirection::Older,
                bytes: Vec::new(),
            },
            3,
        );
        assert!(capture.replace_chrome_update(
            page(&["H", "A", "B", "C", "S2"]),
            AlternateScrollDirection::Older,
        ));
        assert_eq!((capture.chrome_prefix, capture.chrome_suffix), (1, 1));
        let edit = capture
            .merge_view(page(&["H", "W", "X", "Y", "S3"]))
            .unwrap();
        anchor.row = edit.old_positions[anchor.row];
        assert_eq!(
            alternate_row_keys(&capture.rows, capture.cols),
            ["H", "W", "X", "Y", "A", "B", "C", "S3"]
        );
        assert_eq!(
            selected_row_text(capture.rows.get(anchor.row).map(Vec::as_slice), 16, 0, 0),
            "A"
        );

        assert_eq!(
            merge(
                &["H1", "H2", "A", "B", "C", "D", "S-old"],
                &["H1", "H2", "W", "X", "Y", "Z", "S-new"],
                AlternateScrollDirection::Older,
            ),
            [
                "H1", "H2", "W", "X", "Y", "Z", "S-new", "H1", "H2", "A", "B", "C", "D", "S-old"
            ],
            "without overlap evidence, preserving every body row is safer than guessing chrome"
        );
    }

    #[test]
    fn alternate_older_overlap_prefers_the_nearest_captured_occurrence() {
        let rows = |lines: &[&str]| {
            lines
                .iter()
                .map(|line| line.as_bytes().to_vec())
                .collect::<Vec<_>>()
        };
        let merged = directional_content_merge(
            &rows(&["A", "B", "Q", "A", "B", "C", "D", "E"]),
            &[5, 6, 7],
            &rows(&["Z", "A", "B"]),
            8,
            AlternateScrollDirection::Older,
        );

        assert_eq!(
            alternate_row_keys(&merged.rows, 8),
            ["A", "B", "Q", "Z", "A", "B", "C", "D", "E"]
        );
        assert_eq!(merged.new_positions, [3, 4, 5]);
    }

    #[test]
    fn alternate_selection_serializes_scrolls_and_bounds_captured_rows() {
        let page = |first: usize| ScreenView {
            rows: (first..first + 3)
                .map(|line| format!("line-{line:04}").into_bytes())
                .collect(),
            size: (3, 16),
            cursor: (0, 0),
            hide_cursor: false,
        };
        let mut capture = AlternateSelectionBuffer::new(PaneTarget::Agent, page(1_997)).unwrap();
        capture.rows = (0..SCROLLBACK_LINES)
            .map(|line| format!("line-{line:04}").into_bytes())
            .collect();
        capture.viewport_rows = page(1_997).rows;
        capture.viewport_positions = vec![1_997, 1_998, 1_999];

        assert_eq!(
            capture.queue_scroll(
                AlternateScrollRequest {
                    direction: AlternateScrollDirection::Newer,
                    bytes: b"first".to_vec(),
                },
                10,
            ),
            Some(b"first".to_vec())
        );
        assert_eq!(
            capture.queue_scroll(
                AlternateScrollRequest {
                    direction: AlternateScrollDirection::Newer,
                    bytes: b"second".to_vec(),
                },
                10,
            ),
            None
        );

        let edit = capture.merge_view(page(2_000)).unwrap();
        assert_eq!(edit.old_positions[0], 0);
        assert_eq!(capture.rows.len(), SCROLLBACK_LINES);
        assert_eq!(capture.viewport_positions, [1_997, 1_998, 1_999]);
        assert_eq!(
            alternate_row_keys(&capture.rows[..1], capture.cols),
            ["line-0003"]
        );
        assert_eq!(
            alternate_row_keys(&capture.rows[SCROLLBACK_LINES - 1..], capture.cols),
            ["line-2002"]
        );
        assert_eq!(capture.begin_next_scroll(11), Some(b"second".to_vec()));
        assert_eq!(capture.queued_scrolls.len(), 0);
        assert_eq!(
            capture
                .pending_scroll
                .as_ref()
                .map(|pending| pending.direction),
            Some(AlternateScrollDirection::Newer)
        );

        let older = ScreenView {
            rows: ["older-3", "older-2", "older-1"]
                .map(|line| line.as_bytes().to_vec())
                .to_vec(),
            size: (3, 16),
            cursor: (0, 0),
            hide_cursor: false,
        };
        let mut capture = AlternateSelectionBuffer::new(PaneTarget::Agent, page(0)).unwrap();
        capture.rows = (0..SCROLLBACK_LINES)
            .map(|line| format!("line-{line:04}").into_bytes())
            .collect();
        capture.viewport_rows = page(0).rows;
        capture.viewport_positions = vec![0, 1, 2];
        capture.queue_scroll(
            AlternateScrollRequest {
                direction: AlternateScrollDirection::Older,
                bytes: Vec::new(),
            },
            12,
        );
        capture.merge_view(older).unwrap();
        assert_eq!(capture.rows.len(), SCROLLBACK_LINES);
        assert_eq!(capture.viewport_positions, [0, 1, 2]);
        assert_eq!(
            alternate_row_keys(&capture.rows[..3], capture.cols),
            ["older-3", "older-2", "older-1"]
        );
        assert_eq!(
            alternate_row_keys(&capture.rows[SCROLLBACK_LINES - 1..], capture.cols),
            ["line-1996"]
        );

        let mut capture = AlternateSelectionBuffer::new(PaneTarget::Agent, page(0)).unwrap();
        for request in 0..ALTERNATE_SCROLL_QUEUE_LIMIT + 10 {
            capture.queue_scroll(
                AlternateScrollRequest {
                    direction: AlternateScrollDirection::Newer,
                    bytes: vec![request as u8],
                },
                20,
            );
        }
        assert_eq!(capture.queued_scrolls.len(), ALTERNATE_SCROLL_QUEUE_LIMIT);
    }

    #[test]
    fn alternate_mouse_reporting_selection_captures_the_scrolled_page() {
        let root = tempdir().unwrap();
        let agent = ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path()).arg("-c").arg(
                r"stty raw -echo; \
                  printf '\033[?1049h\033[?1002h\033[?1006hline-4\r\nline-5\r\nline-6'; \
                  dd bs=1 count=10 of=/dev/null 2>/dev/null; \
                  printf '\033[2J\033[Hline-1'; sleep 0.08; \
                  printf '\r\nline-2\r\nline-3'; sleep 2",
            ),
            (12, 3),
        )
        .unwrap();
        agent.wait_for_first_output(Duration::from_secs(1));
        let deadline = Instant::now() + Duration::from_secs(1);
        while agent.mouse_protocol().0 == vt100::MouseProtocolMode::None
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        let mut terminals = SessionTerminals {
            agent: Some(Arc::new(agent)),
            ..SessionTerminals::default()
        };
        let layout = WorkspaceLayout::new(120, 30, 0, 0);

        for event in [
            WorkspaceMouseEvent {
                button: 0,
                col: layout.agent.left + 6,
                row: layout.agent.top + 3,
                pressed: true,
            },
            WorkspaceMouseEvent {
                button: 64,
                col: layout.agent.left + 1,
                row: layout.agent.top + 1,
                pressed: true,
            },
        ] {
            terminals.handle_mouse(&layout, event).unwrap();
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            let visible = terminals
                .agent
                .as_ref()
                .unwrap()
                .screen_view()
                .rows
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
            if String::from_utf8_lossy(&visible).contains("line-1") {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        terminals
            .handle_mouse(
                &layout,
                WorkspaceMouseEvent {
                    button: 32,
                    col: layout.agent.left + 1,
                    row: layout.agent.top + 1,
                    pressed: true,
                },
            )
            .unwrap();
        terminals
            .handle_mouse(
                &layout,
                WorkspaceMouseEvent {
                    button: 0,
                    col: layout.agent.left + 1,
                    row: layout.agent.top + 1,
                    pressed: false,
                },
            )
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while terminals.pending_alternate_copy.is_some() && Instant::now() < deadline {
            terminals.refresh_alternate_selection().unwrap();
            terminals.finish_pending_alternate_copy();
            thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(
            terminals.selected_text().as_deref(),
            Some("line-1\nline-2\nline-3\nline-4\nline-5\nline-6")
        );
    }

    #[test]
    fn mouse_drag_selects_text_in_one_pane() {
        let root = tempdir().unwrap();
        let agent = ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path())
                .arg("-c")
                .arg("printf 'select-me\\n'; sleep 2"),
            (20, 3),
        )
        .unwrap();
        agent.wait_for_first_output(Duration::from_secs(1));
        let mut terminals = SessionTerminals {
            agent: Some(Arc::new(agent)),
            ..SessionTerminals::default()
        };
        let layout = WorkspaceLayout::new(120, 30, 0, 0);

        terminals
            .handle_mouse(
                &layout,
                WorkspaceMouseEvent {
                    button: 0,
                    col: layout.agent.left + 1,
                    row: layout.agent.top + 1,
                    pressed: true,
                },
            )
            .unwrap();
        terminals
            .handle_mouse(
                &layout,
                WorkspaceMouseEvent {
                    button: 32,
                    col: layout.agent.left + 6,
                    row: layout.agent.top + 1,
                    pressed: true,
                },
            )
            .unwrap();

        assert_eq!(terminals.selected_text().as_deref(), Some("select"));
        let chrome = WorkspaceChrome {
            sessions: vec!["▾ repo".into(), "○ Cdx select output".into()],
            selected: 1,
            selected_session_key: None,
            search_query: String::new(),
            status_counts: (0, 0, 1, 0),
            preview: vec!["preview".into()],
            notification: None,
        };
        let mut output = Vec::new();
        render_workspace(
            &mut output,
            &terminals,
            &chrome,
            &layout,
            WorkspaceFocus::Agent,
            false,
        )
        .unwrap();
        assert!(
            String::from_utf8_lossy(&output).contains("\x1b[7mselect\x1b[0m"),
            "selected cells should be visibly highlighted"
        );
    }

    #[test]
    fn mouse_click_without_drag_does_not_select_or_copy() {
        let root = tempdir().unwrap();
        let agent = ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path())
                .arg("-c")
                .arg(r"printf '\033[?1049hclick-me'; sleep 2"),
            (20, 3),
        )
        .unwrap();
        agent.wait_for_first_output(Duration::from_secs(1));
        let mut terminals = SessionTerminals {
            agent: Some(Arc::new(agent)),
            ..SessionTerminals::default()
        };
        let layout = WorkspaceLayout::new(120, 30, 0, 0);
        let click = WorkspaceMouseEvent {
            button: 0,
            col: layout.agent.left + 1,
            row: layout.agent.top + 1,
            pressed: true,
        };

        terminals.handle_mouse(&layout, click).unwrap();
        terminals
            .handle_mouse(
                &layout,
                WorkspaceMouseEvent {
                    pressed: false,
                    ..click
                },
            )
            .unwrap();

        assert_eq!(terminals.selection, None);
        assert_eq!(terminals.selected_text(), None);
        assert_eq!(terminals.notice, None);
    }

    #[test]
    fn ordinary_drag_selects_text_in_a_shell_pane() {
        let root = tempdir().unwrap();
        let shell = ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path())
                .arg("-c")
                .arg("printf 'shell-copy\\n'; sleep 2"),
            (20, 3),
        )
        .unwrap();
        shell.wait_for_first_output(Duration::from_secs(1));
        let content_row = shell
            .screen_view()
            .rows
            .iter()
            .position(|row| String::from_utf8_lossy(row).contains("shell-copy"))
            .unwrap() as u16;
        let mut terminals = SessionTerminals {
            shells: vec![ShellPane::new("one".into(), shell, "shell 1".into())],
            ..SessionTerminals::default()
        };
        let layout = WorkspaceLayout::new(120, 30, 1, 0);
        let shell_rect = layout.shell_panes[0].1;

        terminals
            .handle_mouse(
                &layout,
                WorkspaceMouseEvent {
                    button: 0,
                    col: shell_rect.left + 1,
                    row: shell_rect.top + 2 + content_row,
                    pressed: true,
                },
            )
            .unwrap();
        terminals
            .handle_mouse(
                &layout,
                WorkspaceMouseEvent {
                    button: 32,
                    col: shell_rect.left + 6,
                    row: shell_rect.top + 2 + content_row,
                    pressed: true,
                },
            )
            .unwrap();

        assert_eq!(terminals.selected_text().as_deref(), Some("shell-"));
    }

    #[test]
    fn unreserved_control_byte_is_forwarded_immediately() {
        let mut router = WorkspaceInputRouter::default();
        let routed = router.route(b"\x02", WorkspaceFocus::Agent);
        assert!(matches!(
            routed.as_slice(),
            [WorkspaceInput::Forward(bytes)] if bytes == b"\x02"
        ));
    }

    #[test]
    fn modified_enter_encodings_are_forwarded_to_child() {
        for focus in [WorkspaceFocus::Agent, WorkspaceFocus::Shell] {
            for sequence in [
                b"\n".as_slice(),
                b"\x1b[13;5u".as_slice(),
                b"\x1b[27;5;13~".as_slice(),
            ] {
                let mut router = WorkspaceInputRouter::default();
                assert!(matches!(
                    router.route(sequence, focus).as_slice(),
                    [WorkspaceInput::Forward(bytes)] if bytes == sequence
                ));
            }
        }
    }

    #[test]
    fn only_documented_control_bytes_are_reserved_in_child_focus() {
        let agent_reserved = [0x11, 0x1c, 0x1e];
        let shell_reserved = [0x0e, 0x11, 0x18, 0x1c, 0x1e];

        for (focus, reserved) in [
            (WorkspaceFocus::Agent, agent_reserved.as_slice()),
            (WorkspaceFocus::Shell, shell_reserved.as_slice()),
        ] {
            for byte in 0x00..=0x1f {
                if byte == 0x1b || reserved.contains(&byte) {
                    continue;
                }
                let mut router = WorkspaceInputRouter::default();
                let input = [byte];
                assert!(
                    matches!(
                        router.route(&input, focus).as_slice(),
                        [WorkspaceInput::Forward(bytes)] if bytes == &input
                    ),
                    "control byte 0x{byte:02x} was unexpectedly intercepted in {focus:?}"
                );
            }
        }
    }

    #[test]
    fn ordinary_keys_and_ctrl_t_are_forwarded_in_child_focus() {
        for focus in [WorkspaceFocus::Agent, WorkspaceFocus::Shell] {
            for sequence in [
                b"s".as_slice(),
                b"h".as_slice(),
                b"j".as_slice(),
                b"k".as_slice(),
                b"l".as_slice(),
                b"d".as_slice(),
                b"r".as_slice(),
                b"m".as_slice(),
                b"y".as_slice(),
                b"n".as_slice(),
                b"x".as_slice(),
                b"+".as_slice(),
                b"-".as_slice(),
                b"1".as_slice(),
                b"\x14".as_slice(),
                b"\x1b[A".as_slice(),
                b"\x1b[B".as_slice(),
                // Claude Code's app:toggleTranscript, app:openArtifact, and
                // selection:extendLineEnd must reach the child untouched.
                b"\x0f".as_slice(),
                b"\x1d".as_slice(),
                b"\x1b[1;2F".as_slice(),
            ] {
                let mut router = WorkspaceInputRouter::default();
                assert!(matches!(
                    router.route(sequence, focus).as_slice(),
                    [WorkspaceInput::Forward(bytes)] if bytes == sequence
                ));
            }
        }
    }

    #[test]
    fn direct_shell_controls_do_not_require_session_list_focus() {
        for focus in [WorkspaceFocus::Agent, WorkspaceFocus::Shell] {
            let mut router = WorkspaceInputRouter::default();
            assert!(matches!(
                router.route(b"\x1e", focus).as_slice(),
                [WorkspaceInput::Command(WorkspaceCommand::NewShell)]
            ));
        }

        for (sequence, command) in [
            (b"\x0e".as_slice(), WorkspaceCommand::NextShell),
            (b"\x18".as_slice(), WorkspaceCommand::CloseShell),
        ] {
            let mut shell_router = WorkspaceInputRouter::default();
            assert!(matches!(
                shell_router.route(sequence, WorkspaceFocus::Shell).as_slice(),
                [WorkspaceInput::Command(actual)] if *actual == command
            ));

            let mut agent_router = WorkspaceInputRouter::default();
            assert!(matches!(
                agent_router.route(sequence, WorkspaceFocus::Agent).as_slice(),
                [WorkspaceInput::Forward(bytes)] if bytes == sequence
            ));
        }
    }

    #[test]
    fn enhanced_control_sequences_keep_workspace_shortcuts_working() {
        for (sequence, command) in [
            (b"\x1b[92;5u".as_slice(), WorkspaceCommand::ToggleFocus),
            (b"\x1b[94;5u".as_slice(), WorkspaceCommand::NewShell),
            (b"\x1b[113;5u".as_slice(), WorkspaceCommand::Dashboard),
        ] {
            let mut router = WorkspaceInputRouter::default();
            assert!(matches!(
                router.route(sequence, WorkspaceFocus::Agent).as_slice(),
                [WorkspaceInput::Command(actual)] if *actual == command
            ));
        }

        for (sequence, command) in [
            (b"\x1b[110;5u".as_slice(), WorkspaceCommand::NextShell),
            (b"\x1b[120;5u".as_slice(), WorkspaceCommand::CloseShell),
        ] {
            let mut shell_router = WorkspaceInputRouter::default();
            assert!(matches!(
                shell_router
                    .route(sequence, WorkspaceFocus::Shell)
                    .as_slice(),
                [WorkspaceInput::Command(actual)] if *actual == command
            ));

            let mut agent_router = WorkspaceInputRouter::default();
            assert!(matches!(
                agent_router
                    .route(sequence, WorkspaceFocus::Agent)
                    .as_slice(),
                [WorkspaceInput::Forward(bytes)] if bytes == sequence
            ));
        }
    }

    #[test]
    fn escape_is_never_a_console_command_and_is_flushed_to_the_child() {
        for focus in [WorkspaceFocus::Agent, WorkspaceFocus::Shell] {
            let mut router = WorkspaceInputRouter::default();
            assert!(router.route(b"\x1b", focus).is_empty());
            assert_eq!(router.flush(), Some(b"\x1b".to_vec()));
        }
    }

    #[test]
    fn focus_cycles_sessions_agent_and_shell() {
        let root = tempdir().unwrap();
        let session = session(AgentKind::Codex, root.path());
        let mut terminals = SessionTerminals::default();

        // No shells: Agent -> Sessions (no shell spawned)
        let sessions = terminals
            .toggle_workspace_focus(&session, WorkspaceFocus::Agent)
            .unwrap();
        assert_eq!(sessions, WorkspaceFocus::Sessions);
        assert_eq!(terminals.shells.len(), 0);

        // Sessions -> Agent
        let agent = terminals
            .toggle_workspace_focus(&session, sessions)
            .unwrap();
        assert_eq!(agent, WorkspaceFocus::Agent);

        // With a shell present: Agent -> Shell -> Sessions -> Agent
        let (id, shell) = terminals.spawn_shell(&session, (80, 12)).unwrap();
        terminals.shells.push(ShellPane::new(id, shell, "1".into()));
        terminals.selected_shell = 0;

        let shell = terminals
            .toggle_workspace_focus(&session, WorkspaceFocus::Agent)
            .unwrap();
        assert_eq!(shell, WorkspaceFocus::Shell);

        let sessions2 = terminals.toggle_workspace_focus(&session, shell).unwrap();
        assert_eq!(sessions2, WorkspaceFocus::Sessions);

        let agent2 = terminals
            .toggle_workspace_focus(&session, sessions2)
            .unwrap();
        assert_eq!(agent2, WorkspaceFocus::Agent);
    }

    #[test]
    fn close_shell_is_immediate_only_when_shell_has_focus() {
        assert_eq!(
            shell_close_action(WorkspaceFocus::Agent),
            ShellCloseAction::Ignore
        );
        assert_eq!(
            shell_close_action(WorkspaceFocus::Sessions),
            ShellCloseAction::Ignore
        );
        assert_eq!(
            shell_close_action(WorkspaceFocus::Shell),
            ShellCloseAction::Close
        );
    }

    #[test]
    fn focused_session_list_accepts_navigation_and_creation_actions() {
        assert_eq!(workspace_command(b"/"), Some(WorkspaceCommand::Search));
        assert_eq!(workspace_command(b"a"), Some(WorkspaceCommand::Alert));
        assert_eq!(workspace_command(b"?"), Some(WorkspaceCommand::Help));
        assert_eq!(
            session_list_input(b"\x1b[A"),
            Some(SessionListInput::Previous)
        );
        assert_eq!(session_list_input(b"k"), Some(SessionListInput::Previous));
        assert_eq!(session_list_input(b"\x1b[B"), Some(SessionListInput::Next));
        assert_eq!(session_list_input(b"j"), Some(SessionListInput::Next));
        assert_eq!(session_list_input(b"\r"), Some(SessionListInput::Activate));
        assert_eq!(session_list_input(b"n"), Some(SessionListInput::NewSession));
        assert_eq!(session_list_input(b"s"), Some(SessionListInput::OpenShell));
        assert_eq!(
            session_list_input(b"x"),
            Some(SessionListInput::ToggleArchive)
        );
    }

    #[test]
    fn workspace_search_handles_batched_editing_and_commit_input() {
        let mut search = WorkspaceSearch {
            value: "old".into(),
            original_query: String::new(),
            original_selected_session_key: None,
        };

        let (input, changed) = apply_workspace_search_input(&mut search, b"\x7f\x7f\x7fnew\r");

        assert_eq!(input, WorkspaceSearchInput::Commit);
        assert!(changed);
        assert_eq!(search.value, "new");
    }

    #[test]
    fn subsequent_workspace_frame_does_not_clear_the_screen() {
        let terminals = SessionTerminals::default();
        let chrome = WorkspaceChrome {
            sessions: vec!["repo  codex".into()],
            selected: 0,
            selected_session_key: None,
            search_query: String::new(),
            status_counts: (0, 0, 1, 0),
            preview: vec!["preview".into()],
            notification: None,
        };
        let layout = WorkspaceLayout::new(120, 40, 0, 0);
        let mut output = Vec::new();

        render_workspace(
            &mut output,
            &terminals,
            &chrome,
            &layout,
            WorkspaceFocus::Agent,
            false,
        )
        .unwrap();

        assert!(!output.windows(4).any(|bytes| bytes == b"\x1b[2J"));
    }

    #[test]
    fn workspace_frame_is_synchronized_to_prevent_partial_redraw_flicker() {
        let terminals = SessionTerminals::default();
        let chrome = WorkspaceChrome {
            sessions: vec!["repo  codex".into()],
            selected: 0,
            selected_session_key: None,
            search_query: String::new(),
            status_counts: (0, 0, 1, 0),
            preview: vec!["preview".into()],
            notification: None,
        };
        let layout = WorkspaceLayout::new(120, 40, 0, 0);
        let mut output = Vec::new();

        render_workspace(
            &mut output,
            &terminals,
            &chrome,
            &layout,
            WorkspaceFocus::Sessions,
            false,
        )
        .unwrap();

        assert!(
            output.starts_with(b"\x1b[?2026h"),
            "the terminal can display a partially rewritten frame without synchronized update mode"
        );
        assert!(
            output.ends_with(b"\x1b[?2026l"),
            "the synchronized update must end after the complete frame"
        );
    }

    #[test]
    fn focused_sidebar_highlights_the_selected_item_instead_of_the_title() {
        let chrome = WorkspaceChrome {
            sessions: vec!["▾ repo".into(), "○ Cdx current task".into()],
            selected: 1,
            selected_session_key: None,
            search_query: String::new(),
            status_counts: (0, 0, 1, 0),
            preview: vec!["preview".into()],
            notification: None,
        };
        let layout = WorkspaceLayout::new(120, 40, 0, 0);
        let mut output = Vec::new();

        render_sidebar(&mut output, &chrome, &layout, true).unwrap();

        let output = String::from_utf8_lossy(&output);
        assert!(
            output.contains("\x1b[30;46;1m▸ ○ Cdx current task"),
            "focused selection should own the cyan focus treatment"
        );
        assert!(
            !output.contains("\x1b[30;46;1m SESSIONS"),
            "the section title must not look like the focused row"
        );
    }

    #[test]
    fn workspace_sidebar_shows_live_status_counts_above_sessions() {
        let chrome = WorkspaceChrome {
            sessions: vec!["▾ repo".into(), "○ Cdx current task".into()],
            selected: 1,
            selected_session_key: None,
            search_query: String::new(),
            status_counts: (2, 1, 18, 3),
            preview: vec!["preview".into()],
            notification: None,
        };

        let mut narrow = Vec::new();
        render_sidebar(
            &mut narrow,
            &chrome,
            &WorkspaceLayout::new(120, 40, 0, 0),
            false,
        )
        .unwrap();
        let narrow = String::from_utf8_lossy(&narrow);
        assert!(narrow.contains("\x1b[32m● 2 work\x1b[0m"));
        assert!(narrow.contains("\x1b[33m◐ 1 wait\x1b[0m"));
        assert!(narrow.contains("\x1b[90m○ 18 idle\x1b[0m"));
        assert!(narrow.contains("\x1b[31m× 3 fail\x1b[0m"));
        assert!(narrow.contains("\x1b[4;1H\x1b[1;36m▾ repo"));

        let mut wide = Vec::new();
        render_sidebar(
            &mut wide,
            &chrome,
            &WorkspaceLayout::new(180, 40, 0, 0),
            false,
        )
        .unwrap();
        let wide = String::from_utf8_lossy(&wide);
        assert!(wide.contains("● 2 working"));
        assert!(wide.contains("◐ 1 waiting"));
        assert!(wide.contains("× 3 failed"));
    }

    #[test]
    fn workspace_sidebar_is_compact_on_wide_terminals() {
        assert_eq!(WorkspaceLayout::new(120, 40, 0, 0).sidebar_width, 20);
        assert_eq!(WorkspaceLayout::new(180, 40, 0, 0).sidebar_width, 28);
        assert_eq!(WorkspaceLayout::new(240, 40, 0, 0).sidebar_width, 28);
    }

    #[test]
    fn workspace_layout_supports_maximize_restore_and_shell_area_resize() {
        let mut agent = WorkspaceLayout::new(120, 40, 2, 1);
        agent.apply_options(Some(PaneTarget::Agent), 0);
        assert!(agent.shell_panes.is_empty());
        assert_eq!(agent.agent.height, agent.status_row - 1);

        let mut shell = WorkspaceLayout::new(120, 40, 2, 1);
        shell.apply_options(Some(PaneTarget::Shell(1)), 0);
        assert_eq!(shell.agent.height, 0);
        assert_eq!(shell.shell_panes.len(), 1);
        assert_eq!(shell.shell_panes[0].0, 1);
        assert_eq!(shell.shell_panes[0].1.height, shell.status_row);

        let mut resized = WorkspaceLayout::new(120, 40, 2, 1);
        let old_height = resized.shell_panes[0].1.height;
        resized.apply_options(None, 4);
        assert_eq!(resized.shell_panes[0].1.height, old_height + 4);
    }

    #[test]
    fn command_block_capture_excludes_output_before_the_last_submitted_command() {
        assert_eq!(
            command_block_after("old prompt\n$ build\nbuild passed\n$ ", "old prompt\n$ "),
            "build\nbuild passed\n$ "
        );
        assert_eq!(
            command_block_after("rotated tail", "missing prefix"),
            "rotated tail"
        );
    }

    #[test]
    fn workspace_text_is_fitted_by_terminal_columns() {
        assert_eq!(fit_text(" Cla 为什么 exporter", 12), " Cla 为什么 ");
    }

    #[test]
    fn terminal_render_clears_stale_pane_cells() {
        let root = tempdir().unwrap();
        let terminal = ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path())
                .arg("-c")
                .arg("printf ok; sleep 2"),
            (10, 2),
        )
        .unwrap();
        terminal.wait_for_first_output(Duration::from_secs(1));

        let rect = PaneRect {
            top: 1,
            left: 3,
            width: 10,
            height: 2,
        };
        let mut frame = Vec::new();
        render_terminal(&mut frame, &terminal, rect).unwrap();

        let mut outer = vt100::Parser::new(5, 20, 0);
        outer.process(b"\x1b[2;4HXXXXXXXXXX\x1b[3;4HYYYYYYYYYY");
        outer.process(&frame);
        let rows = outer
            .screen()
            .rows(rect.left, rect.width)
            .collect::<Vec<_>>();

        assert_eq!(rows[usize::from(rect.top)], "ok");
        assert_eq!(rows[usize::from(rect.top + 1)], "");
    }

    #[test]
    fn terminal_viewport_scrolls_back_and_returns_to_live_tail() {
        let root = tempdir().unwrap();
        let terminal = ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path())
                .arg("-c")
                .arg("i=1; while [ $i -le 8 ]; do echo line-$i; i=$((i+1)); done; sleep 2"),
            (20, 3),
        )
        .unwrap();
        terminal.wait_for_first_output(Duration::from_secs(1));

        let live = terminal.screen_view();
        let live_text = live
            .rows
            .iter()
            .map(|row| plain_text(row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(live_text.contains("line-8"));
        assert_eq!(terminal.scrollback_offset(), 0);

        assert_eq!(terminal.scroll_viewport(2), 2);
        let history = terminal.screen_view();
        let history_text = history
            .rows
            .iter()
            .map(|row| plain_text(row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!history_text.contains("line-8"));
        assert_eq!(terminal.scrollback_offset(), 2);

        terminal.scroll_to_live_tail();
        let _live_again = terminal.screen_view();
        assert_eq!(terminal.scrollback_offset(), 0);
    }

    #[test]
    fn workspace_reserves_an_agent_title_bar_and_shows_direct_shortcuts() {
        let terminals = SessionTerminals::default();
        let chrome = WorkspaceChrome {
            sessions: vec!["▾ repo".into(), "Cdx fix focus".into()],
            selected: 1,
            selected_session_key: None,
            search_query: String::new(),
            status_counts: (0, 0, 1, 0),
            preview: vec!["preview".into()],
            notification: None,
        };
        let layout = WorkspaceLayout::new(120, 40, 0, 0);
        assert_eq!(layout.agent.top, 1);

        let mut output = Vec::new();
        render_workspace(
            &mut output,
            &terminals,
            &chrome,
            &layout,
            WorkspaceFocus::Agent,
            false,
        )
        .unwrap();
        let output = String::from_utf8_lossy(&output);
        assert!(output.contains("AGENT · Cdx fix focus"));
        assert!(output.contains("Ctrl-Q dashboard"));
        assert!(output.contains("Ctrl-^ new shell"));
        assert!(output.contains("Ctrl-\\ focus"));
        assert!(output.contains("Shift-PageUp/Down scroll"));
        assert!(output.contains("FOCUS AGENT"));
    }

    #[test]
    fn focused_shell_footer_shows_all_direct_controls() {
        let root = tempdir().unwrap();
        let shell = ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path())
                .arg("-c")
                .arg("printf 'shell ready\\n'; sleep 2"),
            (80, 12),
        )
        .unwrap();
        shell.wait_for_first_output(Duration::from_secs(1));
        let terminals = SessionTerminals {
            shells: vec![ShellPane::new("one".into(), shell, "shell 1".into())],
            ..SessionTerminals::default()
        };
        let chrome = WorkspaceChrome {
            sessions: vec!["▾ repo".into(), "○ Cdx inspect shell".into()],
            selected: 1,
            selected_session_key: None,
            search_query: String::new(),
            status_counts: (0, 0, 1, 0),
            preview: vec!["preview".into()],
            notification: None,
        };
        let layout = WorkspaceLayout::new(160, 40, 1, 0);
        let mut output = Vec::new();

        render_workspace(
            &mut output,
            &terminals,
            &chrome,
            &layout,
            WorkspaceFocus::Shell,
            false,
        )
        .unwrap();
        let output = String::from_utf8_lossy(&output);

        assert!(output.contains("FOCUS SHELL 1/1"));
        assert!(output.contains("Ctrl-Q dashboard"));
        assert!(output.contains("Ctrl-\\ focus"));
        assert!(output.contains("Ctrl-^ new"));
        assert!(output.contains("Ctrl-N next"));
        assert!(output.contains("Ctrl-X close"));
        assert!(output.contains("Shift-PageUp/Down scroll"));
    }

    #[test]
    fn focused_session_list_has_a_visible_focus_badge() {
        let terminals = SessionTerminals {
            notice: Some("SESSION ARCHIVED · moved to the Archived group".into()),
            ..SessionTerminals::default()
        };
        let chrome = WorkspaceChrome {
            sessions: vec!["▾ repo".into(), "○ Cdx inspect session".into()],
            selected: 1,
            selected_session_key: None,
            search_query: String::new(),
            status_counts: (0, 0, 1, 0),
            preview: vec!["preview".into()],
            notification: None,
        };
        let layout = WorkspaceLayout::new(120, 40, 0, 0);
        let mut output = Vec::new();

        render_workspace(
            &mut output,
            &terminals,
            &chrome,
            &layout,
            WorkspaceFocus::Sessions,
            false,
        )
        .unwrap();
        let output = String::from_utf8_lossy(&output);

        assert!(output.contains("FOCUS SESSIONS"));
        assert!(output.contains("SESSION ARCHIVED"));
        assert!(output.contains("n new"));
        assert!(output.contains("s +shell"));
        assert!(output.contains("x archive"));
        assert!(output.contains("h agent"));
        assert!(output.contains("m shell"));
        assert!(output.contains("\x1b[30;46;1m▸ ○ Cdx inspect sess"));
        assert!(!output.contains("\x1b[30;46;1m SESSIONS"));
    }

    #[test]
    fn focused_session_list_footer_shows_search_alert_and_help() {
        let terminals = SessionTerminals::default();
        let chrome = WorkspaceChrome {
            sessions: vec!["▾ repo".into(), "○ Cdx inspect session".into()],
            selected: 1,
            selected_session_key: None,
            search_query: String::new(),
            status_counts: (0, 0, 1, 0),
            preview: vec!["preview".into()],
            notification: None,
        };
        let layout = WorkspaceLayout::new(120, 40, 0, 0);
        let mut output = Vec::new();

        render_workspace(
            &mut output,
            &terminals,
            &chrome,
            &layout,
            WorkspaceFocus::Sessions,
            false,
        )
        .unwrap();
        let output = String::from_utf8_lossy(&output);

        assert!(output.contains("/ search"));
        assert!(output.contains("a alert"));
        assert!(output.contains("? help"));
    }

    #[test]
    fn workspace_alert_renders_below_session_shortcut_hints() {
        let terminals = SessionTerminals::default();
        let chrome = WorkspaceChrome {
            sessions: vec!["▾ repo".into(), "○ Cdx inspect session".into()],
            selected: 1,
            selected_session_key: None,
            search_query: String::new(),
            status_counts: (0, 1, 1, 0),
            preview: vec!["preview".into()],
            notification: Some("inspect session: approval needed".into()),
        };
        let layout = WorkspaceLayout::new(160, 40, 0, 0);
        let mut output = Vec::new();

        render_workspace(
            &mut output,
            &terminals,
            &chrome,
            &layout,
            WorkspaceFocus::Sessions,
            false,
        )
        .unwrap();
        let mut parser = vt100::Parser::new(40, 160, 0);
        parser.process(&output);
        let rows = parser.screen().rows(0, 160).collect::<Vec<_>>();
        let shortcuts_row = rows
            .iter()
            .position(|row| row.contains("/ search") && row.contains("? help"))
            .expect("session shortcut hints should stay visible");
        let alert_row = rows
            .iter()
            .position(|row| row.contains("ALERT · inspect session: approval needed"))
            .expect("alert should be visible");

        assert_eq!(shortcuts_row, rows.len() - 2);
        assert_eq!(alert_row, rows.len() - 1);

        let chrome = WorkspaceChrome {
            notification: None,
            ..chrome
        };
        output.clear();
        render_workspace(
            &mut output,
            &terminals,
            &chrome,
            &layout,
            WorkspaceFocus::Sessions,
            false,
        )
        .unwrap();
        parser.process(&output);
        let rows = parser.screen().rows(0, 160).collect::<Vec<_>>();

        assert!(rows[rows.len() - 2].contains("/ search"));
        assert!(rows[rows.len() - 1].trim().is_empty());
    }

    #[test]
    fn workspace_search_renders_live_query_and_controls() {
        let terminals = SessionTerminals::default();
        let chrome = WorkspaceChrome {
            sessions: vec!["▾ repo".into(), "○ Cla latency".into()],
            selected: 1,
            selected_session_key: Some("claude:latency".into()),
            search_query: "latency".into(),
            status_counts: (0, 0, 1, 0),
            preview: vec!["preview".into()],
            notification: None,
        };
        let layout = WorkspaceLayout::new(120, 40, 0, 0);
        let mut output = Vec::new();

        render_workspace_with_bindings(
            &mut output,
            &terminals,
            &chrome,
            &layout,
            WorkspaceRenderState {
                focus: WorkspaceFocus::Sessions,
                search: Some("latency"),
                help: false,
            },
            &WorkspaceBindings::from_config(&AgentConsoleConfig::default()),
            false,
        )
        .unwrap();
        let output = String::from_utf8_lossy(&output);

        assert!(output.contains("SEARCH SESSIONS"));
        assert!(output.contains("/ latency"));
        assert!(output.contains("Enter keep"));
        assert!(output.contains("Esc cancel"));
        assert!(output.contains("Backspace edit"));
    }

    #[test]
    fn workspace_search_clears_sidebar_rows_removed_by_live_filter() {
        let terminals = SessionTerminals::default();
        let layout = WorkspaceLayout::new(120, 40, 0, 0);
        let bindings = WorkspaceBindings::from_config(&AgentConsoleConfig::default());
        let mut parser = vt100::Parser::new(40, 120, 0);
        let mut output = Vec::new();

        let unfiltered = WorkspaceChrome {
            sessions: vec!["▾ repo".into(), "○ Cdx KEEP".into(), "○ Cdx DROP".into()],
            selected: 1,
            selected_session_key: Some("codex:keep".into()),
            search_query: String::new(),
            status_counts: (0, 0, 2, 0),
            preview: vec!["preview".into()],
            notification: None,
        };
        render_workspace_with_bindings(
            &mut output,
            &terminals,
            &unfiltered,
            &layout,
            WorkspaceRenderState {
                focus: WorkspaceFocus::Sessions,
                search: None,
                help: false,
            },
            &bindings,
            false,
        )
        .unwrap();
        parser.process(&output);
        assert!(parser.screen().contents().contains("DROP"));

        output.clear();
        let filtered = WorkspaceChrome {
            sessions: vec!["▾ repo".into(), "○ Cdx KEEP".into()],
            search_query: "keep".into(),
            status_counts: (0, 0, 1, 0),
            ..unfiltered
        };
        render_workspace_with_bindings(
            &mut output,
            &terminals,
            &filtered,
            &layout,
            WorkspaceRenderState {
                focus: WorkspaceFocus::Sessions,
                search: Some("keep"),
                help: false,
            },
            &bindings,
            false,
        )
        .unwrap();
        parser.process(&output);

        let screen = parser.screen().contents();
        assert!(screen.contains("KEEP"));
        assert!(!screen.contains("DROP"));
    }

    #[test]
    fn workspace_help_renders_contextual_session_shortcuts() {
        let terminals = SessionTerminals::default();
        let chrome = WorkspaceChrome {
            sessions: vec!["▾ repo".into(), "○ Cla latency".into()],
            selected: 1,
            selected_session_key: Some("claude:latency".into()),
            search_query: String::new(),
            status_counts: (0, 0, 1, 0),
            preview: vec!["preview".into()],
            notification: None,
        };
        let layout = WorkspaceLayout::new(120, 40, 0, 0);
        let mut output = Vec::new();

        render_workspace_with_bindings(
            &mut output,
            &terminals,
            &chrome,
            &layout,
            WorkspaceRenderState {
                focus: WorkspaceFocus::Sessions,
                search: None,
                help: true,
            },
            &WorkspaceBindings::from_config(&AgentConsoleConfig::default()),
            false,
        )
        .unwrap();
        let output = String::from_utf8_lossy(&output);

        assert!(output.contains("WORKSPACE KEY BINDINGS"));
        assert!(output.contains("WORKSPACE · DIRECT"));
        assert!(output.contains("WORKSPACE · SESSIONS"));
        assert!(output.contains("search sessions"));
        assert!(output.contains("next unread alert"));
        assert!(output.contains("WORKSPACE HELP"));
        assert!(output.contains("? or Esc close"));
    }

    #[test]
    fn maximized_agent_renders_the_live_pty_instead_of_the_session_preview() {
        let root = tempdir().unwrap();
        let agent = ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path())
                .arg("-c")
                .arg("printf 'LIVE AGENT SCREEN\\n'; sleep 2"),
            (80, 24),
        )
        .unwrap();
        agent.wait_for_first_output(Duration::from_secs(1));
        let terminals = SessionTerminals {
            agent: Some(Arc::new(agent)),
            maximized: Some(PaneTarget::Agent),
            ..SessionTerminals::default()
        };
        let chrome = WorkspaceChrome {
            sessions: vec!["▾ repo".into(), "Cdx current task".into()],
            selected: 1,
            selected_session_key: None,
            search_query: String::new(),
            status_counts: (0, 0, 1, 0),
            preview: vec!["STALE SESSION PREVIEW".into()],
            notification: None,
        };
        let layout = WorkspaceLayout::new(120, 40, 0, 0);
        let mut output = Vec::new();

        render_workspace(
            &mut output,
            &terminals,
            &chrome,
            &layout,
            WorkspaceFocus::Agent,
            false,
        )
        .unwrap();
        let output = String::from_utf8_lossy(&output);

        assert!(output.contains("AGENT · Cdx current task"));
        assert!(output.contains("LIVE AGENT SCREEN"));
        assert!(!output.contains("STALE SESSION PREVIEW"));
        assert!(output.contains("FOCUS AGENT"));
    }

    #[test]
    fn workspace_title_reports_scrollback_offset() {
        let root = tempdir().unwrap();
        let agent = ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path())
                .arg("-c")
                .arg("i=1; while [ $i -le 8 ]; do echo line-$i; i=$((i+1)); done; sleep 2"),
            (20, 3),
        )
        .unwrap();
        agent.wait_for_first_output(Duration::from_secs(1));
        agent.scroll_viewport(2);
        let terminals = SessionTerminals {
            agent: Some(Arc::new(agent)),
            ..SessionTerminals::default()
        };
        let chrome = WorkspaceChrome {
            sessions: vec!["▾ repo".into(), "○ Cdx inspect history".into()],
            selected: 1,
            selected_session_key: None,
            search_query: String::new(),
            status_counts: (0, 0, 1, 0),
            preview: vec!["preview".into()],
            notification: None,
        };
        let layout = WorkspaceLayout::new(120, 40, 0, 0);

        let mut output = Vec::new();
        render_workspace(
            &mut output,
            &terminals,
            &chrome,
            &layout,
            WorkspaceFocus::Agent,
            false,
        )
        .unwrap();

        assert!(String::from_utf8_lossy(&output).contains("SCROLL +2"));
    }

    #[test]
    fn workspace_disables_terminal_mouse_reporting() {
        assert!(
            DISABLE_MOUSE_REPORTING
                .windows(8)
                .any(|bytes| bytes == b"\x1b[?1003l")
        );
        assert!(
            DISABLE_MOUSE_REPORTING
                .windows(8)
                .any(|bytes| bytes == b"\x1b[?1006l")
        );
    }

    #[test]
    fn workspace_requests_modified_key_reporting_only_for_agent_input() {
        assert_eq!(ENABLE_KEYBOARD_ENHANCEMENT, b"\x1b[>1u");
        assert_eq!(DISABLE_KEYBOARD_ENHANCEMENT, b"\x1b[<1u");

        let mut output = Vec::new();
        let mut enabled = false;
        sync_keyboard_enhancement(&mut output, &mut enabled, WorkspaceFocus::Sessions).unwrap();
        sync_keyboard_enhancement(&mut output, &mut enabled, WorkspaceFocus::Agent).unwrap();
        sync_keyboard_enhancement(&mut output, &mut enabled, WorkspaceFocus::Agent).unwrap();
        sync_keyboard_enhancement(&mut output, &mut enabled, WorkspaceFocus::Shell).unwrap();
        assert_eq!(
            output,
            [ENABLE_KEYBOARD_ENHANCEMENT, DISABLE_KEYBOARD_ENHANCEMENT].concat()
        );
    }

    #[test]
    fn console_key_events_encode_as_terminal_input() {
        assert_eq!(
            terminal_event_bytes(Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            ))),
            vec![0x03]
        );
        assert_eq!(
            terminal_event_bytes(Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT,))),
            b"\x1b[1;3D"
        );
        assert_eq!(
            terminal_event_bytes(Event::Key(KeyEvent::new(
                KeyCode::F(12),
                KeyModifiers::NONE,
            ))),
            b"\x1b[24~"
        );
        assert_eq!(
            terminal_event_bytes(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::CONTROL,
            ))),
            b"\x1b[13;5u"
        );
    }

    #[test]
    fn workspace_sidebar_colors_provider_labels_like_dashboard() {
        let chrome = WorkspaceChrome {
            sessions: vec![
                "▾ repo".into(),
                "○ Cdx codex task".into(),
                "! Cla claude task".into(),
            ],
            selected: 1,
            selected_session_key: None,
            search_query: String::new(),
            status_counts: (0, 1, 1, 0),
            preview: vec!["preview".into()],
            notification: None,
        };
        let layout = WorkspaceLayout::new(120, 40, 0, 0);
        let mut output = Vec::new();

        render_sidebar(&mut output, &chrome, &layout, false).unwrap();

        let output = String::from_utf8_lossy(&output);
        assert!(output.contains("\x1b[36mCdx\x1b[0m"));
        assert!(output.contains("\x1b[38;2;219;126;82mCla\x1b[0m"));
    }

    #[test]
    fn console_mouse_events_encode_as_sgr_input() {
        let event = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 9,
            row: 4,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(terminal_event_bytes(Event::Mouse(event)), b"\x1b[<65;10;5M");
    }

    /// A cheap stand-in for a real login shell: `add_shell` would run `$SHELL -l`, whose
    /// startup files differ per machine, and none of that is what these assertions are about.
    fn idle_terminal(cwd: &Path) -> ManagedTerminal {
        ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", cwd).arg("-c").arg("sleep 30"),
            (80, 24),
        )
        .unwrap()
    }

    fn manager_with_shells(key: &str, ids: &[&str], cwd: &Path) -> TerminalManager {
        let mut terminals = SessionTerminals::default();
        for (index, id) in ids.iter().enumerate() {
            terminals.shells.push(ShellPane::new(
                (*id).to_owned(),
                idle_terminal(cwd),
                format!("shell {}", index + 1),
            ));
        }
        let mut manager = TerminalManager::new_local(AgentConsoleConfig::default());
        manager
            .terminals
            .insert(key.to_owned(), Arc::new(Mutex::new(terminals)));
        manager
    }

    /// Two lookups have to be the *same* terminal, not two views of one.
    ///
    /// The handle is what lets a websocket poll a terminal while an attached workspace is
    /// repainting it, each holding no lock the other needs. That only works while both are
    /// looking at one object; a copy would give the browser a terminal that quietly diverged
    /// from the one on screen.
    #[test]
    fn a_shell_handle_is_the_same_terminal_every_surface_gets() {
        let root = tempdir().unwrap();
        let manager = manager_with_shells("claude:one", &["aaa"], root.path());

        let first = manager.shell("claude:one", "aaa").unwrap();
        let second = manager.shell("claude:one", "aaa").unwrap();

        assert!(
            Arc::ptr_eq(&first, &second),
            "each lookup handed out a different terminal"
        );
    }

    #[test]
    fn a_sessions_shells_are_listed_with_the_ids_that_address_them() {
        let root = tempdir().unwrap();
        let manager = manager_with_shells("claude:one", &["aaa", "bbb"], root.path());

        assert_eq!(
            manager.shells("claude:one"),
            vec![
                ShellInfo {
                    id: "aaa".into(),
                    name: "shell 1".into()
                },
                ShellInfo {
                    id: "bbb".into(),
                    name: "shell 2".into()
                },
            ]
        );
        assert!(
            manager.shells("claude:absent").is_empty(),
            "a session with no terminals has no shells rather than panicking"
        );
    }

    #[test]
    fn a_shell_is_borrowed_by_id_and_only_from_its_own_session() {
        let root = tempdir().unwrap();
        let manager = manager_with_shells("claude:one", &["aaa", "bbb"], root.path());

        assert!(manager.shell("claude:one", "bbb").is_some());
        assert!(manager.shell("claude:one", "zzz").is_none());
        assert!(
            manager.shell("claude:other", "aaa").is_none(),
            "a shell id must not address a shell belonging to a different session"
        );
    }

    #[test]
    fn closing_one_shell_leaves_the_others_alive_and_keeps_the_selection_in_range() {
        let root = tempdir().unwrap();
        let mut manager = manager_with_shells("claude:one", &["aaa", "bbb"], root.path());
        manager
            .terminals
            .get_mut("claude:one")
            .unwrap()
            .lock()
            .unwrap()
            .selected_shell = 1;

        assert!(manager.close_shell("claude:one", "aaa"));
        assert!(
            !manager.close_shell("claude:one", "aaa"),
            "closing the same shell twice reports that there was nothing left to close"
        );

        assert_eq!(
            manager
                .shells("claude:one")
                .into_iter()
                .map(|shell| shell.id)
                .collect::<Vec<_>>(),
            vec!["bbb".to_owned()]
        );
        assert!(
            manager
                .shell("claude:one", "bbb")
                .is_some_and(|agent| agent.is_alive()),
            "closing one shell must not disturb another"
        );
        assert_eq!(
            manager.terminals["claude:one"]
                .lock()
                .unwrap()
                .selected_shell,
            0,
            "the TUI's selection has to stay inside the shells that are left"
        );
    }

    #[test]
    fn a_shell_id_is_the_last_segment_of_its_daemon_id_even_when_the_session_key_has_pipes() {
        assert_eq!(shell_id_of("shell|claude:abc123|9a3f"), Some("9a3f"));
        assert_eq!(shell_id_of("shell|odd|key|9a3f"), Some("9a3f"));
        assert_eq!(
            shell_id_of("agent|claude:abc123"),
            None,
            "an agent terminal never names a shell"
        );
        assert_eq!(shell_id_of("shell|claude:abc123"), None);
    }

    // ----------------------------------------------------- the attach snapshot
    //
    // A client attaching to a terminal that has been running gets a checkpoint, which is
    // exactly one screenful. These cover the other half of that answer: the rows above the
    // screen, and the guarantee that the two halves meet without overlapping or leaving a gap.

    /// A parser with a deeper scrollback than the text it is given, so the rows that scroll
    /// off the top are retained rather than dropped.
    fn parser_after(height: u16, cols: u16, text: &str) -> vt100::Parser {
        let mut parser = vt100::Parser::new(height, cols, SCROLLBACK_LINES);
        parser.process(text.as_bytes());
        parser
    }

    fn snapshot_rows(snapshot: &str) -> Vec<String> {
        snapshot
            .split("\r\n")
            .map(|row| plain_text(row.as_bytes()).trim_end().to_owned())
            .collect()
    }

    #[test]
    fn the_scrollback_snapshot_is_the_rows_above_the_screen_and_none_of_the_ones_on_it() {
        let mut parser = parser_after(4, 20, "one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\n");

        let snapshot =
            terminal_scrollback_snapshot(&mut parser, &StatusBarScrollback::default()).unwrap();

        assert_eq!(snapshot_rows(&snapshot), vec!["one", "two", "three"]);
        assert!(
            !snapshot.contains("four"),
            "the visible screen is the checkpoint's job; carrying it here too would print it twice"
        );
    }

    /// The seam, as an invariant rather than a hope. Whatever the terminal holds, the rows the
    /// snapshot carries followed by the rows the checkpoint paints are the retained grid split
    /// in two -- nothing counted twice, nothing dropped between them.
    #[test]
    fn the_snapshot_and_the_checkpoint_partition_the_retained_grid() {
        let mut parser = parser_after(4, 20, "one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\n");
        let scrollback = StatusBarScrollback::default();
        let printed = |rows: Vec<String>| -> Vec<String> {
            rows.into_iter().filter(|row| !row.is_empty()).collect()
        };

        let retained = printed(
            terminal_retained_rows(&mut parser, &scrollback)
                .iter()
                .map(|row| plain_text(row).trim_end().to_owned())
                .collect(),
        );
        let above = printed(snapshot_rows(
            &terminal_scrollback_snapshot(&mut parser, &scrollback).unwrap(),
        ));
        let mut repainted = vt100::Parser::new(4, 20, 0);
        repainted.process(&terminal_state_checkpoint(&parser, &scrollback));
        let screen = printed(
            repainted
                .screen()
                .rows_formatted(0, 20)
                .map(|row| plain_text(&row).trim_end().to_owned())
                .collect(),
        );

        assert_eq!([above, screen].concat(), retained);
    }

    /// A screen that starts blank evicts its blank rows before it evicts a line of text, so
    /// the oldest rows are padding. Left in, they make the top of the history look empty.
    #[test]
    fn the_blank_rows_a_bounded_scrollback_starts_with_are_dropped() {
        let mut parser = parser_after(3, 20, "\r\n\r\n\r\nfirst\r\nsecond\r\nthird\r\nfourth\r\n");

        let snapshot =
            terminal_scrollback_snapshot(&mut parser, &StatusBarScrollback::default()).unwrap();

        assert_eq!(snapshot_rows(&snapshot), vec!["first", "second"]);
    }

    #[test]
    fn a_terminal_that_has_printed_nothing_has_no_snapshot_to_send() {
        let mut parser = vt100::Parser::new(4, 20, SCROLLBACK_LINES);

        assert!(
            terminal_scrollback_snapshot(&mut parser, &StatusBarScrollback::default()).is_none()
        );
    }

    /// `rows_formatted` emits only the attributes a row needs, so a row that ends mid-colour
    /// would bleed into the next one all the way down the history.
    #[test]
    fn each_snapshot_row_closes_its_own_colour() {
        let mut parser = parser_after(2, 20, "\x1b[31mred\x1b[m\r\nplain\r\nlast\r\n");

        let snapshot =
            terminal_scrollback_snapshot(&mut parser, &StatusBarScrollback::default()).unwrap();

        assert_eq!(snapshot_rows(&snapshot), vec!["red", "plain"]);
        assert!(
            snapshot.contains("\u{1b}[31m"),
            "the colour itself survives: {snapshot:?}"
        );
        assert!(
            snapshot.ends_with("\u{1b}[m"),
            "every row closes its own colour: {snapshot:?}"
        );
    }

    /// An application that takes the alternate screen keeps no history anywhere -- `vt100`
    /// gives that grid no scrollback and neither does xterm.js -- so there is nothing to send
    /// and the checkpoint stands alone rather than being preceded by rows that would land in
    /// a buffer the application has switched away from.
    #[test]
    fn an_alternate_screen_has_no_rows_above_it_to_send() {
        let mut parser = parser_after(4, 20, "\x1b[?1049hone\r\ntwo\r\nthree\r\nfour\r\nfive\r\n");

        assert!(
            terminal_scrollback_snapshot(&mut parser, &StatusBarScrollback::default()).is_none()
        );
    }

    /// A TUI that pins its composer to the bottom scrolls a partial region, and `vt100` keeps
    /// nothing for that -- so those rows come from the capture `pty.rs` makes for the TUI's
    /// own scroll keys instead, through the same snapshot.
    #[test]
    fn a_partial_scroll_regions_captured_rows_are_the_snapshot_vt100_never_had() {
        let mut parser = vt100::Parser::new(3, 20, SCROLLBACK_LINES);
        let mut scrollback = StatusBarScrollback::default();
        scrollback
            .rows
            .push_back(b"scrolled-out-of-the-region".to_vec());

        let snapshot = terminal_scrollback_snapshot(&mut parser, &scrollback).unwrap();

        assert_eq!(snapshot_rows(&snapshot), vec!["scrolled-out-of-the-region"]);
    }

    /// The point of taking the rows from the parser rather than the retained bytes: once the
    /// raw ring has rolled over, the bytes cannot rebuild the history and the parser still can.
    #[test]
    fn a_resync_carries_rows_the_raw_ring_no_longer_holds_and_only_when_asked() {
        let root = tempdir().unwrap();
        let terminal = LocalTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path()).arg("-c").arg(
                "printf 'marker-alpha\\n'; \
                 awk 'BEGIN{s=\"\"; for(i=0;i<390;i++) s=s \"x\"; for(i=0;i<350;i++) print s}'; \
                 printf 'marker-omega\\n'; sleep 5",
            ),
            // Wide and short on purpose: the raw ring rolls over at 128 KiB, and rolling it
            // over in as few rows as possible keeps the rows well inside the parser's own
            // retention -- which is exactly the gap the snapshot is here to cover.
            (400, 6),
        )
        .unwrap();
        let start = Instant::now();
        loop {
            let rolled = terminal.output.lock().unwrap().base_offset > 0;
            let finished = terminal
                .parser
                .lock()
                .unwrap()
                .screen()
                .contents()
                .contains("marker-omega");
            if rolled && finished {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "terminal did not roll the raw ring over and finish printing"
            );
            thread::sleep(Duration::from_millis(20));
        }

        let asked = terminal.output_since(0, Scrollback::Include);
        assert!(asked.checkpoint.is_some(), "a rolled-over offset resyncs");
        let snapshot = asked
            .scrollback
            .expect("a resync that was asked for the rows above the screen has to carry them");
        assert!(
            snapshot.contains("marker-alpha"),
            "the first line is long gone from the retained bytes and still in the parser"
        );
        assert!(
            !snapshot.contains("marker-omega"),
            "the last line is on the screen, which the checkpoint paints"
        );

        let unasked = terminal.output_since(0, Scrollback::Omit);
        assert!(unasked.checkpoint.is_some());
        assert!(
            unasked.scrollback.is_none(),
            "every poll after the first is a delta; re-sending the history would repeat it"
        );

        terminal.terminate();
    }

    /// The other half of the exclusivity: an offset still inside the ring is answered with
    /// every byte the terminal ever produced, which rebuilds the history on its own.
    #[test]
    fn a_poll_that_replays_the_whole_ring_sends_no_snapshot_beside_it() {
        let root = tempdir().unwrap();
        let terminal = ManagedTerminal::spawn(
            &CommandSpec::new("/bin/sh", root.path())
                .arg("-c")
                .arg("printf 'one\\r\\ntwo\\r\\n'; sleep 5"),
            (40, 6),
        )
        .unwrap();
        terminal.wait_for_first_output(Duration::from_secs(5));

        let poll = terminal.poll_raw(0, Scrollback::Include).unwrap();

        assert!(poll.checkpoint.is_none(), "nothing has rolled over yet");
        assert!(String::from_utf8_lossy(&poll.bytes).contains("one"));
        assert!(
            poll.scrollback.is_none(),
            "the bytes start at the terminal's first byte, so the rows would be a second copy"
        );

        terminal.terminate();
    }

    /// The daemon outlives the binary that started it, so a new client can find an old daemon
    /// and an old client a new one. Neither direction may fail to parse.
    #[test]
    fn a_poll_still_parses_across_a_daemon_that_predates_the_snapshot() {
        let asked = serde_json::to_string(&DaemonRequest::Poll {
            id: "agent|claude:one".into(),
            offset: 7,
            scrollback: true,
        })
        .unwrap();
        assert!(asked.contains("\"scrollback\":true"), "{asked}");

        let older: DaemonRequest =
            serde_json::from_str(r#"{"Poll":{"id":"agent|claude:one","offset":7}}"#).unwrap();
        let DaemonRequest::Poll {
            offset, scrollback, ..
        } = older
        else {
            panic!("a poll request has to parse as one");
        };
        assert_eq!(offset, 7);
        assert!(
            !scrollback,
            "a client that never heard of the snapshot cannot be asking for one"
        );

        let older: DaemonResponse = serde_json::from_str(
            r#"{"Poll":{"start":0,"end":0,"bytes":[],"alive":true,"exit":null}}"#,
        )
        .unwrap();
        let DaemonResponse::Poll { scrollback, .. } = older else {
            panic!("a poll response has to parse as one");
        };
        assert!(scrollback.is_none());
    }

    /// A one-shot stand-in for a daemon's socket: answers one request and exits.
    #[cfg(unix)]
    fn serve_one_request<F>(socket: &Path, answer: F)
    where
        F: FnOnce(String) -> String + Send + 'static,
    {
        let listener = UnixListener::bind(socket).unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let response = answer(line);
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(b"\n").unwrap();
        });
    }

    /// Tolerating the version gap field by field is what let an upgrade go quiet: the older
    /// daemon kept answering polls, just without the rows above the screen. Asking outright
    /// is the part that can be reported, so a daemon that cannot answer counts as behind
    /// rather than as broken.
    #[cfg(unix)]
    #[test]
    fn a_daemon_that_cannot_answer_the_version_question_is_the_answer() {
        let root = tempdir().unwrap();
        let socket = root.path().join("older.sock");
        serve_one_request(&socket, |_| {
            r#"{"Error":"unknown variant `Version`"}"#.to_owned()
        });

        let reported = daemon_protocol(&socket).unwrap();
        assert_eq!(reported, Some(0));
        assert!(
            reported.is_some_and(|protocol| protocol < DAEMON_PROTOCOL),
            "a daemon from before the question has to sort below this build"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_current_daemon_answers_the_protocol_this_build_speaks() {
        let root = tempdir().unwrap();
        let socket = root.path().join("current.sock");
        serve_one_request(&socket, |line| {
            let mut state = PtyDaemonState::default();
            let (response, _) = state.handle(serde_json::from_str(&line).unwrap());
            serde_json::to_string(&response).unwrap()
        });

        assert_eq!(daemon_protocol(&socket).unwrap(), Some(DAEMON_PROTOCOL));
    }

    #[cfg(unix)]
    #[test]
    fn no_daemon_at_all_is_not_an_out_of_date_one() {
        let root = tempdir().unwrap();
        assert_eq!(
            daemon_protocol(&root.path().join("absent.sock")).unwrap(),
            None
        );
    }
}
