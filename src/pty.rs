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

const CAPTURE_LINES: usize = 200;
const SCROLLBACK_LINES: usize = 2_000;
const CAPTURE_BYTES: usize = 16 * 1024;
const RAW_CAPTURE_BYTES: usize = 128 * 1024;
const LEASE_STALE_AFTER: Duration = Duration::from_millis(500);
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
}

impl CommandSpec {
    pub fn new(program: impl Into<OsString>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: cwd.into(),
        }
    }

    pub fn arg(mut self, value: impl Into<OsString>) -> Self {
        self.args.push(value.into());
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
        cursor: screen.cursor_position(),
        hide_cursor: screen.hide_cursor()
            || screen.scrollback() > 0
            || status_bar_scrollback.offset > 0,
    }
}

fn terminal_selected_rows(
    parser: &vt100::Parser,
    status_bar_scrollback: &StatusBarScrollback,
    first: TerminalCell,
    second: TerminalCell,
) -> Vec<(TerminalCell, String)> {
    let screen = parser.screen();
    let (rows, cols) = screen.size();
    if rows == 0 || cols == 0 {
        return Vec::new();
    }
    let mut start = first.clamped(rows, cols);
    let mut end = second.clamped(rows, cols);
    if (start.row, start.col) > (end.row, end.col) {
        std::mem::swap(&mut start, &mut end);
    }
    let view = terminal_screen_view(parser, status_bar_scrollback);
    (start.row..=end.row)
        .map(|row| {
            let first_col = if row == start.row { start.col } else { 0 };
            let last_col = if row == end.row { end.col } else { cols - 1 };
            let width = last_col - first_col + 1;
            let mut row_parser = vt100::Parser::new(1, cols, 0);
            if let Some(formatted) = view.rows.get(usize::from(row)) {
                row_parser.process(formatted);
            }
            let text =
                row_parser
                    .screen()
                    .contents_between(0, first_col, 0, last_col.saturating_add(1));
            (
                TerminalCell {
                    row,
                    col: first_col,
                },
                fit_text(&text, width),
            )
        })
        .collect()
}

fn terminal_selected_text(
    parser: &vt100::Parser,
    status_bar_scrollback: &StatusBarScrollback,
    first: TerminalCell,
    second: TerminalCell,
) -> String {
    terminal_selected_rows(parser, status_bar_scrollback, first, second)
        .into_iter()
        .map(|(_, text)| text.trim_end().to_owned())
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

struct LocalTerminal {
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    output: Arc<Mutex<OutputState>>,
    output_generation: Arc<AtomicU64>,
    parser: Arc<Mutex<vt100::Parser>>,
    status_bar_scrollback: Arc<Mutex<StatusBarScrollback>>,
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
            master: pair.master,
            writer,
            child: Arc::new(Mutex::new(child)),
            output,
            output_generation,
            parser,
            status_bar_scrollback,
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

    fn selected_text(&self, first: TerminalCell, second: TerminalCell) -> String {
        let parser = self.parser.lock().unwrap();
        let scrollback = self.status_bar_scrollback.lock().unwrap();
        terminal_selected_text(&parser, &scrollback, first, second)
    }

    fn selected_rows(
        &self,
        first: TerminalCell,
        second: TerminalCell,
    ) -> Vec<(TerminalCell, String)> {
        let parser = self.parser.lock().unwrap();
        let scrollback = self.status_bar_scrollback.lock().unwrap();
        terminal_selected_rows(&parser, &scrollback, first, second)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let (cols, rows) = normalized_size((cols, rows));
        self.master
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

    pub fn terminate(&self) {
        let mut child = self.child.lock().unwrap();
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
        }
    }

    fn output_since(&self, requested: u64) -> TerminalOutputDelta {
        let parser = self.parser.lock().unwrap();
        let scrollback = self.status_bar_scrollback.lock().unwrap();
        let state = self.output.lock().unwrap();
        let end = state.base_offset.saturating_add(state.raw.len() as u64);
        if requested < state.base_offset || requested > end {
            let checkpoint = terminal_state_checkpoint(&parser, &scrollback);
            return TerminalOutputDelta {
                start: end,
                end,
                bytes: Vec::new(),
                checkpoint: Some(checkpoint),
                status_bar_rows: Some(scrollback.rows.iter().cloned().collect()),
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
            exit: state.exit_description.clone(),
        }
    }
}

struct ScreenView {
    rows: Vec<Vec<u8>>,
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
        }
    }
}

