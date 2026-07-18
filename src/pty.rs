use std::{
    collections::{HashMap, VecDeque},
    env,
    ffi::{OsStr, OsString},
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use unicode_width::UnicodeWidthChar;
use uuid::Uuid;

use crate::{
    clipboard,
    config::{AgentConsoleConfig, format_key_label},
    model::{AgentKind, Session},
    store::{ensure_private_dir, make_private_file},
};

const CAPTURE_LINES: usize = 200;
const CAPTURE_BYTES: usize = 16 * 1024;
const RAW_CAPTURE_BYTES: usize = 128 * 1024;
const LEASE_STALE_AFTER: Duration = Duration::from_millis(500);
const ENABLE_MOUSE_REPORTING: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const DISABLE_MOUSE_REPORTING: &[u8] =
    b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1016l";

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

struct LocalTerminal {
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    output: Arc<Mutex<OutputState>>,
    output_generation: Arc<AtomicU64>,
    parser: Arc<Mutex<vt100::Parser>>,
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
            CAPTURE_LINES,
        )));
        let parser_for_thread = Arc::clone(&parser);

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
                                parser.process(bytes);
                                query_router.route(bytes, parser.screen().cursor_position())
                            };
                            if !responses.is_empty() {
                                let mut writer = writer_for_thread.lock().unwrap();
                                for response in responses {
                                    let _ = writer.write_all(&response);
                                }
                                let _ = writer.flush();
                            }
                            {
                                let mut state = output_for_thread.lock().unwrap();
                                state.raw.extend(bytes);
                                while state.raw.len() > RAW_CAPTURE_BYTES {
                                    state.raw.pop_front();
                                    state.base_offset = state.base_offset.saturating_add(1);
                                }
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
        let screen = parser.screen();
        let (_, cols) = screen.size();
        let scrollback = screen.scrollback();
        ScreenView {
            rows: screen.rows_formatted(0, cols).collect(),
            cursor: screen.cursor_position(),
            hide_cursor: screen.hide_cursor() || scrollback > 0,
        }
    }

    fn scroll_viewport(&self, rows: isize) -> usize {
        let mut parser = self.parser.lock().unwrap();
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
        parser.screen_mut().set_scrollback(0);
        drop(parser);
        self.output_generation.fetch_add(1, Ordering::Relaxed);
    }

    fn scrollback_offset(&self) -> usize {
        self.parser.lock().unwrap().screen().scrollback()
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
        let screen = parser.screen();
        let (rows, cols) = screen.size();
        if rows == 0 || cols == 0 {
            return String::new();
        }
        let mut start = first.clamped(rows, cols);
        let mut end = second.clamped(rows, cols);
        if (start.row, start.col) > (end.row, end.col) {
            std::mem::swap(&mut start, &mut end);
        }
        screen.contents_between(start.row, start.col, end.row, end.col.saturating_add(1))
    }

    fn selected_rows(
        &self,
        first: TerminalCell,
        second: TerminalCell,
    ) -> Vec<(TerminalCell, String)> {
        let parser = self.parser.lock().unwrap();
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
        (start.row..=end.row)
            .map(|row| {
                let first_col = if row == start.row { start.col } else { 0 };
                let last_col = if row == end.row { end.col } else { cols - 1 };
                let width = last_col - first_col + 1;
                let text = screen
                    .rows(first_col, width)
                    .nth(usize::from(row))
                    .unwrap_or_default();
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
        self.output_generation.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn terminate(&self) {
        let mut child = self.child.lock().unwrap();
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
        }
    }

    fn output_since(&self, requested: u64) -> (u64, u64, Vec<u8>, Option<String>) {
        let state = self.output.lock().unwrap();
        let start = requested.max(state.base_offset);
        let skip = start.saturating_sub(state.base_offset) as usize;
        let bytes = state.raw.iter().skip(skip).copied().collect::<Vec<_>>();
        let end = state.base_offset.saturating_add(state.raw.len() as u64);
        (start, end, bytes, state.exit_description.clone())
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
                if let std::collections::hash_map::Entry::Vacant(entry) = self.terminals.entry(id) {
                    match LocalTerminal::spawn(&spec.command_spec(), (cols, rows)) {
                        Ok(terminal) => {
                            entry.insert(terminal);
                        }
                        Err(error) => return (DaemonResponse::Error(error.to_string()), false),
                    }
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
                let (start, end, bytes, exit) = terminal.output_since(offset);
                DaemonResponse::Poll {
                    start,
                    end,
                    bytes,
                    alive,
                    exit,
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

fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: signal 0 performs only an existence/permission check.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn daemon_request(socket: &Path, request: &DaemonRequest) -> io::Result<DaemonResponse> {
    let mut stream = UnixStream::connect(socket)?;
    serde_json::to_writer(&mut stream, request).map_err(io::Error::other)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    serde_json::from_str(&line).map_err(io::Error::other)
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

pub fn stop_pty_daemon(socket: &Path) -> io::Result<()> {
    response_ok(daemon_request(socket, &DaemonRequest::Shutdown)?)
}

pub fn daemon_health(socket: &Path) -> io::Result<Option<()>> {
    if !socket.exists() {
        return Ok(None);
    }
    response_ok(daemon_request(socket, &DaemonRequest::Ping)?).map(Some)
}

struct RemoteTerminal {
    socket: PathBuf,
    id: Mutex<String>,
    owner_id: String,
    offset: Mutex<u64>,
    output: Mutex<OutputState>,
    output_generation: AtomicU64,
    parser: Mutex<vt100::Parser>,
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
            parser: Mutex::new(vt100::Parser::new(size.1, size.0, CAPTURE_LINES)),
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
        if start != requested {
            let size = *self.size.lock().unwrap();
            *self.parser.lock().unwrap() = vt100::Parser::new(size.1, size.0, CAPTURE_LINES);
            let mut output = self.output.lock().unwrap();
            output.raw.clear();
            output.base_offset = start;
        }
        if !bytes.is_empty() {
            self.parser.lock().unwrap().process(&bytes);
        }
        let mut output = self.output.lock().unwrap();
        let changed = !bytes.is_empty() || output.exited == alive;
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
        let screen = parser.screen();
        let (_, cols) = screen.size();
        let scrollback = screen.scrollback();
        ScreenView {
            rows: screen.rows_formatted(0, cols).collect(),
            cursor: screen.cursor_position(),
            hide_cursor: screen.hide_cursor() || scrollback > 0,
        }
    }

    fn scroll_viewport(&self, rows: isize) -> usize {
        let _ = self.sync();
        let mut parser = self.parser.lock().unwrap();
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
        self.parser.lock().unwrap().screen_mut().set_scrollback(0);
        self.output_generation.fetch_add(1, Ordering::Relaxed);
    }

    fn scrollback_offset(&self) -> usize {
        self.parser.lock().unwrap().screen().scrollback()
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
        let screen = parser.screen();
        let (rows, cols) = screen.size();
        if rows == 0 || cols == 0 {
            return String::new();
        }
        let mut start = first.clamped(rows, cols);
        let mut end = second.clamped(rows, cols);
        if (start.row, start.col) > (end.row, end.col) {
            std::mem::swap(&mut start, &mut end);
        }
        screen.contents_between(start.row, start.col, end.row, end.col.saturating_add(1))
    }

    fn selected_rows(
        &self,
        first: TerminalCell,
        second: TerminalCell,
    ) -> Vec<(TerminalCell, String)> {
        let parser = self.parser.lock().unwrap();
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
        (start.row..=end.row)
            .map(|row| {
                let first_col = if row == start.row { start.col } else { 0 };
                let last_col = if row == end.row { end.col } else { cols - 1 };
                let width = last_col - first_col + 1;
                let text = screen
                    .rows(first_col, width)
                    .nth(usize::from(row))
                    .unwrap_or_default();
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
    Confirm,
    Close,
}

fn shell_close_action(
    focus: WorkspaceFocus,
    shell_alive: bool,
    already_confirmed: bool,
) -> ShellCloseAction {
    if !matches!(focus, WorkspaceFocus::Shell | WorkspaceFocus::Sessions) {
        ShellCloseAction::Ignore
    } else if shell_alive && !already_confirmed {
        ShellCloseAction::Confirm
    } else {
        ShellCloseAction::Close
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceExit {
    Dashboard,
    Alert,
    ActivateSession,
    NewSession,
    OpenShell,
    ToggleArchive,
    PreviousSession(WorkspaceFocus),
    NextSession(WorkspaceFocus),
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceChrome {
    pub sessions: Vec<String>,
    pub selected: usize,
    pub preview: Vec<String>,
    pub notification: Option<String>,
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
        point_in_rect(col, row, terminal).then_some((
            PaneTarget::Shell(*index),
            TerminalCell {
                row: row - terminal.top,
                col: col - terminal.left,
            },
        ))
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
    PreviousSession,
    NextSession,
    SelectShell(usize),
    RenameShell,
    ToggleMaximize,
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
            ("previous_session", WorkspaceCommand::PreviousSession),
            ("next_session", WorkspaceCommand::NextSession),
            ("rename_shell", WorkspaceCommand::RenameShell),
            ("maximize", WorkspaceCommand::ToggleMaximize),
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
                if let Some(sequence) = workspace_key_sequence(&label) {
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
                if let Some(sequence) = workspace_key_sequence(&label) {
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
        self.labels.get(action).map_or("unbound", String::as_str)
    }
}

fn workspace_key_sequence(label: &str) -> Option<Vec<u8>> {
    let lower = label.to_ascii_lowercase();
    let bytes = match lower.as_str() {
        "ctrl-up" => b"\x1b[1;5A".to_vec(),
        "ctrl-down" => b"\x1b[1;5B".to_vec(),
        "shift-pageup" => b"\x1b[5;2~".to_vec(),
        "shift-pagedown" => b"\x1b[6;2~".to_vec(),
        "shift-end" => b"\x1b[1;2F".to_vec(),
        value if value.starts_with("alt-") && value.len() > 4 => {
            let mut sequence = vec![0x1b];
            sequence.extend_from_slice(&value.as_bytes()[4..]);
            sequence
        }
        value if value.starts_with("ctrl-") && value.len() == 6 => {
            vec![value.as_bytes()[5] & 0x1f]
        }
        value if value.chars().count() == 1 => value.as_bytes().to_vec(),
        _ => return None,
    };
    Some(bytes)
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
        | WorkspaceCommand::SelectShell(_)
        | WorkspaceCommand::RenameShell
        | WorkspaceCommand::ToggleMaximize
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
            let shell = env::var_os("SHELL").unwrap_or_else(|| OsString::from("/bin/sh"));
            let spec = CommandSpec::new(shell, &session.cwd).arg("-l");
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
        match button {
            64 | 65 => {
                if let Some(terminal) = self.terminal(pane)
                    && pane == PaneTarget::Agent
                {
                    let (mode, encoding) = terminal.mouse_protocol();
                    if mode != vt100::MouseProtocolMode::None {
                        if let Some(bytes) = encoded_child_mouse_event(event, cell, encoding) {
                            terminal.write(&bytes)?;
                            return Ok(());
                        }
                    } else if terminal.alternate_screen()
                        && let Some(bytes) = alternate_screen_scroll(button)
                    {
                        terminal.write(&bytes)?;
                        return Ok(());
                    }
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
                self.notice = match self.selected_text() {
                    Some(text) => match clipboard::copy(&text) {
                        Ok(()) => {
                            Some(format!("selection copied · {} chars", text.chars().count()))
                        }
                        Err(error) => Some(format!("copy failed: {error}")),
                    },
                    None => Some("nothing selected".into()),
                };
            }
            _ => {}
        }
        Ok(())
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
        F: FnMut() -> WorkspaceChrome,
        G: FnMut() -> io::Result<()>,
    {
        let mut size = normalized_size(crossterm::terminal::size().unwrap_or((120, 40)));
        let mut stdout = io::stdout().lock();
        stdout.write_all(ENABLE_MOUSE_REPORTING)?;
        stdout.write_all(b"\x1b[2J\x1b[H")?;
        stdout.flush()?;
        let result = (|| {
            let mut exit = WorkspaceExit::Dashboard;
            let mut input = [0_u8; 4096];
            let render_bindings = bindings.clone();
            let mut input_router = WorkspaceInputRouter {
                pending: Vec::new(),
                bindings,
            };
            let mut last_signature = Vec::new();
            let mut last_layout_key = None;
            let mut clear_next_frame = true;
            let mut rename_input: Option<Vec<u8>> = None;
            let mut close_confirmation: Option<usize> = None;
            let mut chrome = observe();
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
                let next_chrome = observe();
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
                        focus,
                        &render_bindings,
                        clear_next_frame,
                    )?;
                    last_signature = signature;
                    clear_next_frame = false;
                }

                let mut descriptor = libc::pollfd {
                    fd: libc::STDIN_FILENO,
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: descriptor points to one valid pollfd for the duration of the call.
                let ready = unsafe { libc::poll(&mut descriptor, 1, 10) };
                if ready < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                if ready == 0 || descriptor.revents & libc::POLLIN == 0 {
                    if let Some(bytes) = input_router.flush() {
                        close_confirmation = None;
                        self.write_focused(focus, &bytes)?;
                    }
                    continue;
                }
                let read = io::stdin().read(&mut input)?;
                if read == 0 {
                    break;
                }
                if let Some(mut value) = rename_input.take() {
                    close_confirmation = None;
                    let mut cancelled = false;
                    let mut committed = false;
                    for byte in &input[..read] {
                        match *byte {
                            b'\r' | b'\n' => committed = true,
                            0x1b => cancelled = true,
                            0x7f | 0x08 => {
                                value.pop();
                                while value
                                    .last()
                                    .is_some_and(|byte| byte & 0b1100_0000 == 0b1000_0000)
                                {
                                    value.pop();
                                }
                            }
                            byte if !byte.is_ascii_control() => value.push(byte),
                            _ => {}
                        }
                    }
                    if committed {
                        let name = String::from_utf8_lossy(&value).trim().to_owned();
                        if !name.is_empty()
                            && let Some(shell) = self.shells.get_mut(self.selected_shell)
                        {
                            shell.name = name.clone();
                            self.notice = Some(format!("renamed shell to {name}"));
                        }
                    } else if cancelled {
                        self.notice = Some("shell rename cancelled".into());
                    } else {
                        self.notice = Some(format!(
                            "rename shell: {}_ · Enter apply · Esc cancel",
                            String::from_utf8_lossy(&value)
                        ));
                        rename_input = Some(value);
                    }
                    last_signature.clear();
                    continue;
                }
                for routed in input_router.route(&input[..read], focus) {
                    if let WorkspaceInput::Mouse(event) = routed {
                        close_confirmation = None;
                        self.handle_mouse(&layout, event)?;
                        last_signature.clear();
                        continue;
                    }
                    let WorkspaceInput::Command(command) = routed else {
                        if let WorkspaceInput::Forward(bytes) = routed {
                            close_confirmation = None;
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
                    if command != WorkspaceCommand::CloseShell {
                        close_confirmation = None;
                    }
                    match command {
                        WorkspaceCommand::Dashboard => break 'workspace,
                        WorkspaceCommand::Alert => {
                            exit = WorkspaceExit::Alert;
                            break 'workspace;
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
                        WorkspaceCommand::RenameShell => {
                            if let Some(shell) = self.shells.get(self.selected_shell) {
                                self.notice = Some(format!(
                                    "rename shell '{}' to: _ · Enter apply · Esc cancel",
                                    shell.name
                                ));
                                rename_input = Some(Vec::new());
                            } else {
                                self.notice = Some("focus a shell before renaming".into());
                            }
                        }
                        WorkspaceCommand::ToggleMaximize => {
                            let target = match focus {
                                WorkspaceFocus::Sessions => {
                                    if !self.shells.is_empty() {
                                        focus = WorkspaceFocus::Shell;
                                        PaneTarget::Shell(self.selected_shell)
                                    } else if self.agent.is_some() {
                                        focus = WorkspaceFocus::Agent;
                                        PaneTarget::Agent
                                    } else {
                                        self.notice = Some(
                                            "no agent or shell is available to maximize".into(),
                                        );
                                        continue;
                                    }
                                }
                                WorkspaceFocus::Agent => PaneTarget::Agent,
                                WorkspaceFocus::Shell => PaneTarget::Shell(self.selected_shell),
                            };
                            self.maximized = (self.maximized != Some(target)).then_some(target);
                            self.notice = Some(if self.maximized.is_some() {
                                format!(
                                    "pane maximized · {} restores",
                                    render_bindings.label("maximize")
                                )
                            } else {
                                "pane restored".into()
                            });
                            clear_next_frame = true;
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
                                close_confirmation = None;
                                self.notice = Some("no shell to close".into());
                                continue;
                            }
                            let alive = self
                                .shells
                                .get(self.selected_shell)
                                .is_some_and(|shell| shell.terminal.is_alive());
                            match shell_close_action(
                                focus,
                                alive,
                                close_confirmation == Some(self.selected_shell),
                            ) {
                                ShellCloseAction::Ignore => {
                                    close_confirmation = None;
                                    self.notice = Some("focus a shell before closing it".into());
                                }
                                ShellCloseAction::Confirm => {
                                    close_confirmation = Some(self.selected_shell);
                                    self.notice = Some(format!(
                                        "shell is running · press {} again to close",
                                        render_bindings.label("close_shell")
                                    ));
                                }
                                ShellCloseAction::Close => {
                                    close_confirmation = None;
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
        let restore = stdout
            .write_all(DISABLE_MOUSE_REPORTING)
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
    let shell = env::var_os("SHELL").unwrap_or_else(|| OsString::from("/bin/sh"));
    ManagedTerminal::spawn(&CommandSpec::new(shell, &session.cwd).arg("-l"), size)
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
        focus,
        &WorkspaceBindings::from_config(&AgentConsoleConfig::default()),
        clear,
    )
}

fn render_workspace_with_bindings(
    stdout: &mut impl Write,
    terminals: &SessionTerminals,
    chrome: &WorkspaceChrome,
    layout: &WorkspaceLayout,
    focus: WorkspaceFocus,
    bindings: &WorkspaceBindings,
    clear: bool,
) -> io::Result<()> {
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
        let agent_label = if focus == WorkspaceFocus::Sessions {
            format!("SESSION PREVIEW · {selected_label}")
        } else {
            pane_label_with_scrollback(
                &format!("AGENT · {selected_label}"),
                terminals.agent.as_ref(),
            )
        };
        render_pane_title(
            stdout,
            0,
            layout.agent.left,
            layout.agent.width,
            &agent_label,
            focus == WorkspaceFocus::Agent,
        )?;
        if focus == WorkspaceFocus::Sessions {
            render_session_preview(stdout, &chrome.preview, layout.agent)?;
        } else if let Some(agent) = &terminals.agent {
            render_terminal(stdout, agent, layout.agent)?;
            if let Some(selection) = terminals
                .selection
                .filter(|selection| selection.pane == PaneTarget::Agent)
            {
                render_selection(stdout, agent, selection, layout.agent)?;
            }
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
        format!(
            "  ALERT · {notification}  ·  {} jump  {} dashboard",
            bindings.label("alert"),
            bindings.label("dashboard")
        )
    } else {
        let shortcuts = match focus {
            WorkspaceFocus::Sessions => format!(
                "↑↓/j/k session  Enter agent  n session  s shell  x archive  {} focus  {} dashboard",
                bindings.label("focus"),
                bindings.label("dashboard")
            ),
            WorkspaceFocus::Agent => format!(
                "keys pass through  ·  {} new shell  {} focus  {} dashboard  ·  Shift-PageUp/Down scroll",
                bindings.label("new_shell"),
                bindings.label("focus"),
                bindings.label("dashboard")
            ),
            WorkspaceFocus::Shell => format!(
                "{} new  {} next  {} close  {} focus  {} dashboard  ·  Shift-PageUp/Down scroll",
                bindings.label("new_shell"),
                bindings.label("next_shell"),
                bindings.label("close_shell"),
                bindings.label("focus"),
                bindings.label("dashboard")
            ),
        };
        terminals.notice.as_deref().map_or_else(
            || format!("  {shortcuts}"),
            |notice| {
                let essentials = match focus {
                    WorkspaceFocus::Sessions => "n session  s shell  x archive".to_owned(),
                    WorkspaceFocus::Agent => format!(
                        "{} new shell  {} focus",
                        bindings.label("new_shell"),
                        bindings.label("focus")
                    ),
                    WorkspaceFocus::Shell => format!(
                        "{} new  {} next  {} close",
                        bindings.label("new_shell"),
                        bindings.label("next_shell"),
                        bindings.label("close_shell")
                    ),
                };
                format!("  {notice}  ·  {essentials}")
            },
        )
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
    let visible = usize::from(layout.status_row.saturating_sub(1));
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
                row as u16 + 1,
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
            format!("\x1b[7m{label}\x1b[0m")
        } else {
            label
        };
        write_at(stdout, row as u16 + 1, 0, line.as_bytes())?;
    }
    render_vertical_line(stdout, layout.sidebar_width, 0, layout.status_row)
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
            use_daemon: env::var("AGENT_CONSOLE_PTY_MODE").as_deref() != Ok("local"),
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
        profile: Option<&str>,
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
            let spec = agent_command(&self.config, session, profile, current_exe, new_session);
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
        F: FnMut() -> WorkspaceChrome,
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
    profile: Option<&str>,
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
            spec = spec.arg("-C").arg(session.cwd.as_os_str());
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
    let command = config.provider_command_for_profile(session.agent, profile, args);
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
            if compact.contains(needle) || start.elapsed() >= Duration::from_secs(1) {
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
        let codex = agent_command(
            &config,
            &session(AgentKind::Codex, cwd),
            None,
            executable,
            false,
        );
        assert_eq!(codex.program, "codex");
        assert_eq!(codex.args[0], "resume");
        assert!(codex.args.iter().any(|arg| arg == "id"));

        let claude = agent_command(
            &config,
            &session(AgentKind::Claude, cwd),
            None,
            executable,
            false,
        );
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
            None,
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
    fn agent_command_uses_selected_named_profile() {
        let config = AgentConsoleConfig::parse(
            "[profiles.work]\ncodex = [\"profile-wrapper\", \"codex\", \"--profile\", \"work\"]\n",
            Path::new("config.toml"),
        )
        .unwrap();
        let command = agent_command(
            &config,
            &session(AgentKind::Codex, Path::new("/tmp/repo")),
            Some("work"),
            Path::new("/tmp/agent-console"),
            false,
        );

        assert_eq!(command.program, "profile-wrapper");
        assert_eq!(command.args[0], "codex");
        assert_eq!(command.args[1], "--profile");
        assert_eq!(command.args[2], "work");
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
        terminal.wait_for_first_output(Duration::from_secs(2));
        assert!(terminal.plain_capture().contains("ready"));
        terminal.write(b"hello\n").unwrap();
        let start = Instant::now();
        while terminal.is_alive() && start.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(terminal.plain_capture().contains("got:hello"));
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
                b"\x0f",
                WorkspaceCommand::ToggleFocus,
                WorkspaceFocus::Agent,
            ),
            (b"\x11", WorkspaceCommand::Dashboard, WorkspaceFocus::Shell),
            (b"\x1d", WorkspaceCommand::Alert, WorkspaceFocus::Agent),
            (b"\x1c", WorkspaceCommand::NewShell, WorkspaceFocus::Agent),
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
                b"r",
                WorkspaceCommand::RenameShell,
                WorkspaceFocus::Sessions,
            ),
            (
                b"m",
                WorkspaceCommand::ToggleMaximize,
                WorkspaceFocus::Sessions,
            ),
            (b"+", WorkspaceCommand::GrowShell, WorkspaceFocus::Sessions),
            (
                b"-",
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
        assert!(matches!(
            workspace_command(b"\x1b[1;2F"),
            Some(WorkspaceCommand::LiveTail)
        ));
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
    fn unreserved_control_byte_is_forwarded_immediately() {
        let mut router = WorkspaceInputRouter::default();
        let routed = router.route(b"\x02", WorkspaceFocus::Agent);
        assert!(matches!(
            routed.as_slice(),
            [WorkspaceInput::Forward(bytes)] if bytes == b"\x02"
        ));
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
                router.route(b"\x1c", focus).as_slice(),
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
    fn close_shell_requires_console_or_shell_focus_and_confirmation_for_a_live_shell() {
        assert_eq!(
            shell_close_action(WorkspaceFocus::Agent, true, false),
            ShellCloseAction::Ignore
        );
        assert_eq!(
            shell_close_action(WorkspaceFocus::Sessions, true, false),
            ShellCloseAction::Confirm
        );
        assert_eq!(
            shell_close_action(WorkspaceFocus::Shell, true, false),
            ShellCloseAction::Confirm
        );
        assert_eq!(
            shell_close_action(WorkspaceFocus::Shell, true, true),
            ShellCloseAction::Close
        );
        assert_eq!(
            shell_close_action(WorkspaceFocus::Shell, false, false),
            ShellCloseAction::Close
        );
    }

    #[test]
    fn focused_session_list_accepts_navigation_and_creation_actions() {
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
    fn subsequent_workspace_frame_does_not_clear_the_screen() {
        let terminals = SessionTerminals::default();
        let chrome = WorkspaceChrome {
            sessions: vec!["repo  codex".into()],
            selected: 0,
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
        assert!(output.contains("Ctrl-\\ new shell"));
        assert!(output.contains("Ctrl-O focus"));
        assert!(output.contains("FOCUS AGENT"));
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
        assert!(output.contains("n session"));
        assert!(output.contains("s shell"));
        assert!(output.contains("x archive"));
        assert!(output.contains("\x1b[30;46;1m▸ ○ Cdx inspect sess"));
        assert!(!output.contains("\x1b[30;46;1m SESSIONS"));
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
}