impl WireCommandSpec {
    fn command_spec(&self) -> CommandSpec {
        CommandSpec {
            program: self.program.clone().into(),
            args: self.args.iter().map(OsString::from).collect(),
            cwd: self.cwd.clone(),
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
    Ensure {
        id: String,
        spec: WireCommandSpec,
        cols: u16,
        rows: u16,
    },
    Poll {
        id: String,
        offset: u64,
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
    Poll {
        start: u64,
        end: u64,
        bytes: Vec<u8>,
        #[serde(default)]
        checkpoint: Option<Vec<u8>>,
        #[serde(default)]
        status_bar_rows: Option<Vec<Vec<u8>>>,
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
            DaemonRequest::Poll { id, offset } => {
                let Some(terminal) = self.terminals.get(&id) else {
                    return (
                        DaemonResponse::Error(format!("unknown terminal {id}")),
                        false,
                    );
                };
                let alive = terminal.is_alive();
                let delta = terminal.output_since(offset);
                DaemonResponse::Poll {
                    start: delta.start,
                    end: delta.end,
                    bytes: delta.bytes,
                    checkpoint: delta.checkpoint,
                    status_bar_rows: delta.status_bar_rows,
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
                    DaemonResponse::Error("session lease is owned by another TUI".into())
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
            DaemonRequest::Resize { id, cols, rows } => match self.terminals.get(&id) {
                Some(terminal) => terminal
                    .resize(cols, rows)
                    .map(|()| DaemonResponse::Ok)
                    .unwrap_or_else(|error| DaemonResponse::Error(error.to_string())),
                None => DaemonResponse::Error(format!("unknown terminal {id}")),
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
                    DaemonResponse::Error("session lease is owned by another TUI".into())
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

fn response_ok(response: DaemonResponse) -> io::Result<()> {
    match response {
        DaemonResponse::Ok => Ok(()),
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
            },
        )?;
        let DaemonResponse::Poll {
            start,
            end,
            bytes,
            checkpoint,
            status_bar_rows,
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

    fn selected_text(&self, first: TerminalCell, second: TerminalCell) -> String {
        let parser = self.parser.lock().unwrap();
        let scrollback = self.status_bar_scrollback.lock().unwrap();
        terminal_selected_text(&parser, &scrollback, first, second)
    }

    fn selected_rows(
        &self,
        first: TerminalCell,
        second: TerminalCell,
    ) -> Vec<(TerminalCell, String)> {
        let parser = self.parser.lock().unwrap();
        let scrollback = self.status_bar_scrollback.lock().unwrap();
        terminal_selected_rows(&parser, &scrollback, first, second)
    }

    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let size = normalized_size((cols, rows));
        response_ok(daemon_request(
            &self.socket,
            &DaemonRequest::Resize {
                id: self.id.lock().unwrap().clone(),
                cols: size.0,
                rows: size.1,
            },
        )?)?;
        *self.size.lock().unwrap() = size;
        self.parser
            .lock()
            .unwrap()
            .screen_mut()
            .set_size(size.1, size.0);
        self.status_bar_scrollback.lock().unwrap().resize(size.1);
        self.output_generation.fetch_add(1, Ordering::Relaxed);
        Ok(())
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

    fn selected_text(&self, first: TerminalCell, second: TerminalCell) -> String {
        match &self.backend {
            TerminalBackend::Local(terminal) => terminal.selected_text(first, second),
            TerminalBackend::Remote(terminal) => terminal.selected_text(first, second),
        }
    }

    fn selected_rows(
        &self,
        first: TerminalCell,
        second: TerminalCell,
    ) -> Vec<(TerminalCell, String)> {
        match &self.backend {
            TerminalBackend::Local(terminal) => terminal.selected_rows(first, second),
            TerminalBackend::Remote(terminal) => terminal.selected_rows(first, second),
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        match &self.backend {
            TerminalBackend::Local(terminal) => terminal.resize(cols, rows),
            TerminalBackend::Remote(terminal) => terminal.resize(cols, rows),
        }
    }

    pub fn terminate(&self) {
        match &self.backend {
            TerminalBackend::Local(terminal) => terminal.terminate(),
            TerminalBackend::Remote(terminal) => terminal.terminate(),
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
    terminal: ManagedTerminal,
    name: String,
    capture_prefix: String,
}

impl ShellPane {
    fn new(terminal: ManagedTerminal, name: String) -> Self {
        Self {
            terminal,
            name,
            capture_prefix: String::new(),
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
    pub agent: Option<ManagedTerminal>,
    shells: Vec<ShellPane>,
    pub selected_shell: usize,
    selection: Option<TerminalSelection>,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PaneTarget {
    Agent,
    Shell(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TerminalSelection {
    pane: PaneTarget,
    start: TerminalCell,
    end: TerminalCell,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingAgentClick {
    event: WorkspaceMouseEvent,
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
        let status_row = rows - 1;
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

    fn spawn_shell(&self, session: &Session, size: (u16, u16)) -> io::Result<ManagedTerminal> {
        if let Some(socket) = &self.daemon_socket {
            let id = format!("shell|{}|{}", session.key, Uuid::new_v4());
            let spec = shell_command(&session.cwd);
            ManagedTerminal::ensure_remote(
                socket.clone(),
                id,
                self.lease_owner_id.clone(),
                &spec,
                size,
            )
        } else {
            spawn_shell(session, size)
        }
    }

    fn toggle_workspace_focus(
        &mut self,
        session: &Session,
        focus: WorkspaceFocus,
    ) -> io::Result<WorkspaceFocus> {
        Ok(match focus {
            WorkspaceFocus::Agent if self.shells.is_empty() => {
                let name = self.next_shell_name();
                self.shells
                    .push(ShellPane::new(self.spawn_shell(session, (80, 12))?, name));
                self.selected_shell = 0;
                WorkspaceFocus::Shell
            }
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
        let Some((pane, cell)) = pane_at(layout, col, row) else {
            return Ok(());
        };
        let button = event.button & !(4 | 8 | 16 | 32);
        let dragging = event.button & 32 != 0;
        if pane == PaneTarget::Agent
            && matches!(button, 64 | 65)
            && let Some(terminal) = self.terminal(pane)
        {
            let before = terminal.scrollback_offset();
            let amount = if button == 64 { 3 } else { -3 };
            let after = terminal.scroll_viewport(amount);
            if before > 0 || after > 0 {
                return Ok(());
            }
        }
        if pane == PaneTarget::Agent && event.button & 4 == 0 {
            let mouse_protocol = self.terminal(pane).map(ManagedTerminal::mouse_protocol);
            if let Some((mode, encoding)) = mouse_protocol
                && mode != vt100::MouseProtocolMode::None
            {
                if button == 0 && event.pressed && !dragging {
                    self.selection = None;
                    self.pending_agent_click = Some(PendingAgentClick { event, cell });
                    return Ok(());
                }
                if button == 0 && event.pressed && dragging {
                    if let Some(pending) = self.pending_agent_click.take() {
                        self.selection = Some(TerminalSelection {
                            pane,
                            start: pending.cell,
                            end: cell,
                        });
                        return Ok(());
                    }
                    if let Some(selection) = &mut self.selection
                        && selection.pane == pane
                    {
                        selection.end = cell;
                        return Ok(());
                    }
                }
                if button == 0 && !event.pressed {
                    if let Some(selection) = &mut self.selection
                        && selection.pane == pane
                    {
                        selection.end = cell;
                        self.pending_agent_click = None;
                        self.copy_selection_to_clipboard();
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
                if let Some(terminal) = self.terminal(pane)
                    && pane == PaneTarget::Agent
                    && terminal.alternate_screen()
                    && let Some(bytes) = alternate_screen_scroll(button)
                {
                    terminal.write(&bytes)?;
                    return Ok(());
                }
                if let Some(terminal) = self.terminal(pane) {
                    let amount = if button == 64 { 3 } else { -3 };
                    terminal.scroll_viewport(amount);
                }
            }
            0 if event.pressed && event.button & 32 == 0 => {
                self.selection = Some(TerminalSelection {
                    pane,
                    start: cell,
                    end: cell,
                });
            }
            0 if event.pressed && event.button & 32 != 0 => {
                if let Some(selection) = &mut self.selection
                    && selection.pane == pane
                {
                    selection.end = cell;
                }
            }
            0 if !event.pressed => {
                if let Some(selection) = &mut self.selection
                    && selection.pane == pane
                {
                    selection.end = cell;
                }
                self.copy_selection_to_clipboard();
            }
            _ => {}
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

    fn terminal(&self, pane: PaneTarget) -> Option<&ManagedTerminal> {
        match pane {
            PaneTarget::Agent => self.agent.as_ref(),
            PaneTarget::Shell(index) => self.shells.get(index).map(|pane| &pane.terminal),
        }
    }

    fn selected_text(&self) -> Option<String> {
        let selection = self.selection?;
        self.terminal(selection.pane)
            .map(|terminal| terminal.selected_text(selection.start, selection.end))
            .filter(|text| !text.is_empty())
    }

    fn attach_workspace<F, G>(
        &mut self,
        session: &Session,
        mut focus: WorkspaceFocus,
        bindings: WorkspaceBindings,
        mut observe: F,
        mut validate_lease: G,
    ) -> io::Result<WorkspaceExit>
    where
        F: FnMut(Option<WorkspaceSearchUpdate>) -> WorkspaceChrome,
        G: FnMut() -> io::Result<()>,
    {
        let mut size = normalized_size(crossterm::terminal::size().unwrap_or((120, 40)));
        let mut stdout = io::stdout().lock();
        let mut keyboard_enhancement_enabled = false;
        stdout.write_all(ENABLE_MOUSE_REPORTING)?;
        sync_keyboard_enhancement(&mut stdout, &mut keyboard_enhancement_enabled, focus)?;
        stdout.write_all(b"\x1b[2J\x1b[H")?;
        stdout.flush()?;
        let result = (|| {
            let mut exit = WorkspaceExit::Dashboard;
            let render_bindings = bindings.clone();
            let mut input_router = WorkspaceInputRouter {
                pending: Vec::new(),
                bindings,
            };
            let mut last_signature = Vec::new();
            let mut last_layout_key = None;
            let mut clear_next_frame = true;
            let mut search = None;
            let mut help_open = false;
            let mut chrome = observe(None);
            'workspace: loop {
                validate_lease()?;
                if focus == WorkspaceFocus::Agent {
                    match self.agent.as_ref() {
                        Some(agent) if agent.is_alive() => {}
                        Some(agent) => {
                            self.notice = Some(agent.exit_description().map_or_else(
                                || "agent exited; showing the latest session preview".into(),
                                |exit| {
                                    format!(
                                        "agent exited ({exit}); showing the latest session preview"
                                    )
                                },
                            ));
                            focus = WorkspaceFocus::Sessions;
                            last_signature.clear();
                        }
                        None => {
                            exit = WorkspaceExit::ActivateSession;
                            break 'workspace;
                        }
                    }
                }
                sync_keyboard_enhancement(&mut stdout, &mut keyboard_enhancement_enabled, focus)?;
                let next_chrome = observe(None);
                if next_chrome != chrome {
                    chrome = next_chrome;
                    last_signature.clear();
                }
                if self.shells.is_empty() {
                    self.selected_shell = 0;
                    if focus == WorkspaceFocus::Shell {
                        focus = if self.agent.is_some() {
                            WorkspaceFocus::Agent
                        } else {
                            WorkspaceFocus::Sessions
                        };
                    }
                } else {
                    self.selected_shell = self.selected_shell.min(self.shells.len() - 1);
                }

                let new_size = normalized_size(crossterm::terminal::size().unwrap_or(size));
                if new_size != size {
                    size = new_size;
                    last_signature.clear();
                    clear_next_frame = true;
                }
                let mut layout =
                    WorkspaceLayout::new(size.0, size.1, self.shells.len(), self.selected_shell);
                layout.apply_options(self.maximized, self.shell_height_adjust);
                let layout_key = (
                    size,
                    self.shells.len(),
                    self.selected_shell,
                    self.maximized,
                    self.shell_height_adjust,
                );
                if last_layout_key != Some(layout_key) {
                    self.resize_workspace(&layout)?;
                    last_layout_key = Some(layout_key);
                    last_signature.clear();
                    clear_next_frame = true;
                }
                let signature = self.render_signature(size, &layout, focus);
                if signature != last_signature {
                    render_workspace_with_bindings(
                        &mut stdout,
                        self,
                        &chrome,
                        &layout,
                        WorkspaceRenderState {
                            focus,
                            search: search
                                .as_ref()
                                .map(|search: &WorkspaceSearch| search.value.as_str()),
                            help: help_open,
                        },
                        &render_bindings,
                        clear_next_frame,
                    )?;
                    last_signature = signature;
                    clear_next_frame = false;
                }

                let input = match poll_terminal_input(Duration::from_millis(10))? {
                    PolledTerminalInput::Pending => {
                        if let Some(bytes) = input_router.flush() {
                            self.write_focused(focus, &bytes)?;
                        }
                        continue;
                    }
                    PolledTerminalInput::EndOfFile => break,
                    PolledTerminalInput::Bytes(input) => input,
                };
                if help_open {
                    if input == b"\x1b"
                        || render_bindings.command(&input) == Some(WorkspaceCommand::Help)
                    {
                        help_open = false;
                    }
                    last_signature.clear();
                    continue 'workspace;
                }
                if let Some(active_search) = search.as_mut() {
                    let (search_input, changed) =
                        apply_workspace_search_input(active_search, &input);
                    match search_input {
                        WorkspaceSearchInput::Cancel => {
                            let update = WorkspaceSearchUpdate::Cancel {
                                query: active_search.original_query.clone(),
                                selected_session_key: active_search
                                    .original_selected_session_key
                                    .clone(),
                            };
                            let _ = observe(Some(update));
                            exit = WorkspaceExit::RefreshSessions;
                            break 'workspace;
                        }
                        WorkspaceSearchInput::Commit => {
                            if changed {
                                chrome = observe(Some(WorkspaceSearchUpdate::Preview(
                                    active_search.value.clone(),
                                )));
                            }
                            search = None;
                        }
                        WorkspaceSearchInput::Editing if changed => {
                            chrome = observe(Some(WorkspaceSearchUpdate::Preview(
                                active_search.value.clone(),
                            )));
                        }
                        WorkspaceSearchInput::Editing => {}
                    }
                    last_signature.clear();
                    continue 'workspace;
                }
                for routed in input_router.route(&input, focus) {
                    if let WorkspaceInput::Mouse(event) = routed {
                        self.handle_mouse(&layout, event)?;
                        last_signature.clear();
                        continue;
                    }
                    let WorkspaceInput::Command(command) = routed else {
                        if let WorkspaceInput::Forward(bytes) = routed {
                            if focus == WorkspaceFocus::Sessions {
                                match session_list_input(&bytes) {
                                    Some(SessionListInput::Previous) => {
                                        exit = WorkspaceExit::PreviousSession(focus);
                                        break 'workspace;
                                    }
                                    Some(SessionListInput::Next) => {
                                        exit = WorkspaceExit::NextSession(focus);
                                        break 'workspace;
                                    }
                                    Some(SessionListInput::Activate) => {
                                        exit = WorkspaceExit::ActivateSession;
                                        break 'workspace;
                                    }
                                    Some(SessionListInput::NewSession) => {
                                        exit = WorkspaceExit::NewSession;
                                        break 'workspace;
                                    }
                                    Some(SessionListInput::OpenShell) => {
                                        exit = WorkspaceExit::OpenShell;
                                        break 'workspace;
                                    }
                                    Some(SessionListInput::ToggleArchive) => {
                                        exit = WorkspaceExit::ToggleArchive;
                                        break 'workspace;
                                    }
                                    None => {}
                                }
                                continue;
                            }
                            if self
                                .focused_terminal(focus)
                                .is_some_and(|terminal| terminal.scrollback_offset() > 0)
                            {
                                self.focused_terminal(focus).unwrap().scroll_to_live_tail();
                            }
                            self.write_focused(focus, &bytes)?;
                        }
                        continue;
                    };
                    match command {
                        WorkspaceCommand::Dashboard => break 'workspace,
                        WorkspaceCommand::Alert => {
                            exit = WorkspaceExit::Alert;
                            break 'workspace;
                        }
                        WorkspaceCommand::Search => {
                            search = Some(WorkspaceSearch {
                                value: chrome.search_query.clone(),
                                original_query: chrome.search_query.clone(),
                                original_selected_session_key: chrome.selected_session_key.clone(),
                            });
                        }
                        WorkspaceCommand::Help => {
                            help_open = true;
                        }
                        WorkspaceCommand::PreviousSession => {
                            exit = WorkspaceExit::PreviousSession(focus);
                            break 'workspace;
                        }
                        WorkspaceCommand::NextSession => {
                            exit = WorkspaceExit::NextSession(focus);
                            break 'workspace;
                        }
                        WorkspaceCommand::SelectShell(index) => {
                            if index < self.shells.len() {
                                self.selected_shell = index;
                                focus = WorkspaceFocus::Shell;
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
                                render_bindings.label("focus")
                            ));
                            exit = WorkspaceExit::FocusShell;
                            break 'workspace;
                        }
                        WorkspaceCommand::ToggleShellArea => {
                            self.maximized = Some(PaneTarget::Agent);
                            self.notice = Some(format!(
                                "agent maximized · {} changes focus",
                                render_bindings.label("focus")
                            ));
                            exit = WorkspaceExit::ActivateSession;
                            break 'workspace;
                        }
                        WorkspaceCommand::GrowShell => {
                            if self.maximized.is_none() && !self.shells.is_empty() {
                                self.shell_height_adjust =
                                    self.shell_height_adjust.saturating_add(2).min(20);
                                self.notice = Some("shell area enlarged".into());
                                clear_next_frame = true;
                            }
                        }
                        WorkspaceCommand::ShrinkShell => {
                            if self.maximized.is_none() && !self.shells.is_empty() {
                                self.shell_height_adjust =
                                    self.shell_height_adjust.saturating_sub(2).max(-10);
                                self.notice = Some("shell area reduced".into());
                                clear_next_frame = true;
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
                            if focus == WorkspaceFocus::Sessions {
                                exit = WorkspaceExit::ActivateSession;
                                break 'workspace;
                            }
                            focus = self.toggle_workspace_focus(session, focus)?;
                            if self.maximized.is_some() {
                                self.maximized = match focus {
                                    WorkspaceFocus::Sessions => None,
                                    WorkspaceFocus::Agent => Some(PaneTarget::Agent),
                                    WorkspaceFocus::Shell => {
                                        Some(PaneTarget::Shell(self.selected_shell))
                                    }
                                };
                            }
                        }
                        WorkspaceCommand::NewShell => {
                            if focus == WorkspaceFocus::Sessions {
                                exit = WorkspaceExit::OpenShell;
                                break 'workspace;
                            }
                            let name = self.next_shell_name();
                            self.shells
                                .push(ShellPane::new(self.spawn_shell(session, (80, 12))?, name));
                            self.selected_shell = self.shells.len() - 1;
                            focus = WorkspaceFocus::Shell;
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
                                focus = WorkspaceFocus::Shell;
                                if self.maximized.is_some() {
                                    self.maximized = Some(PaneTarget::Shell(self.selected_shell));
                                }
                            } else {
                                self.notice = Some(format!(
                                    "no shell is open; press {} to create one",
                                    render_bindings.label("new_shell")
                                ));
                            }
                        }
                        WorkspaceCommand::NextShell => {
                            if !self.shells.is_empty() {
                                self.selected_shell = (self.selected_shell + 1) % self.shells.len();
                                focus = WorkspaceFocus::Shell;
                                if self.maximized.is_some() {
                                    self.maximized = Some(PaneTarget::Shell(self.selected_shell));
                                }
                            } else {
                                self.notice = Some(format!(
                                    "no shell is open; press {} to create one",
                                    render_bindings.label("new_shell")
                                ));
                            }
                        }
                        WorkspaceCommand::CloseShell => {
                            if self.shells.is_empty() {
                                self.notice = Some("no shell to close".into());
                                continue;
                            }
                            match shell_close_action(focus) {
                                ShellCloseAction::Ignore => {
                                    self.notice = Some("focus a shell before closing it".into());
                                }
                                ShellCloseAction::Close => {
                                    self.shells.remove(self.selected_shell).terminal.terminate();
                                    if self.shells.is_empty() {
                                        self.selected_shell = 0;
                                        focus = WorkspaceFocus::Agent;
                                        self.maximized = None;
                                    } else {
                                        self.selected_shell =
                                            self.selected_shell.min(self.shells.len() - 1);
                                        if self.maximized.is_some() {
                                            self.maximized =
                                                Some(PaneTarget::Shell(self.selected_shell));
                                        }
                                    }
                                }
                            }
                        }
                        WorkspaceCommand::ScrollUp => {
                            if let Some(terminal) = self.focused_terminal(focus) {
                                terminal
                                    .scroll_viewport(
                                        focused_viewport_height(&layout, self, focus) as isize
                                    );
                            }
                        }
                        WorkspaceCommand::ScrollDown => {
                            if let Some(terminal) = self.focused_terminal(focus) {
                                terminal.scroll_viewport(
                                    -(focused_viewport_height(&layout, self, focus) as isize),
                                );
                            }
                        }
                        WorkspaceCommand::LiveTail => {
                            if let Some(terminal) = self.focused_terminal(focus) {
                                terminal.scroll_to_live_tail();
                            }
                        }
                    }
                }
                last_signature.clear();
            }
            Ok(exit)
        })();
        let restore = if keyboard_enhancement_enabled {
            stdout.write_all(DISABLE_KEYBOARD_ENHANCEMENT)
        } else {
            Ok(())
        }
        .and_then(|()| stdout.write_all(DISABLE_MOUSE_REPORTING))
        .and_then(|()| stdout.write_all(b"\x1b[0m\x1b[?25h"))
        .and_then(|()| stdout.flush());
        match result {
            Ok(exit) => restore.map(|()| exit),
            Err(error) => Err(error),
        }
    }

    fn resize_workspace(&self, layout: &WorkspaceLayout) -> io::Result<()> {
        if let Some(agent) = &self.agent {
            agent.resize(layout.agent.width, layout.agent.height)?;
        }
        for (index, rect) in &layout.shell_panes {
            self.shells[*index]
                .terminal
                .resize(rect.width, rect.height.saturating_sub(1))?;
        }
        Ok(())
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
            WorkspaceFocus::Agent => self.agent.as_ref(),
            WorkspaceFocus::Shell => self
                .shells
                .get(self.selected_shell)
                .map(|pane| &pane.terminal),
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
                .map_or(0, ManagedTerminal::output_generation),
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
                terminals.agent.as_ref(),
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
                    render_selection(stdout, agent, selection, layout.agent)?;
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
                terminals.shells.get(*index).map(|pane| &pane.terminal),
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
                render_selection(stdout, &shell.terminal, selection, terminal_rect)?;
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
        let controls_text = if let Some(notification) = &chrome.notification {
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
                "  ALERT · {notification}  ·  {} jump  {} dashboard",
                alert,
                bindings.label("dashboard")
            )
        } else {
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
            terminals.notice.as_deref().map_or_else(
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
        )
        };
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
    position_workspace_cursor(stdout, terminals, layout, focus)?;
    stdout.flush()
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
    terminal: &ManagedTerminal,
    selection: TerminalSelection,
    rect: PaneRect,
) -> io::Result<()> {
    for (cell, text) in terminal.selected_rows(selection.start, selection.end) {
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
    terminals: HashMap<String, SessionTerminals>,
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

    pub fn agent(&self, key: &str) -> Option<&ManagedTerminal> {
        self.terminals.get(key)?.agent.as_ref()
    }

    pub fn shell_capture(&self, key: &str) -> Option<String> {
        let terminals = self.terminals.get(key)?;
        terminals
            .shells
            .get(terminals.selected_shell)
            .map(ShellPane::command_capture)
    }

    pub fn shell_count(&self, key: &str) -> usize {
        self.terminals
            .get(key)
            .map_or(0, |terminal| terminal.shells.len())
    }

    pub fn set_notice(&mut self, key: &str, notice: String) {
        self.terminals.entry(key.to_owned()).or_default().notice = Some(notice);
    }

    pub fn terminate_agent(&mut self, key: &str) {
        if let Some(agent) = self
            .terminals
            .get_mut(key)
            .and_then(|terminals| terminals.agent.take())
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
        let terminals = self.terminals.entry(session.key.clone()).or_default();
        terminals.daemon_socket.clone_from(&daemon_socket);
        terminals
            .lease_owner_id
            .clone_from(&self.lease_owner.instance_id);
        let Some(socket) = &daemon_socket else {
            return Ok(());
        };

        if terminals.shells.is_empty()
            && let DaemonResponse::List(ids) = daemon_request(
                socket,
                &DaemonRequest::List {
                    prefix: format!("shell|{}|", session.key),
                },
            )?
        {
            for id in ids {
                let name = terminals.next_shell_name();
                terminals.shells.push(ShellPane::new(
                    ManagedTerminal::connect_remote(
                        socket.clone(),
                        id,
                        self.lease_owner.instance_id.clone(),
                        size,
                    )?,
                    name,
                ));
            }
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
            terminals.agent = Some(ManagedTerminal::connect_remote(
                socket.clone(),
                id,
                self.lease_owner.instance_id.clone(),
                size,
            )?);
        }
        Ok(())
    }

    pub fn ensure_agent(
        &mut self,
        session: &Session,
        current_exe: &Path,
        new_session: bool,
        size: (u16, u16),
    ) -> io::Result<&ManagedTerminal> {
        self.ensure_session_view(session, current_exe, size)?;
        let daemon_socket = self.daemon_socket.clone();
        let agent_id = format!("agent|{}", session.key);
        let terminals = self.terminals.get_mut(&session.key).unwrap();
        let needs_spawn = terminals
            .agent
            .as_ref()
            .is_none_or(|value| !value.is_alive());
        if needs_spawn {
            let spec = agent_command(&self.config, session, current_exe, new_session);
            terminals.agent = Some(if let Some(socket) = daemon_socket {
                ManagedTerminal::ensure_remote(
                    socket,
                    agent_id,
                    self.lease_owner.instance_id.clone(),
                    &spec,
                    size,
                )?
            } else {
                ManagedTerminal::spawn(&spec, size)?
            });
        }
        Ok(terminals.agent.as_ref().unwrap())
    }

    pub fn add_shell(&mut self, session: &Session, size: (u16, u16)) -> io::Result<usize> {
        let terminals = self.terminals.entry(session.key.clone()).or_default();
        let name = terminals.next_shell_name();
        let shell = terminals.spawn_shell(session, size)?;
        terminals.shells.push(ShellPane::new(shell, name));
        terminals.selected_shell = terminals.shells.len() - 1;
        Ok(terminals.selected_shell)
    }

    pub fn attach_workspace<F>(
        &mut self,
        session: &Session,
        focus: WorkspaceFocus,
        force_takeover: bool,
        observe: F,
    ) -> io::Result<WorkspaceExit>
    where
        F: FnMut(Option<WorkspaceSearchUpdate>) -> WorkspaceChrome,
    {
        let bindings = WorkspaceBindings::from_config(&self.config);
        let needs_lease = focus != WorkspaceFocus::Sessions;
        let leased = if needs_lease && let Some(socket) = &self.daemon_socket {
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
        let lease_socket = self.daemon_socket.clone();
        let lease_session_key = session.key.clone();
        let lease_owner_id = self.lease_owner.instance_id.clone();
        let terminals = self
            .terminals
            .get_mut(&session.key)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "agent terminal is not open"))?;
        let result = terminals.attach_workspace(session, focus, bindings, observe, || {
            if leased && let Some(socket) = &lease_socket {
                response_ok(daemon_request(
                    socket,
                    &DaemonRequest::ValidateLease {
                        session_key: lease_session_key.clone(),
                        owner_id: lease_owner_id.clone(),
                    },
                )?)
            } else {
                Ok(())
            }
        });
        if leased && let Some(socket) = &self.daemon_socket {
            let _ = daemon_request(
                socket,
                &DaemonRequest::Release {
                    session_key: session.key.clone(),
                    owner_id: self.lease_owner.instance_id.clone(),
                },
            );
        }
        result
    }

    pub fn alive_keys(&self) -> Vec<String> {
        self.terminals
            .iter()
            .filter(|(_, terminals)| {
                terminals
                    .agent
                    .as_ref()
                    .is_some_and(ManagedTerminal::is_alive)
            })
            .map(|(key, _)| key.clone())
            .collect()
    }

    pub fn agent_alive(&self, key: &str) -> bool {
        self.agent(key).is_some_and(ManagedTerminal::is_alive)
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
            let mut spec = CommandSpec::new("claude", &session.cwd)
                .arg("--settings")
                .arg(hooks);
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
    let CommandSpec { args, cwd, .. } = spec;
    let command = config.provider_command(session.agent, args);
    CommandSpec {
        program: command.program,
        args: command.args,
        cwd,
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

pub fn bracketed_paste(value: &str) -> Vec<u8> {
    let safe = value.replace('\x1b', "");
    let mut output = Vec::with_capacity(safe.len() + 12);
    output.extend_from_slice(b"\x1b[200~");
    output.extend_from_slice(safe.as_bytes());
    output.extend_from_slice(b"\x1b[201~");
    output
}

fn plain_text(bytes: &[u8]) -> String {
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

        let delta = terminal.output_since(0);
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

        let delta = terminal.output_since(0);
        assert!(delta.checkpoint.is_some());
        assert_eq!(
            delta.status_bar_rows,
            Some(vec![b"retained-before-128-kib".to_vec()])
        );
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
            agent: Some(agent),
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
            agent: Some(agent),
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
            agent: Some(agent),
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

        let first = TerminalCell { row: 0, col: 0 };
        let last = TerminalCell { row: 0, col: 5 };
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
            agent: Some(agent),
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
            agent: Some(agent),
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
            agent: Some(agent),
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
            agent: Some(agent),
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
            TerminalCell { row: 0, col: 6 },
            TerminalCell { row: 1, col: 4 },
        );

        assert_eq!(selected, "beta\ngamma");
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
                TerminalCell { row: 0, col: 0 },
                TerminalCell { row: 0, col: 13 },
            ),
            "claude-visible"
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
            agent: Some(agent),
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
            shells: vec![ShellPane::new(shell, "shell 1".into())],
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

        let shell = terminals
            .toggle_workspace_focus(&session, WorkspaceFocus::Agent)
            .unwrap();
        let sessions = terminals.toggle_workspace_focus(&session, shell).unwrap();
        let agent = terminals
            .toggle_workspace_focus(&session, sessions)
            .unwrap();

        assert_eq!(shell, WorkspaceFocus::Shell);
        assert_eq!(sessions, WorkspaceFocus::Sessions);
        assert_eq!(agent, WorkspaceFocus::Agent);
        assert_eq!(terminals.shells.len(), 1);
        assert_eq!(terminals.selected_shell, 0);
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
            shells: vec![ShellPane::new(shell, "shell 1".into())],
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
            agent: Some(agent),
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
            agent: Some(agent),
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
}
