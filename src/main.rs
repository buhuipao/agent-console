mod app;
mod clipboard;
mod completion;
mod config;
mod diagnostics;
mod discovery;
mod doctor;
mod events;
mod model;
mod providers;
mod pty;
mod store;
mod summary;
mod web;

use std::{
    env, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use app::{App, DialogField, TextDialogKind, WebStatus, WorkspaceDrive};
use completion::workspace_directory_completions;
use config::AgentConsoleConfig;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};
use model::{AgentKind, SessionStatus, unix_timestamp};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear as WidgetClear, Padding, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthChar;

type DashboardTerminal = Terminal<CrosstermBackend<io::Stdout>>;

fn main() -> io::Result<()> {
    if let Some(state_dir) = store::state_dir() {
        let _ = diagnostics::init(&state_dir);
    }
    diagnostics::install_panic_hook();
    let result = dispatch();
    if let Err(error) = &result {
        diagnostics::record(&format!("fatal error: {error}"));
    }
    result
}

fn dispatch() -> io::Result<()> {
    let mut args = env::args().skip(1).peekable();
    // Peeked, not consumed: the dashboard now takes options of its own, so a leading `--host`
    // has to fall through to it rather than be swallowed as a subcommand name.
    match args.peek().map(String::as_str) {
        Some("--version" | "-V") => {
            println!("agent-console {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("--help" | "-h") => {
            print!("{}", cli_help());
            Ok(())
        }
        Some("hook") => {
            args.next();
            run_hook(args.next())
        }
        Some("doctor") => {
            args.next();
            run_doctor()
        }
        Some("pty-daemon") => {
            args.next();
            args.next()
                .map(PathBuf::from)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "pty-daemon requires a socket path",
                    )
                })
                .and_then(|socket| pty::run_pty_daemon(&socket))
        }
        Some("pty-daemon-stop") => {
            args.next();
            args.next()
                .map(PathBuf::from)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "pty-daemon-stop requires a socket path",
                    )
                })
                .and_then(|socket| pty::stop_pty_daemon(&socket))
        }
        Some("web") => {
            args.next();
            run_web_command(args)
        }
        Some(other) if !other.starts_with('-') => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown command: {other}"),
        )),
        _ => run_dashboard(args),
    }
}

fn cli_help() -> &'static str {
    concat!(
        "Agent Console ",
        env!("CARGO_PKG_VERSION"),
        "\n\n",
        "Usage:\n",
        "  agent-console [--host H] [--port P] [--auth U:P] [--no-web]\n",
        "                                        Open the dashboard, serving the web UI too\n",
        "  agent-console doctor                 Check providers and terminal prerequisites\n",
        "  agent-console web [--host H] [--port P] [--auth U:P]\n",
        "                                        Serve the web/PWA dashboard alone, with no TUI\n",
        "  agent-console --help                 Show this help\n",
        "  agent-console --version              Show the version\n",
        "\n",
        "Web options (dashboard and `web` alike; default 127.0.0.1:7878):\n",
        "  --host <H>   Bind address; a hostname or an IP. Anything but loopback warns\n",
        "  --port <P>   Bind port. Already in use leaves the dashboard running without it\n",
        "  --auth <U:P> HTTP Basic user and password. Without it, a random URL token\n",
        "  --no-web     Open the dashboard only (dashboard invocation only)\n",
        "\n",
        "Each of those also reads an environment variable, then [web] host/port/auth/\n",
        "enabled in ~/.config/agent-console/config.toml:\n",
        "  AGENT_CONSOLE_WEB_HOST   AGENT_CONSOLE_WEB_PORT\n",
        "  AGENT_CONSOLE_WEB_AUTH   AGENT_CONSOLE_WEB_ENABLED\n",
        "Command line wins over environment wins over config file. Prefer the environment\n",
        "or the config file for a password: argv is visible in `ps` to every local user.\n",
    )
}

fn web_help() -> &'static str {
    concat!(
        "Agent Console web ",
        env!("CARGO_PKG_VERSION"),
        "\n\n",
        "Serve a responsive PWA dashboard that drives the same sessions the TUI does:\n",
        "list, create, attach, type into, resize, archive, and terminate sessions from a\n",
        "phone or desktop browser.\n\n",
        "Plain `agent-console` already serves this beside the dashboard. This subcommand is\n",
        "for a machine with no TUI attached to it.\n\n",
        "Usage:\n",
        "  agent-console web [--host <H>] [--port <P>] [--auth <user>:<password>]\n\n",
        "Options:\n",
        "  --host <H>   Bind address, hostname or IP (default 127.0.0.1)\n",
        "  --port <P>   Bind port (default 7878)\n",
        "  --auth <U:P> HTTP Basic user and password. Without it, a random URL token\n",
        "  --help, -h   Show this help\n\n",
        "Also read from AGENT_CONSOLE_WEB_HOST, AGENT_CONSOLE_WEB_PORT and\n",
        "AGENT_CONSOLE_WEB_AUTH, and from [web] host/port/auth in\n",
        "~/.config/agent-console/config.toml. Command line wins, then environment, then\n",
        "the config file.\n",
    )
}

/// What the command line asked for, before the environment and the config file get a say.
#[derive(Debug)]
enum ParsedOptions {
    Help,
    Options(web::WebOverrides),
}

/// Parses the web options both invocations share.
///
/// `--no-web` is only offered where it means something: on `agent-console web` the server is
/// the entire command, so accepting a flag that turns it off would be nonsense.
fn parse_web_options(
    mut args: impl Iterator<Item = String>,
    context: &str,
    allow_disable: bool,
) -> io::Result<ParsedOptions> {
    let mut overrides = web::WebOverrides::default();
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{arg} requires a value"),
                )
            })
        };
        match arg.as_str() {
            "--help" | "-h" => return Ok(ParsedOptions::Help),
            "--host" => overrides.host = Some(value()?),
            "--port" => {
                let raw = value()?;
                overrides.port = Some(raw.parse().map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("invalid --port value: {raw}"),
                    )
                })?);
            }
            // The value is a password. It is not echoed here, and a bad one is reported by
            // `WebSettings::resolve` in terms of the rule it broke.
            "--auth" => overrides.auth = Some(value()?),
            "--no-web" if allow_disable => overrides.enabled = Some(false),
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown {context}: {other}"),
                ));
            }
        }
    }
    Ok(ParsedOptions::Options(overrides))
}

/// Command line, then environment, then `[web]` in the config file.
fn web_settings(overrides: &web::WebOverrides) -> io::Result<web::WebSettings> {
    let config = AgentConsoleConfig::load()?;
    web::WebSettings::resolve(overrides, &web::WebEnv::from_environment(), &config.web)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

fn run_web_command(args: impl Iterator<Item = String>) -> io::Result<()> {
    match parse_web_options(args, "web option", false)? {
        ParsedOptions::Help => {
            print!("{}", web_help());
            Ok(())
        }
        // `enabled` is ignored here on purpose: asking for the server by name outranks a
        // config file that turned the dashboard's embedded one off.
        ParsedOptions::Options(overrides) => web::run_web(&web_settings(&overrides)?),
    }
}

fn run_hook(provider: Option<String>) -> io::Result<()> {
    use std::io::Read;

    let provider = match provider.as_deref() {
        Some("codex") => AgentKind::Codex,
        Some("claude") => AgentKind::Claude,
        Some("pi") => AgentKind::Pi,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "hook provider must be codex, claude, or pi",
            ));
        }
    };
    let events_dir = store::state_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cannot resolve state directory"))?
        .join("events");
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let value = serde_json::from_str(&input).map_err(io::Error::other)?;
    events::ingest_hook(provider, &value, &events_dir)?;
    Ok(())
}

/// Prints the shared `doctor::report()`. Every probe lives there so the web endpoint runs
/// the same checks; this function only decides how a line reads on a terminal.
fn run_doctor() -> io::Result<()> {
    let report = doctor::report()?;
    println!(
        "ok   providers enabled: {}",
        report.providers_enabled.join(", ")
    );
    for provider in &report.providers {
        let name = provider.name;
        if !provider.available {
            println!("info {name}: {}", provider.detail);
            continue;
        }
        println!("ok   {name}: {}", provider.detail);
        match provider.version_support {
            Some("supported") => println!("ok   {name} version: supported"),
            Some("too_old") => println!("fail {name} version: below the supported minimum"),
            _ => println!("info {name} version: could not parse; compatibility unverified"),
        }
        for capability in &provider.capabilities {
            print_doctor_check(capability);
        }
    }
    for path in &report.discovery {
        println!(
            "{} {}: {}",
            if path.exists { "ok  " } else { "info" },
            path.name,
            path.path
        );
    }
    for check in &report.checks {
        print_doctor_check(check);
    }
    if let Some(path) = &report.diagnostics_path {
        println!("ok   rotating diagnostics: {path}");
    }
    if !report.providers.iter().any(|provider| provider.available) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no supported agent CLI is available",
        ));
    }
    if report.failures > 0 {
        return Err(io::Error::other(format!(
            "doctor found {} required capability failure(s)",
            report.failures
        )));
    }
    Ok(())
}

fn print_doctor_check(check: &doctor::CheckReport) {
    let prefix = if check.ok { "ok  " } else { "fail" };
    println!("{prefix} {}: {}", check.name, check.detail);
}

fn run_dashboard(args: impl Iterator<Item = String>) -> io::Result<()> {
    let overrides = match parse_web_options(args, "option", true)? {
        ParsedOptions::Help => {
            print!("{}", cli_help());
            return Ok(());
        }
        ParsedOptions::Options(overrides) => overrides,
    };
    // Resolved before anything slow runs: a malformed `--auth` should fail in milliseconds,
    // not after session discovery.
    let settings = web_settings(&overrides)?;
    let startup_cwd = env::current_dir()?;

    // One `App`, shared. The dashboard locks it per loop iteration and the server locks it
    // per request, so both surfaces see the same sessions, the same discovery worker and the
    // same summary worker rather than two divergent copies of all three.
    let app = Arc::new(Mutex::new(App::load(startup_cwd)?));
    let status = start_embedded_web(&app, &settings);
    app.lock().unwrap().set_web_status(status);

    enable_raw_mode()?;
    let cleanup = TerminalCleanup;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = run(&mut terminal, &app);
    let restore = restore_terminal(&mut terminal);
    let shutdown = app.lock().unwrap().shutdown();
    drop(cleanup);
    result.and(restore).and(shutdown)
}

/// Starts the embedded server, turning any failure into something the dashboard can display.
///
/// Nothing here is fatal. A port already taken by another agent-console -- or by anything
/// else -- must not cost the user their dashboard, so the failure becomes a line of chrome
/// instead of an exit code. The port is *not* silently moved: a server on a port nobody was
/// told about is worse than no server at all.
fn start_embedded_web(app: &Arc<Mutex<App>>, settings: &web::WebSettings) -> WebStatus {
    if !settings.enabled {
        return WebStatus::Disabled;
    }
    match web::start_embedded(Arc::clone(app), settings) {
        Ok(running) => WebStatus::Serving {
            url: running.url,
            auth: running.auth,
            exposed: running.exposed,
        },
        Err(error) => {
            let reason = if error.kind() == io::ErrorKind::AddrInUse {
                format!("{}:{} is already in use", settings.host, settings.port)
            } else {
                error.to_string()
            };
            diagnostics::record(&format!("embedded web server did not start: {reason}"));
            eprintln!("warning: agent-console web UI did not start: {reason}");
            WebStatus::Unavailable(reason)
        }
    }
}

struct TerminalCleanup;

impl Drop for TerminalCleanup {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
    }
}

fn restore_terminal(terminal: &mut DashboardTerminal) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen,
        Clear(ClearType::All)
    )?;
    terminal.show_cursor()
}

/// How long the dashboard waits for input before ticking again.
const TICK_INTERVAL: Duration = Duration::from_millis(100);

/// How long a workspace frame waits for input before repainting.
const WORKSPACE_POLL: Duration = Duration::from_millis(10);

/// The dashboard loop, driving an `App` it shares with the embedded web server.
///
/// The lock is taken three times per iteration and released in between. It is deliberately
/// *not* held across `event::poll`, which is where this loop spends nearly all of its time:
/// holding it there would make every web request wait out a poll interval, and holding it
/// across `event::read` -- which blocks until a key arrives -- would stall them indefinitely.
fn run(terminal: &mut DashboardTerminal, shared: &Arc<Mutex<App>>) -> io::Result<()> {
    let current_exe = env::current_exe()?;
    loop {
        {
            let app = shared.lock().unwrap();
            terminal.draw(|frame| draw(frame, &app))?;
        }
        let event = if event::poll(TICK_INTERVAL)? {
            Some(event::read()?)
        } else {
            None
        };
        let outcome = {
            let mut app = shared.lock().unwrap();
            let outcome = match event {
                Some(event) => handle_dashboard_event(&mut app, event, &current_exe)?,
                None => DashboardOutcome::Continue,
            };
            // A workspace takes this iteration's place rather than adding to it: it runs the
            // same tick on every frame of its own, and the dashboard's resumes when it closes.
            if matches!(outcome, DashboardOutcome::Continue) {
                app.tick();
            }
            outcome
        };
        match outcome {
            DashboardOutcome::Continue => {}
            DashboardOutcome::Quit => return Ok(()),
            DashboardOutcome::Workspace { drive, failure } => {
                run_workspace(shared, drive, failure, &current_exe, terminal);
            }
        }
    }
}

/// What one dashboard event asked the loop above to do next.
enum DashboardOutcome {
    Continue,
    Quit,
    Workspace {
        drive: Box<WorkspaceDrive>,
        /// The banner prefix a failure keeps, so the message reads the same whether the
        /// workspace failed while opening or while it was up.
        failure: &'static str,
    },
}

/// Runs an open workspace to its end, one frame at a time.
///
/// This loop is out here, rather than inside the pty layer where it used to be, because of
/// what a frame must *not* hold. An attach that ran to completion under `&mut App` kept the
/// shared lock for as long as a session was open, so the embedded web server could not answer
/// at all during the only state anyone actually uses the console in. Splitting the frame is
/// what fixes it, and the split only means something if the pieces are locked separately --
/// which can only be arranged by whoever owns the lock, out here.
fn run_workspace(
    shared: &Arc<Mutex<App>>,
    mut drive: Box<WorkspaceDrive>,
    failure: &str,
    current_exe: &Path,
    terminal: &mut DashboardTerminal,
) {
    let result = (|| -> io::Result<()> {
        loop {
            // Each stage names what it locks. Only the middle one touches the `App`, and only
            // for a runtime tick and a chrome snapshot; the two around it -- which is where a
            // frame actually spends its time -- hold this session's terminal lock instead, and
            // the wait at the end holds nothing at all.
            let outcome = drive.apply_input()?;
            let chrome = shared
                .lock()
                .unwrap()
                .workspace_frame_chrome(&mut drive, outcome.search);
            let exit = match outcome.exit {
                Some(exit) => Some(exit),
                None => drive.render(chrome)?,
            };
            if let Some(exit) = exit {
                let finished = shared.lock().unwrap().advance_workspace_attach(
                    &mut drive,
                    exit,
                    current_exe,
                )?;
                if finished {
                    return Ok(());
                }
                continue;
            }
            drive.wait(WORKSPACE_POLL)?;
        }
    })();
    if let Err(error) = result {
        let mut app = shared.lock().unwrap();
        let _ = app.abandon_workspace(*drive);
        app.banner = Some(format!("{failure}: {error}"));
    }
    let _ = execute!(
        terminal.backend_mut(),
        EnableMouseCapture,
        Clear(ClearType::All)
    );
    terminal.swap_buffers();
}

/// Applies one input event, reporting what the dashboard loop should do next.
fn handle_dashboard_event(
    app: &mut App,
    event: Event,
    current_exe: &Path,
) -> io::Result<DashboardOutcome> {
    match event {
        Event::Mouse(mouse) => handle_dashboard_mouse(app, mouse),
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            if app.dialog.is_some() {
                return handle_dialog_key(app, key.code, current_exe);
            } else if app.text_dialog.is_some() {
                handle_text_dialog_key(app, key.code);
            } else {
                let key_name = dashboard_key_name(key);
                if app.help_open {
                    if key.code == KeyCode::Esc || app.dashboard_action(&key_name) == Some("help") {
                        app.help_open = false;
                    }
                    return Ok(DashboardOutcome::Continue);
                }
                if let Some(action) = app.dashboard_action(&key_name) {
                    return Ok(handle_dashboard_action(app, action, current_exe));
                }
            }
        }
        _ => {}
    }
    Ok(DashboardOutcome::Continue)
}

fn handle_dashboard_action(
    app: &mut App,
    action: &str,
    current_exe: &std::path::Path,
) -> DashboardOutcome {
    match action {
        "quit" => return DashboardOutcome::Quit,
        "next" => app.select_next(),
        "previous" => app.select_previous(),
        "enter" => return attach_agent(app, current_exe),
        "takeover" => return force_attach_agent(app, current_exe),
        "shell" => return attach_shell(app, current_exe),
        "copy" => {
            if let Err(error) = app.copy_shell_capture() {
                app.banner = Some(error);
            }
        }
        "stage" => {
            if let Err(error) = app.stage_shell_capture() {
                app.banner = Some(error);
            }
        }
        "new" => app.open_new_dialog(),
        "alert" if !app.jump_to_next_notification() => {
            app.banner = Some("no unread alerts".into());
        }
        "retry_summary" => {
            if let Err(error) = app.retry_selected_summary() {
                app.banner = Some(error);
            }
        }
        "search" => app.open_search_dialog(),
        "alias" => app.open_alias_dialog(),
        "archive" => {
            if let Err(error) = app.toggle_selected_archive() {
                app.banner = Some(error);
            }
        }
        "help" => app.help_open = true,
        _ => {}
    }
    DashboardOutcome::Continue
}

fn dashboard_key_name(key: KeyEvent) -> String {
    let prefix = if key.modifiers.contains(KeyModifiers::CONTROL) {
        "ctrl-"
    } else if key.modifiers.contains(KeyModifiers::ALT) {
        "alt-"
    } else {
        ""
    };
    let name = match key.code {
        KeyCode::Char(character) => character.to_ascii_lowercase().to_string(),
        KeyCode::Enter => "enter".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => "backtab".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::PageUp => "pageup".into(),
        KeyCode::PageDown => "pagedown".into(),
        KeyCode::F(number) => format!("f{number}"),
        _ => return String::new(),
    };
    format!("{prefix}{name}")
}

fn handle_dashboard_mouse(app: &mut App, mouse: MouseEvent) {
    if app.dialog.is_some() || app.text_dialog.is_some() || app.help_open {
        return;
    }
    let (width, height) = crossterm::terminal::size().unwrap_or((120, 40));
    let over_sessions = mouse.column < dashboard_sidebar_width(width)
        && mouse.row >= 3
        && mouse.row < height.saturating_sub(3);
    if !over_sessions {
        return;
    }
    match mouse.kind {
        MouseEventKind::ScrollUp => app.select_previous(),
        MouseEventKind::ScrollDown => app.select_next(),
        _ => {}
    }
}

fn handle_text_dialog_key(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Esc => app.cancel_text_dialog(),
        KeyCode::Enter => {
            if let Err(error) = app.commit_text_dialog() {
                app.banner = Some(error);
            }
        }
        KeyCode::Backspace => {
            app.pop_text_dialog_character();
        }
        KeyCode::Char(character) => {
            app.push_text_dialog_character(character);
        }
        _ => {}
    }
}

fn handle_dialog_key(
    app: &mut App,
    key: KeyCode,
    current_exe: &std::path::Path,
) -> io::Result<DashboardOutcome> {
    match key {
        KeyCode::Esc => app.cancel_dialog(),
        KeyCode::BackTab => {
            if let Some(dialog) = &mut app.dialog {
                dialog.field = toggle_dialog_field(dialog.field);
                dialog.error = None;
            }
        }
        KeyCode::Tab
            if app.dialog.as_ref().is_some_and(|dialog| {
                dialog.field == DialogField::Cwd
                    && !dialog.cwd_replace_on_input
                    && !dialog.cwd_completion_accepted
                    && dialog.cwd_cursor == dialog.cwd.chars().count()
                    && !dialog_workspace_completions(dialog, dirs::home_dir().as_deref()).is_empty()
            }) =>
        {
            if let Some(dialog) = &mut app.dialog {
                accept_workspace_completion(dialog);
            }
        }
        // `< codex >` is drawn with an arrow on each side, so each one has to walk its own
        // way round the ring. With two providers the direction made no difference; with three
        // it does, and Left going forwards would contradict the control it is drawn under.
        KeyCode::Left | KeyCode::Right | KeyCode::Char('h') | KeyCode::Char('l')
            if app
                .dialog
                .as_ref()
                .is_some_and(|dialog| dialog.field == DialogField::Provider) =>
        {
            let forwards = matches!(key, KeyCode::Right | KeyCode::Char('l'));
            if let Some(dialog) = &mut app.dialog {
                dialog.provider = if forwards {
                    match dialog.provider {
                        AgentKind::Codex => AgentKind::Claude,
                        AgentKind::Claude => AgentKind::Pi,
                        AgentKind::Pi => AgentKind::Codex,
                    }
                } else {
                    match dialog.provider {
                        AgentKind::Codex => AgentKind::Pi,
                        AgentKind::Claude => AgentKind::Codex,
                        AgentKind::Pi => AgentKind::Claude,
                    }
                };
            }
        }
        KeyCode::Up | KeyCode::Down
            if app
                .dialog
                .as_ref()
                .is_some_and(|dialog| dialog.field == DialogField::Cwd) =>
        {
            if let Some(dialog) = &mut app.dialog {
                let count = dialog_workspace_completions(dialog, dirs::home_dir().as_deref()).len();
                if count > 0 {
                    dialog.cwd_completion_index = if key == KeyCode::Up {
                        dialog
                            .cwd_completion_index
                            .checked_sub(1)
                            .unwrap_or(count - 1)
                    } else {
                        (dialog.cwd_completion_index + 1) % count
                    };
                    dialog.cwd_completion_accepted = false;
                }
            }
        }
        KeyCode::Left
        | KeyCode::Right
        | KeyCode::Home
        | KeyCode::End
        | KeyCode::Backspace
        | KeyCode::Delete => {
            if let Some(dialog) = &mut app.dialog
                && dialog.field == DialogField::Cwd
            {
                edit_dialog_cwd(dialog, key);
            }
        }
        KeyCode::Char(character) => {
            if let Some(dialog) = &mut app.dialog
                && dialog.field == DialogField::Cwd
            {
                edit_dialog_cwd(dialog, KeyCode::Char(character));
            }
        }
        KeyCode::Enter => match app.create_from_dialog() {
            Ok(()) => return Ok(attach_agent(app, current_exe)),
            Err(error) => {
                if let Some(dialog) = &mut app.dialog {
                    dialog.error = Some(error);
                }
            }
        },
        _ => {}
    }
    Ok(DashboardOutcome::Continue)
}

fn edit_dialog_cwd(dialog: &mut app::NewSessionDialog, key: KeyCode) {
    let character_count = dialog.cwd.chars().count();
    dialog.cwd_cursor = dialog.cwd_cursor.min(character_count);
    match key {
        KeyCode::Left => {
            dialog.cwd_cursor = if dialog.cwd_replace_on_input {
                0
            } else {
                dialog.cwd_cursor.saturating_sub(1)
            };
            dialog.cwd_replace_on_input = false;
            reset_dialog_cwd_completion(dialog);
        }
        KeyCode::Right => {
            dialog.cwd_cursor = if dialog.cwd_replace_on_input {
                character_count
            } else {
                dialog.cwd_cursor.saturating_add(1).min(character_count)
            };
            dialog.cwd_replace_on_input = false;
            reset_dialog_cwd_completion(dialog);
        }
        KeyCode::Home => {
            dialog.cwd_cursor = 0;
            dialog.cwd_replace_on_input = false;
            reset_dialog_cwd_completion(dialog);
        }
        KeyCode::End => {
            dialog.cwd_cursor = character_count;
            dialog.cwd_replace_on_input = false;
            reset_dialog_cwd_completion(dialog);
        }
        KeyCode::Backspace => {
            if dialog.cwd_replace_on_input {
                dialog.cwd.clear();
                dialog.cwd_replace_on_input = false;
                dialog.cwd_cursor = 0;
            } else if dialog.cwd_cursor > 0 {
                let start = char_byte_index(&dialog.cwd, dialog.cwd_cursor - 1);
                let end = char_byte_index(&dialog.cwd, dialog.cwd_cursor);
                dialog.cwd.replace_range(start..end, "");
                dialog.cwd_cursor -= 1;
            }
            reset_dialog_cwd_completion(dialog);
        }
        KeyCode::Delete => {
            if dialog.cwd_replace_on_input {
                dialog.cwd.clear();
                dialog.cwd_replace_on_input = false;
                dialog.cwd_cursor = 0;
            } else if dialog.cwd_cursor < character_count {
                let start = char_byte_index(&dialog.cwd, dialog.cwd_cursor);
                let end = char_byte_index(&dialog.cwd, dialog.cwd_cursor + 1);
                dialog.cwd.replace_range(start..end, "");
            }
            reset_dialog_cwd_completion(dialog);
        }
        KeyCode::Char(character) => {
            if dialog.cwd_replace_on_input {
                dialog.cwd.clear();
                dialog.cwd_replace_on_input = false;
                dialog.cwd_cursor = 0;
            }
            let byte_index = char_byte_index(&dialog.cwd, dialog.cwd_cursor);
            dialog.cwd.insert(byte_index, character);
            dialog.cwd_cursor += 1;
            reset_dialog_cwd_completion(dialog);
        }
        _ => {}
    }
}

fn reset_dialog_cwd_completion(dialog: &mut app::NewSessionDialog) {
    dialog.cwd_completion_index = 0;
    dialog.cwd_completion_accepted = false;
    dialog.error = None;
}

fn char_byte_index(value: &str, character_index: usize) -> usize {
    value
        .char_indices()
        .nth(character_index)
        .map_or(value.len(), |(index, _)| index)
}

fn dialog_workspace_completions(
    dialog: &app::NewSessionDialog,
    home: Option<&Path>,
) -> Vec<String> {
    if dialog.cwd_cursor.min(dialog.cwd.chars().count()) == dialog.cwd.chars().count() {
        workspace_directory_completions(&dialog.cwd, home)
    } else {
        Vec::new()
    }
}

fn accept_workspace_completion(dialog: &mut app::NewSessionDialog) {
    let completions = dialog_workspace_completions(dialog, dirs::home_dir().as_deref());
    if let Some(completion) = completions.get(
        dialog
            .cwd_completion_index
            .min(completions.len().saturating_sub(1)),
    ) {
        dialog.cwd.clone_from(completion);
        dialog.cwd_cursor = dialog.cwd.chars().count();
        dialog.cwd_completion_index = 0;
        dialog.cwd_completion_accepted = true;
        dialog.cwd_replace_on_input = false;
        dialog.error = None;
    }
}

fn toggle_dialog_field(field: DialogField) -> DialogField {
    match field {
        DialogField::Provider => DialogField::Cwd,
        DialogField::Cwd => DialogField::Provider,
    }
}

fn attach_agent(app: &mut App, current_exe: &std::path::Path) -> DashboardOutcome {
    opened(
        app.enter_selected_agent(current_exe),
        app,
        "cannot enter agent",
    )
}

fn force_attach_agent(app: &mut App, current_exe: &std::path::Path) -> DashboardOutcome {
    opened(
        app.force_enter_selected_agent(current_exe),
        app,
        "cannot take over agent",
    )
}

fn attach_shell(app: &mut App, current_exe: &std::path::Path) -> DashboardOutcome {
    opened(
        app.enter_selected_shell(current_exe),
        app,
        "cannot enter shell",
    )
}

/// Turns an attempt to open a workspace into an outcome the dashboard loop can act on,
/// leaving the banner behind when it did not open.
fn opened(
    opened: io::Result<WorkspaceDrive>,
    app: &mut App,
    failure: &'static str,
) -> DashboardOutcome {
    match opened {
        Ok(drive) => DashboardOutcome::Workspace {
            drive: Box::new(drive),
            failure,
        },
        Err(error) => {
            app.banner = Some(format!("{failure}: {error}"));
            DashboardOutcome::Continue
        }
    }
}

fn draw(frame: &mut Frame, app: &App) {
    let page = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(3),
        ])
        .split(frame.area());
    draw_header(frame, page[0], app);
    let sidebar_width = dashboard_sidebar_width(page[1].width);
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(sidebar_width), Constraint::Min(1)])
        .split(page[1]);
    draw_sessions(frame, body[0], app);
    draw_session_overview(frame, body[1], app);
    draw_footer(frame, page[2], app);
    if app.help_open {
        draw_help(frame, app);
    } else if let Some(dialog) = &app.dialog {
        draw_new_session_dialog(frame, dialog);
    } else if let Some(dialog) = &app.text_dialog {
        draw_text_dialog(frame, app, dialog);
    }
}

fn dashboard_sidebar_width(total_width: u16) -> u16 {
    let preferred = (total_width / 5).clamp(24, 32);
    preferred.min(total_width.saturating_sub(40).max(20))
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let (working, waiting, idle, failed) = app.status_counts();
    let mut spans = vec![
        Span::styled(
            " Agent Console ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("● {working} working"),
            Style::default().fg(Color::Green),
        ),
        Span::styled(
            format!("   ◐ {waiting} waiting"),
            Style::default().fg(Color::Yellow),
        ),
        Span::styled(
            format!("   ○ {idle} idle"),
            Style::default().fg(Color::DarkGray),
        ),
    ];
    if failed > 0 {
        spans.push(Span::styled(
            format!("   × {failed} failed"),
            Style::default().fg(Color::Red),
        ));
    }
    let unread = app.unread_notification_count();
    if unread > 0 {
        spans.push(Span::styled(
            format!("   ◆ {unread} alert"),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let mut lines = vec![Line::from(spans)];
    if let Some(line) = web_status_line(app.web_status()) {
        lines.push(line);
    }
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::BOTTOM)),
        area,
    );
}

/// The header's second line: where the web UI is, or why it is not there.
///
/// The header is three rows tall with only a bottom border, so this line already had room --
/// and it is the full width of the terminal, which is what lets it carry the whole tokened
/// URL without truncating the token.
fn web_status_line(status: &WebStatus) -> Option<Line<'static>> {
    match status {
        WebStatus::Disabled => None,
        WebStatus::Serving { url, auth, exposed } => {
            let mut spans = vec![
                Span::styled(" web ", Style::default().fg(Color::Black).bg(Color::Blue)),
                Span::raw(" "),
                Span::styled(url.clone(), Style::default().fg(Color::Cyan)),
            ];
            // Before the credential description, not after it: this line runs off the right
            // edge on a narrow terminal, and the exposure warning is the half that must
            // survive being clipped.
            if *exposed {
                spans.push(Span::styled(
                    "  ⚠ reachable from the network",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Red)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            spans.push(Span::styled(
                format!("   {auth}"),
                Style::default().fg(Color::DarkGray),
            ));
            Some(Line::from(spans))
        }
        WebStatus::Unavailable(reason) => Some(Line::from(vec![
            Span::styled(" web ", Style::default().fg(Color::Black).bg(Color::Red)),
            Span::styled(format!(" off · {reason}"), Style::default().fg(Color::Red)),
        ])),
    }
}

fn draw_sessions(frame: &mut Frame, area: Rect, app: &App) {
    let mut lines = Vec::new();
    let mut selected_last_line = 0;
    let mut last_workspace = None;
    let mut archived_group = false;
    for index in app.session_display_order() {
        let session = &app.sessions[index];
        let archived = app.session_archived(session);
        if archived && !archived_group {
            lines.push(Line::from(Span::styled(
                "▾ Archived",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )));
            archived_group = true;
            last_workspace = None;
        } else if !archived && last_workspace != Some(&session.cwd) {
            let label = session
                .cwd
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_else(|| session.cwd.to_str().unwrap_or("workspace"));
            lines.push(Line::from(Span::styled(
                format!("▾ {label}"),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
            last_workspace = Some(&session.cwd);
        }
        let (symbol, color) = status_style(session.status, app.tick_count);
        let selected = index == app.selected;
        let base = if selected {
            Style::default().bg(Color::Rgb(45, 53, 72))
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(if selected { "▸" } else { " " }, base.fg(Color::White)),
            Span::styled(format!("{symbol} "), base.fg(color)),
            Span::styled(
                format!("{:<3} ", session.agent.short_label()),
                base.fg(agent_color(session.agent)),
            ),
            Span::styled(
                format!("{} ", session.activity_age(unix_timestamp())),
                base.fg(Color::DarkGray),
            ),
            Span::styled(
                format!(
                    "{}{}",
                    if archived { "⌁ " } else { "" },
                    app.session_title(session)
                ),
                base.fg(if archived && !selected {
                    Color::DarkGray
                } else {
                    Color::White
                })
                .add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            ),
        ]));
        if selected {
            selected_last_line = lines.len();
        }
    }
    if lines.is_empty() {
        lines.push(Line::from("No persisted sessions found."));
        lines.push(Line::from("Press n to start one."));
    }
    let block = Block::default()
        .title(" SESSIONS ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .padding(Padding::new(1, 1, 0, 0));
    let visible_rows = usize::from(area.height.saturating_sub(2)).max(1);
    let scroll = selected_last_line.saturating_sub(visible_rows) as u16;
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)).block(block), area);
}

const SESSION_CARD_HEIGHT: u16 = 7;

fn session_card_layout(area: Rect, item_count: usize, selected: usize) -> Vec<(usize, Rect)> {
    if item_count == 0 || area.width == 0 || area.height == 0 {
        return Vec::new();
    }
    let columns = usize::from((area.width / 36).clamp(1, 3)).min(item_count);
    let visible_rows = usize::from((area.height / SESSION_CARD_HEIGHT).max(1));
    let selected_row = selected.min(item_count - 1) / columns;
    let first_row = selected_row.saturating_sub(visible_rows - 1);
    let first = first_row * columns;
    let count = (visible_rows * columns).min(item_count.saturating_sub(first));
    let gaps = columns.saturating_sub(1) as u16;
    let usable = area.width.saturating_sub(gaps);
    let base_width = usable / columns as u16;
    let remainder = usable % columns as u16;

    (0..count)
        .map(|offset| {
            let column = offset % columns;
            let row = offset / columns;
            let left = area.x
                + (0..column)
                    .map(|index| base_width + u16::from((index as u16) < remainder) + 1)
                    .sum::<u16>();
            let width = base_width + u16::from((column as u16) < remainder);
            let top = area.y + row as u16 * SESSION_CARD_HEIGHT;
            let height = SESSION_CARD_HEIGHT.min(area.bottom().saturating_sub(top));
            (first + offset, Rect::new(left, top, width, height))
        })
        .collect()
}

fn session_priority(session: &model::Session) -> (&'static str, String, Color) {
    match session.status {
        SessionStatus::Waiting => (
            "NEEDS YOU",
            session
                .pending_decisions
                .first()
                .or_else(|| session.summary.needs_user.first())
                .map(|decision| decision.question.clone())
                .or_else(|| {
                    (!session.summary.current_action.trim().is_empty())
                        .then(|| session.summary.current_action.trim().to_owned())
                })
                .or_else(|| session.recent_activity.last().cloned())
                .unwrap_or_else(|| "Waiting for your input".into()),
            Color::Yellow,
        ),
        SessionStatus::Failed => (
            "BLOCKER",
            session
                .summary
                .blockers
                .first()
                .cloned()
                .or_else(|| session.unavailable_reason.clone())
                .or_else(|| session.summary_error.clone())
                .or_else(|| session.recent_activity.last().cloned())
                .unwrap_or_else(|| "Session failed; open it for the provider error".into()),
            Color::Red,
        ),
        SessionStatus::Working => (
            "NOW",
            (!session.summary.current_action.trim().is_empty())
                .then(|| session.summary.current_action.trim().to_owned())
                .or_else(|| session.recent_activity.last().cloned())
                .unwrap_or_else(|| "Agent is working".into()),
            Color::Green,
        ),
        SessionStatus::Idle => (
            "LAST",
            session
                .recent_activity
                .last()
                .cloned()
                .or_else(|| session.summary.progress.last().cloned())
                .unwrap_or_else(|| "No recent activity".into()),
            Color::Gray,
        ),
    }
}

fn draw_session_overview(frame: &mut Frame, area: Rect, app: &App) {
    let order = app.session_display_order();
    let selected = order
        .iter()
        .position(|index| *index == app.selected)
        .unwrap_or(0);
    let outer = Block::default()
        .title(format!(
            " SESSION OVERVIEW · {} sessions · selected {}/{} ",
            order.len(),
            selected.saturating_add(1).min(order.len()),
            order.len()
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .padding(Padding::uniform(1));
    let inner = outer.inner(area);
    frame.render_widget(outer, area);
    if order.is_empty() {
        frame.render_widget(Paragraph::new("Press [n] to start a session."), inner);
        return;
    }

    let regions = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);
    draw_selected_session_focus(frame, regions[0], app, order[selected]);
    draw_session_cards(frame, regions[2], app, &order, selected);
}

fn draw_selected_session_focus(frame: &mut Frame, area: Rect, app: &App, index: usize) {
    let session = &app.sessions[index];
    let (symbol, status_color) = status_style(session.status, app.tick_count);
    let (priority_label, priority, priority_color) = session_priority(session);
    let branch = session.branch.as_deref().unwrap_or("no branch");
    let next = (!session.summary.next_step.trim().is_empty())
        .then(|| session.summary.next_step.trim())
        .or_else(|| session.summary.progress.last().map(String::as_str))
        .unwrap_or("Open the session to continue");
    let lines = vec![
        Line::from(vec![
            Span::styled("TASK  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                app.session_title(session),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                format!("{priority_label:<10}"),
                Style::default()
                    .fg(priority_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(priority, Style::default().fg(priority_color)),
        ]),
        Line::from(vec![
            Span::styled("NEXT      ", Style::default().fg(Color::DarkGray)),
            Span::styled(next.to_owned(), Style::default().fg(Color::Gray)),
        ]),
        Line::from(vec![
            Span::styled("WORKSPACE ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{} · {branch}", session.cwd.display()),
                Style::default().fg(Color::Cyan),
            ),
        ]),
    ];
    let block = Block::default()
        .title(format!(
            " SELECTED SESSION · {} · {symbol} {} · {} · {} shell{} ",
            session.agent.short_label(),
            session.status.label().to_uppercase(),
            session.activity_age(unix_timestamp()),
            app.session_shell_count(session),
            if app.session_shell_count(session) == 1 {
                ""
            } else {
                "s"
            }
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(status_color))
        .padding(Padding::horizontal(1));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .style(Style::default().bg(Color::Rgb(18, 28, 38)))
            .block(block),
        area,
    );
}

fn draw_session_cards(frame: &mut Frame, area: Rect, app: &App, order: &[usize], selected: usize) {
    for (position, rect) in session_card_layout(area, order.len(), selected) {
        let session = &app.sessions[order[position]];
        let is_selected = order[position] == app.selected;
        let (symbol, status_color) = status_style(session.status, app.tick_count);
        let workspace = session
            .cwd
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("workspace");
        let (priority_label, priority, priority_color) = session_priority(session);
        let branch = session.branch.as_deref().unwrap_or("no branch");
        let context = format!("{workspace} · {branch}");
        let next = (!session.summary.next_step.trim().is_empty())
            .then(|| session.summary.next_step.trim())
            .or_else(|| session.summary.progress.last().map(String::as_str));
        let lines = vec![
            Line::from(vec![
                Span::styled("TASK ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    app.session_title(session),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled(
                    format!("{priority_label} "),
                    Style::default()
                        .fg(priority_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(priority, Style::default().fg(priority_color)),
            ]),
            Line::from(vec![
                Span::styled("↳ ", Style::default().fg(Color::DarkGray)),
                Span::styled(context, Style::default().fg(Color::Cyan)),
            ]),
            next.map_or_else(Line::default, |value| {
                Line::from(vec![
                    Span::styled("NEXT ", Style::default().fg(Color::DarkGray)),
                    Span::styled(value.to_owned(), Style::default().fg(Color::Gray)),
                ])
            }),
            Line::default(),
        ];
        let title = format!(
            " {}{}{} · {symbol} {} · {} · sh:{} ",
            if is_selected { "▸ " } else { "" },
            session.agent.short_label(),
            if app.session_archived(session) {
                "⌁ "
            } else {
                ""
            },
            session.status.label().to_uppercase(),
            session.activity_age(unix_timestamp()),
            app.session_shell_count(session),
        );
        let border_color = match (is_selected, session.status) {
            (true, SessionStatus::Idle) => Color::Cyan,
            (_, SessionStatus::Idle) => Color::DarkGray,
            _ => status_color,
        };
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));
        let style = if is_selected {
            Style::default().bg(Color::Rgb(18, 28, 38))
        } else {
            Style::default()
        };
        frame.render_widget(Paragraph::new(lines).style(style).block(block), rect);
    }
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let line = Line::from(vec![
        key(format!(
            "{}/{}",
            app.dashboard_key_label("previous"),
            app.dashboard_key_label("next")
        )),
        hint(" select  "),
        key(app.dashboard_key_label("enter")),
        hint(" agent  "),
        key(app.dashboard_key_label("shell")),
        hint(" shell  "),
        key(app.dashboard_key_label("new")),
        hint(" new  "),
        key(app.dashboard_key_label("alert")),
        hint(" alert  "),
        key(app.dashboard_key_label("search")),
        hint(" search  "),
        key(app.dashboard_key_label("archive")),
        hint(" archive/restore  "),
        key(app.dashboard_key_label("help")),
        hint(" help  "),
        key(app.dashboard_key_label("quit")),
        hint(" quit"),
    ]);
    let notification_banner = app.active_notification().map(|notification| {
        let title = app
            .sessions
            .iter()
            .find(|session| session.key == notification.session_key)
            .map(model::Session::list_title)
            .unwrap_or_else(|| "session".into());
        format!(
            "ALERT · {} · {title}: {} · press {} to jump",
            notification.status.label(),
            notification.message,
            app.dashboard_key_label("alert")
        )
    });
    let banner = app
        .banner
        .as_deref()
        .map(str::to_owned)
        .or(notification_banner)
        .unwrap_or_else(|| {
            "Workspace controls are configurable; open Help for the active bindings".into()
        });
    frame.render_widget(
        Paragraph::new(vec![
            line,
            Line::from(Span::styled(banner, Style::default().fg(Color::Yellow))),
        ])
        .block(Block::default().borders(Borders::TOP)),
        area,
    );
}

fn draw_text_dialog(frame: &mut Frame, app: &App, dialog: &app::TextDialog) {
    let area = centered_rect(60, 5, frame.area());
    frame.render_widget(WidgetClear, area);
    let (title, placeholder) = match dialog.kind {
        TextDialogKind::Search => (
            " SEARCH SESSIONS ",
            "task, alias, path, branch, ID, provider",
        ),
        TextDialogKind::Alias => (" SESSION ALIAS ", "empty value clears the user alias"),
    };
    let value = if dialog.value.is_empty() {
        Span::styled(placeholder, Style::default().fg(Color::DarkGray))
    } else {
        Span::styled(&dialog.value, Style::default().fg(Color::White))
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(value),
            Line::from(match dialog.kind {
                TextDialogKind::Search => format!(
                    "{} matches · live filter · Enter keep · Esc cancel",
                    app.session_display_order().len()
                ),
                TextDialogKind::Alias => "Enter apply · empty clears · Esc cancel".into(),
            }),
        ])
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .padding(Padding::new(1, 1, 0, 0)),
        ),
        area,
    );
}

fn draw_help(frame: &mut Frame, app: &App) {
    let area = centered_rect(98, frame.area().height.saturating_sub(2), frame.area());
    frame.render_widget(WidgetClear, area);
    let title = format!(
        " KEY BINDINGS · Esc / {} close ",
        app.dashboard_key_label("help")
    );
    let inner = Block::default()
        .title(title.clone())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .inner(area);
    frame.render_widget(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
        area,
    );
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .split(inner);
    let all = app.help_lines();
    let direct = all
        .iter()
        .position(|line| line == "WORKSPACE · DIRECT")
        .unwrap_or(all.len());
    let session_list = all
        .iter()
        .position(|line| line == "WORKSPACE · SESSION LIST")
        .unwrap_or(all.len());
    let viewport = all
        .iter()
        .position(|line| line == "WORKSPACE · CHILD VIEWPORT")
        .unwrap_or(all.len());
    let dashboard_lines = all[1..direct]
        .iter()
        .map(|line| Line::from(line.clone()))
        .collect::<Vec<_>>();
    let direct_lines = all
        .get(direct.saturating_add(1)..session_list)
        .unwrap_or_default()
        .iter()
        .chain(all.get(viewport..).unwrap_or_default())
        .map(|line| Line::from(line.clone()))
        .collect::<Vec<_>>();
    let session_lines = all
        .get(session_list.saturating_add(1)..viewport)
        .unwrap_or_default()
        .iter()
        .map(|line| Line::from(line.clone()))
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(dashboard_lines).block(
            Block::default()
                .title(" DASHBOARD ")
                .padding(Padding::new(1, 1, 0, 0)),
        ),
        columns[0],
    );
    frame.render_widget(
        Paragraph::new(direct_lines).block(
            Block::default()
                .title(" WORKSPACE · DIRECT ")
                .borders(Borders::LEFT)
                .padding(Padding::new(1, 1, 0, 0)),
        ),
        columns[1],
    );
    frame.render_widget(
        Paragraph::new(session_lines).block(
            Block::default()
                .title(" WORKSPACE · SESSION LIST ")
                .borders(Borders::LEFT)
                .padding(Padding::new(1, 1, 0, 0)),
        ),
        columns[2],
    );
}

#[derive(Debug, Eq, PartialEq)]
struct WorkspaceEditorView {
    hidden_before: bool,
    before: String,
    cursor: Option<char>,
    after: String,
    hidden_after: bool,
}

impl WorkspaceEditorView {
    #[cfg(test)]
    fn display_width(&self) -> usize {
        usize::from(self.hidden_before)
            + self
                .before
                .chars()
                .chain(self.cursor)
                .chain(self.after.chars())
                .map(character_width)
                .sum::<usize>()
            + usize::from(self.cursor.is_none())
            + usize::from(self.hidden_after)
    }
}

fn character_width(character: char) -> usize {
    UnicodeWidthChar::width(character).unwrap_or(0).max(1)
}

fn workspace_editor_view(value: &str, cursor: usize, max_columns: usize) -> WorkspaceEditorView {
    let characters = value.chars().collect::<Vec<_>>();
    let cursor = cursor.min(characters.len());
    let max_columns = max_columns.max(1);
    let content_columns = max_columns.saturating_sub(2).max(1);
    let cursor_width = characters.get(cursor).copied().map_or(1, character_width);
    let mut used = cursor_width;
    let mut left_width = 0;
    let mut right_width = 0;
    let mut start = cursor;
    let mut end = (cursor + usize::from(cursor < characters.len())).min(characters.len());

    loop {
        let left = start
            .checked_sub(1)
            .map(|index| (index, character_width(characters[index])));
        let right = (end < characters.len()).then(|| (end, character_width(characters[end])));
        let prefer_left = left_width <= right_width;
        let candidates = if prefer_left {
            [
                left.map(|item| (true, item)),
                right.map(|item| (false, item)),
            ]
        } else {
            [
                right.map(|item| (false, item)),
                left.map(|item| (true, item)),
            ]
        };
        let Some((take_left, (index, width))) = candidates
            .into_iter()
            .flatten()
            .find(|(_, (_, width))| used.saturating_add(*width) <= content_columns)
        else {
            break;
        };
        used += width;
        if take_left {
            start = index;
            left_width += width;
        } else {
            end = index + 1;
            right_width += width;
        }
    }

    WorkspaceEditorView {
        hidden_before: start > 0,
        before: characters[start..cursor].iter().collect(),
        cursor: characters.get(cursor).copied(),
        after: characters[(cursor + usize::from(cursor < characters.len()))..end]
            .iter()
            .collect(),
        hidden_after: end < characters.len(),
    }
}

fn workspace_editor_spans(view: WorkspaceEditorView) -> Vec<Span<'static>> {
    let mut spans = Vec::with_capacity(5);
    if view.hidden_before {
        spans.push(Span::styled("…", Style::default().fg(Color::DarkGray)));
    }
    spans.push(Span::styled(view.before, Style::default().fg(Color::White)));
    spans.push(Span::styled(
        view.cursor
            .map_or_else(|| " ".into(), |value| value.to_string()),
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(view.after, Style::default().fg(Color::White)));
    if view.hidden_after {
        spans.push(Span::styled("…", Style::default().fg(Color::DarkGray)));
    }
    spans
}

fn draw_new_session_dialog(frame: &mut Frame, dialog: &app::NewSessionDialog) {
    let area = centered_rect(72, 12, frame.area());
    frame.render_widget(WidgetClear, area);
    let provider_style = if dialog.field == DialogField::Provider {
        Style::default().fg(Color::Black).bg(Color::Cyan)
    } else {
        Style::default().fg(Color::Cyan)
    };
    let workspace_field_width = area.width.saturating_sub(14) as usize;
    let workspace_value = if dialog.field == DialogField::Cwd && dialog.cwd_replace_on_input {
        let value = if dialog.cwd.is_empty() {
            "<type or paste a path>".to_owned()
        } else {
            let view = workspace_editor_view(
                &dialog.cwd,
                dialog.cwd.chars().count(),
                workspace_field_width.saturating_sub(2),
            );
            format!(
                "{}{}{}{}",
                if view.hidden_before { "…" } else { "" },
                view.before,
                view.cursor
                    .into_iter()
                    .chain(view.after.chars())
                    .collect::<String>(),
                if view.hidden_after { "…" } else { "" }
            )
        };
        vec![Span::styled(
            format!(" {value} "),
            Style::default().fg(Color::Black).bg(Color::Cyan),
        )]
    } else if dialog.field == DialogField::Cwd {
        let view = workspace_editor_view(&dialog.cwd, dialog.cwd_cursor, workspace_field_width);
        workspace_editor_spans(view)
    } else {
        vec![Span::styled(&dialog.cwd, Style::default().fg(Color::White))]
    };
    let completions = if dialog.field == DialogField::Cwd && !dialog.cwd_replace_on_input {
        dialog_workspace_completions(dialog, dirs::home_dir().as_deref())
    } else {
        Vec::new()
    };
    let guidance = if dialog.field == DialogField::Cwd {
        if dialog.cwd_replace_on_input {
            "type/paste replace · arrows edit · Shift-Tab provider · Enter start · Esc cancel"
        } else if dialog.cwd_cursor < dialog.cwd.chars().count() {
            "arrows move · Bksp/Del edit · Shift-Tab provider · Enter start · Esc cancel"
        } else if dialog.cwd_completion_accepted {
            "Completed · type child or Shift-Tab provider · Enter start · Esc cancel"
        } else if !completions.is_empty() {
            "↑/↓ choose · Tab complete · Shift-Tab provider · Enter start · Esc cancel"
        } else {
            "type/paste path · arrows move · Shift-Tab provider · Enter validate · Esc cancel"
        }
    } else {
        "Shift-Tab workspace · arrows/h/l change · Enter start · Esc cancel"
    };
    let mut workspace_line = vec![Span::styled(
        "workspace ",
        Style::default().fg(Color::DarkGray),
    )];
    workspace_line.extend(workspace_value);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("provider  ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!(" < {} > ", dialog.provider.label()), provider_style),
        ]),
        Line::default(),
        Line::from(workspace_line),
    ];
    let first_completion = dialog
        .cwd_completion_index
        .saturating_sub(2)
        .min(completions.len().saturating_sub(3));
    for row in 0..3 {
        let index = first_completion + row;
        lines.push(
            completions
                .get(index)
                .map_or_else(Line::default, |candidate| {
                    let selected = index == dialog.cwd_completion_index.min(completions.len() - 1);
                    Line::from(vec![
                        Span::raw("          "),
                        Span::styled(
                            if selected { "› " } else { "  " },
                            Style::default().fg(Color::Cyan),
                        ),
                        Span::styled(
                            candidate,
                            if selected {
                                Style::default()
                                    .fg(Color::Black)
                                    .bg(Color::Cyan)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(Color::DarkGray)
                            },
                        ),
                    ])
                }),
        );
    }
    lines.extend([
        Line::default(),
        Line::from(Span::styled(
            dialog.error.as_deref().unwrap_or(guidance),
            Style::default().fg(if dialog.error.is_some() {
                Color::Red
            } else {
                Color::DarkGray
            }),
        )),
    ]);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(" NEW SESSION ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .padding(Padding::uniform(1)),
        ),
        area,
    );
}

fn centered_rect(width_percent: u16, height: u16, area: Rect) -> Rect {
    let width = area
        .width
        .saturating_mul(width_percent)
        .saturating_div(100)
        .max(20);
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn key(value: impl Into<String>) -> Span<'static> {
    Span::styled(
        format!("[{}]", value.into()),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )
}

fn hint(value: &'static str) -> Span<'static> {
    Span::styled(value, Style::default().fg(Color::Gray))
}

fn agent_color(agent: AgentKind) -> Color {
    match agent {
        AgentKind::Claude => Color::Rgb(219, 126, 82),
        AgentKind::Codex => Color::Cyan,
        AgentKind::Pi => Color::Rgb(147, 197, 114),
    }
}

fn status_style(status: SessionStatus, tick: u64) -> (&'static str, Color) {
    match status {
        SessionStatus::Working => {
            let spinner = ["◐", "◓", "◑", "◒"][(tick as usize / 2) % 4];
            (spinner, Color::Green)
        }
        SessionStatus::Waiting => ("!", Color::Yellow),
        SessionStatus::Failed => ("×", Color::Red),
        SessionStatus::Idle => ("○", Color::DarkGray),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use ratatui::backend::TestBackend;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn keycaps_do_not_depend_on_a_dark_background_for_contrast() {
        let span = key("q");
        assert_eq!(span.content.as_ref(), "[q]");
        assert_ne!(span.style.fg, Some(Color::Black));
        assert_eq!(span.style.bg, None);
    }

    /// The field is drawn as `< codex >`, one arrow per direction. Each arrow has to walk its
    /// own way round the ring, or the control lies about what it does.
    #[test]
    fn the_provider_arrows_walk_the_ring_in_opposite_directions() {
        let mut app = App::test_fixture();
        app.open_new_dialog();
        let provider = |app: &App| app.dialog.as_ref().unwrap().provider;
        assert_eq!(provider(&app), AgentKind::Codex);

        for expected in [AgentKind::Claude, AgentKind::Pi, AgentKind::Codex] {
            handle_dialog_key(&mut app, KeyCode::Right, std::path::Path::new("/tmp/ac")).unwrap();
            assert_eq!(provider(&app), expected);
        }
        for expected in [AgentKind::Pi, AgentKind::Claude, AgentKind::Codex] {
            handle_dialog_key(&mut app, KeyCode::Left, std::path::Path::new("/tmp/ac")).unwrap();
            assert_eq!(provider(&app), expected);
        }
    }

    #[test]
    fn shift_tab_moves_new_session_fields_backwards() {
        assert_eq!(toggle_dialog_field(DialogField::Provider), DialogField::Cwd);
        assert_eq!(toggle_dialog_field(DialogField::Cwd), DialogField::Provider);
    }

    #[test]
    fn dashboard_renders_at_small_and_large_sizes() {
        for (width, height) in [(80, 24), (160, 50)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            let app = App::test_fixture();
            terminal.draw(|frame| draw(frame, &app)).unwrap();
        }
    }

    #[test]
    fn session_list_groups_workspaces_and_uses_first_prompt_titles() {
        let mut app = App::test_fixture();
        app.sessions[0].first_prompt = Some("refresh tokens".into());
        let mut same_workspace = app.sessions[0].clone();
        same_workspace.key = "claude:timeout".into();
        same_workspace.agent = AgentKind::Claude;
        same_workspace.first_prompt = Some("fix timeout".into());
        let mut other_workspace = app.sessions[0].clone();
        other_workspace.key = "codex:frontend".into();
        other_workspace.name = "frontend".into();
        other_workspace.cwd = "/tmp/frontend".into();
        other_workspace.first_prompt = Some("update navbar".into());
        app.sessions = vec![app.sessions[0].clone(), same_workspace, other_workspace];
        app.selected = 1;
        app.toggle_selected_archive().unwrap();

        let backend = TestBackend::new(160, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut sidebar = String::new();
        for y in 3..27 {
            for x in 0..32 {
                sidebar.push_str(buffer.cell((x, y)).unwrap().symbol());
            }
            sidebar.push('\n');
        }

        assert_eq!(sidebar.matches("backend-api").count(), 1);
        assert!(
            sidebar
                .lines()
                .any(|line| line.contains("Cdx") && line.contains("refresh tokens"))
        );
        assert!(
            sidebar
                .lines()
                .any(|line| line.contains("Cla") && line.contains("fix timeout"))
        );
        assert!(sidebar.contains("frontend"));
        assert!(
            sidebar
                .lines()
                .any(|line| line.contains("Cdx") && line.contains("update navbar"))
        );
        assert_eq!(sidebar.matches("Archived").count(), 1);
        assert!(sidebar.find("frontend").unwrap() < sidebar.find("Archived").unwrap());
        assert!(sidebar.find("Archived").unwrap() < sidebar.find("fix timeout").unwrap());
    }

    #[test]
    fn dashboard_sidebar_is_compact_on_wide_terminals() {
        assert_eq!(dashboard_sidebar_width(120), 24);
        assert_eq!(dashboard_sidebar_width(160), 32);
        assert_eq!(dashboard_sidebar_width(240), 32);
    }

    #[test]
    fn session_card_grid_adapts_columns_and_keeps_selection_visible() {
        let narrow = session_card_layout(Rect::new(0, 0, 30, 16), 8, 7);
        let medium = session_card_layout(Rect::new(0, 0, 80, 16), 8, 7);
        let wide = session_card_layout(Rect::new(0, 0, 120, 16), 8, 7);
        let ultra_wide = session_card_layout(Rect::new(0, 0, 180, 16), 8, 7);

        assert_eq!(
            narrow
                .iter()
                .filter(|(_, rect)| rect.y == narrow[0].1.y)
                .count(),
            1
        );
        assert_eq!(
            medium
                .iter()
                .filter(|(_, rect)| rect.y == medium[0].1.y)
                .count(),
            2
        );
        assert_eq!(
            wide.iter()
                .filter(|(_, rect)| rect.y == wide[0].1.y)
                .count(),
            3
        );
        assert_eq!(
            ultra_wide
                .iter()
                .filter(|(_, rect)| rect.y == ultra_wide[0].1.y)
                .count(),
            3
        );
        assert!(narrow.iter().any(|(index, _)| *index == 7));
        assert!(medium.iter().any(|(index, _)| *index == 7));
        assert!(wide.iter().any(|(index, _)| *index == 7));
    }

    #[test]
    fn dashboard_leads_with_the_selected_session_task_and_current_focus() {
        let backend = TestBackend::new(160, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::test_fixture();
        app.sessions[0].first_prompt = Some("FOCUS-KEY-INFO".into());
        app.sessions[0].status = SessionStatus::Working;
        app.sessions[0].summary.current_action = "running the focused regression".into();
        app.sessions[0].summary.next_step = "verify the real terminal".into();

        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("SELECTED SESSION"));
        assert!(rendered.contains("TASK FOCUS-KEY-INFO"));
        assert!(rendered.contains("NOW running the focused regression"));
        assert!(rendered.contains("NEXT verify the real terminal"));
    }

    #[test]
    fn accepting_workspace_completion_keeps_the_field_active_for_child_completion() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("alpha-one")).unwrap();
        let mut app = App::test_fixture();
        app.open_new_dialog();
        let dialog = app.dialog.as_mut().unwrap();
        dialog.field = DialogField::Cwd;
        dialog.cwd = format!("{}/alpha", root.path().display());
        dialog.cwd_cursor = dialog.cwd.chars().count();
        dialog.cwd_replace_on_input = false;

        accept_workspace_completion(dialog);

        assert_eq!(dialog.cwd, format!("{}/alpha-one/", root.path().display()));
        assert!(dialog.cwd_completion_accepted);
        assert_eq!(dialog.field, DialogField::Cwd);
    }

    #[test]
    fn failed_session_priority_has_a_visible_fallback_without_a_summary_blocker() {
        let mut app = App::test_fixture();
        let session = &mut app.sessions[0];
        session.status = SessionStatus::Failed;
        session.summary.blockers.clear();
        session.unavailable_reason = None;
        session.summary_error = None;
        session.recent_activity.clear();

        let (label, detail, color) = session_priority(session);

        assert_eq!(label, "BLOCKER");
        assert!(detail.contains("Session failed"));
        assert_eq!(color, Color::Red);
    }

    #[test]
    fn typing_in_focused_workspace_replaces_the_default_path() {
        let mut app = App::test_fixture();
        app.open_new_dialog();
        let dialog = app.dialog.as_mut().unwrap();
        dialog.field = DialogField::Cwd;

        for character in "/tmp".chars() {
            edit_dialog_cwd(dialog, KeyCode::Char(character));
        }

        assert_eq!(dialog.cwd, "/tmp");
        assert!(!dialog.cwd_replace_on_input);
    }

    #[test]
    fn workspace_editor_inserts_and_deletes_at_the_cursor() {
        let mut app = App::test_fixture();
        app.open_new_dialog();
        let dialog = app.dialog.as_mut().unwrap();
        dialog.field = DialogField::Cwd;
        dialog.cwd = "/tmp/ac".into();
        dialog.cwd_cursor = 6;
        dialog.cwd_replace_on_input = false;

        edit_dialog_cwd(dialog, KeyCode::Char('b'));
        assert_eq!(dialog.cwd, "/tmp/abc");
        assert_eq!(dialog.cwd_cursor, 7);

        edit_dialog_cwd(dialog, KeyCode::Right);
        assert_eq!(dialog.cwd_cursor, 8);
        edit_dialog_cwd(dialog, KeyCode::Left);
        edit_dialog_cwd(dialog, KeyCode::Delete);
        assert_eq!(dialog.cwd, "/tmp/ab");
        edit_dialog_cwd(dialog, KeyCode::Home);
        edit_dialog_cwd(dialog, KeyCode::Delete);
        assert_eq!(dialog.cwd, "tmp/ab");
        edit_dialog_cwd(dialog, KeyCode::End);
        edit_dialog_cwd(dialog, KeyCode::Backspace);
        assert_eq!(dialog.cwd, "tmp/a");
    }

    #[test]
    fn workspace_editor_moves_and_deletes_unicode_characters_safely() {
        let mut app = App::test_fixture();
        app.open_new_dialog();
        let dialog = app.dialog.as_mut().unwrap();
        dialog.field = DialogField::Cwd;
        dialog.cwd = "/tmp/项目".into();
        dialog.cwd_cursor = dialog.cwd.chars().count();
        dialog.cwd_replace_on_input = false;

        edit_dialog_cwd(dialog, KeyCode::Left);
        edit_dialog_cwd(dialog, KeyCode::Backspace);

        assert_eq!(dialog.cwd, "/tmp/目");
        assert_eq!(dialog.cwd_cursor, 5);
    }

    #[test]
    fn workspace_editor_view_keeps_a_long_path_cursor_visible() {
        let value = "/Users/example/workspace/非常长的目录/project";
        let cursor = value.chars().count().saturating_sub(7);
        let view = workspace_editor_view(value, cursor, 18);

        assert!(view.hidden_before);
        assert!(view.before.chars().count() < cursor);
        assert!(view.cursor.is_some());
        assert!(view.display_width() <= 18);
    }

    #[test]
    fn dashboard_highlights_the_card_linked_to_the_sidebar_selection() {
        let backend = TestBackend::new(160, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = App::test_fixture();

        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut rendered = String::new();
        for y in 0..buffer.area().height {
            for x in 0..buffer.area().width {
                rendered.push_str(buffer.cell((x, y)).unwrap().symbol());
            }
            rendered.push('\n');
        }

        assert!(rendered.contains("SESSION OVERVIEW"));
        assert!(rendered.contains("▸ Cdx"));
        assert!(!rendered.contains("LATEST ACTIVITY"));
    }

    /// The web address belongs in the help panel too, and it has to land in the DASHBOARD
    /// column: that column is `help_lines()` up to the first `WORKSPACE ...` heading, so a
    /// block appended at the end would show up under "child viewport" bindings instead.
    #[test]
    fn help_panel_carries_the_web_address_in_the_dashboard_column() {
        let backend = TestBackend::new(140, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::test_fixture();
        app.set_web_status(WebStatus::Serving {
            url: "http://127.0.0.1:7878/?token=abc".into(),
            auth: "HTTP Basic auth as user \"alice\"".into(),
            exposed: false,
        });
        app.help_open = true;

        let lines = app.help_lines();
        let web = lines
            .iter()
            .position(|line| line == "WEB UI")
            .expect("the help panel names the web server");
        let workspace = lines
            .iter()
            .position(|line| line.starts_with("WORKSPACE"))
            .expect("the workspace headings are what split the columns");
        assert!(web < workspace, "the web block has to stay in column one");
        assert!(lines[web + 1].contains("http://127.0.0.1:7878"));
        assert!(
            !lines[web + 1].contains("token"),
            "a token clipped by the column boundary is worse than none: {}",
            lines[web + 1]
        );

        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("WEB UI"));
        assert!(rendered.contains("http://127.0.0.1:7878"));
        // The bindings the panel existed for are still all there.
        assert!(rendered.contains("WORKSPACE · DIRECT"));
        assert!(rendered.contains("CHILD VIEWPORT"));
        assert!(rendered.contains("quit"));
    }

    #[test]
    fn help_panel_separates_direct_session_list_and_child_bindings() {
        let backend = TestBackend::new(140, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::test_fixture();
        app.help_open = true;

        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("WORKSPACE · DIRECT"));
        assert!(rendered.contains("WORKSPACE · SESSION LIST"));
        assert!(rendered.contains("select session"));
        assert!(rendered.contains("open agent"));
        assert!(rendered.contains("focus last shell"));
        assert!(rendered.contains("focus agent"));
        assert!(rendered.contains("CHILD VIEWPORT"));
        assert!(!rendered.contains("hide_shells"));
        assert!(rendered.contains("Ctrl-\\"));
        assert!(!rendered.contains("Ctrl-T"));
        assert!(rendered.contains("Ctrl-X"));
        assert!(rendered.contains("Esc / ? close"));
    }

    #[test]
    fn help_panel_keeps_every_group_visible_at_a_standard_terminal_size() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::test_fixture();
        app.help_open = true;

        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("force takeover"));
        assert!(rendered.contains("sidebar selection"));
        assert!(rendered.contains("CHILD VIEWPORT"));
        assert!(rendered.contains("scroll pointed pane"));
        assert!(rendered.contains("select / copy text"));
        assert!(rendered.contains("copy command output"));
        assert!(rendered.contains("select shell"));
    }

    #[test]
    fn search_and_new_session_dialogs_show_their_contextual_controls() {
        let backend = TestBackend::new(140, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::test_fixture();

        app.open_search_dialog();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let search = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(search.contains("live filter"));
        assert!(search.contains("Enter keep"));
        assert!(search.contains("Esc cancel"));

        app.cancel_text_dialog();
        app.open_alias_dialog();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let alias = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(alias.contains("SESSION ALIAS"));
        assert!(alias.contains("Enter apply"));
        assert!(alias.contains("empty clears"));
        assert!(alias.contains("Esc cancel"));

        app.cancel_text_dialog();
        app.open_new_dialog();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let provider = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(provider.contains("Shift-Tab workspace"));
        assert!(provider.contains("Enter start"));
        assert!(provider.contains("Esc cancel"));

        app.dialog.as_mut().unwrap().field = DialogField::Cwd;
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let workspace = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(workspace.contains("Shift-Tab provider"));
        assert!(workspace.contains("Enter start"));
        assert!(workspace.contains("Esc cancel"));
    }

    #[test]
    fn dashboard_footer_shows_all_primary_controls() {
        let backend = TestBackend::new(180, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = App::test_fixture();

        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        for label in [
            "select",
            "agent",
            "shell",
            "new",
            "alert",
            "search",
            "archive/restore",
            "help",
            "quit",
        ] {
            assert!(rendered.contains(label), "missing dashboard hint: {label}");
        }
    }

    #[test]
    fn search_filters_and_normalizes_selection_before_enter() {
        let mut app = App::test_fixture();
        app.sessions[0].summary.task = "unrelated task".into();
        let mut matching = app.sessions[0].clone();
        matching.key = "claude:latency".into();
        matching.provider_session_id = "latency".into();
        matching.summary.task = "investigate API latency".into();
        app.sessions.push(matching);

        app.open_search_dialog();
        for character in "latency".chars() {
            handle_text_dialog_key(&mut app, KeyCode::Char(character));
        }

        assert_eq!(app.session_display_order(), vec![1]);
        assert_eq!(app.selected, 1);
        assert!(app.text_dialog.is_some(), "live search remains editable");

        handle_text_dialog_key(&mut app, KeyCode::Esc);
        assert_eq!(app.session_display_order(), vec![0, 1]);
        assert_eq!(app.selected, 0, "cancel restores the original selection");

        app.open_search_dialog();
        for character in "latency".chars() {
            handle_text_dialog_key(&mut app, KeyCode::Char(character));
        }
        for _ in 0.."latency".len() {
            handle_text_dialog_key(&mut app, KeyCode::Backspace);
        }
        assert_eq!(app.session_display_order(), vec![0, 1]);

        for character in "latency".chars() {
            handle_text_dialog_key(&mut app, KeyCode::Char(character));
        }
        handle_text_dialog_key(&mut app, KeyCode::Enter);
        assert_eq!(app.session_display_order(), vec![1]);
        assert!(app.text_dialog.is_none());
    }

    #[test]
    fn dashboard_mouse_wheel_navigates_the_session_list() {
        let mut app = App::test_fixture();
        let mut second = app.sessions[0].clone();
        second.key = "claude:second".into();
        second.provider_session_id = "second".into();
        app.sessions.push(second);

        handle_dashboard_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 1,
                row: 4,
                modifiers: KeyModifiers::NONE,
            },
        );

        assert_eq!(app.selected, 1);
    }

    #[test]
    fn cli_help_exposes_non_interactive_metadata_commands() {
        let help = cli_help();
        assert!(help.starts_with(&format!("Agent Console {}\n", env!("CARGO_PKG_VERSION"))));
        assert!(help.contains("agent-console --help"));
        assert!(help.contains("agent-console --version"));
    }

    /// The dashboard serving the web UI is not discoverable unless the help says so, and
    /// `web` has to stay documented for the headless case rather than look retired.
    #[test]
    fn cli_help_documents_the_embedded_server_and_keeps_the_web_subcommand() {
        let help = cli_help();
        for expected in [
            "--host",
            "--port",
            "--auth",
            "--no-web",
            "agent-console web",
            "AGENT_CONSOLE_WEB_HOST",
            "AGENT_CONSOLE_WEB_AUTH",
            "[web] host/port/auth/\n",
        ] {
            assert!(help.contains(expected), "cli_help is missing {expected}");
        }
        assert!(
            help.contains("visible in `ps`"),
            "the help has to say why a password does not belong in argv"
        );
    }

    fn parsed(args: &[&str], allow_disable: bool) -> web::WebOverrides {
        let args = args.iter().map(|arg| (*arg).to_owned());
        match parse_web_options(args, "option", allow_disable).unwrap() {
            ParsedOptions::Options(overrides) => overrides,
            ParsedOptions::Help => panic!("expected options, got help"),
        }
    }

    #[test]
    fn the_dashboard_takes_the_same_web_options_the_subcommand_does() {
        let overrides = parsed(
            &[
                "--host",
                "0.0.0.0",
                "--port",
                "8080",
                "--auth",
                "alice:hunter2",
            ],
            true,
        );

        assert_eq!(overrides.host.as_deref(), Some("0.0.0.0"));
        assert_eq!(overrides.port, Some(8080));
        assert_eq!(overrides.auth.as_deref(), Some("alice:hunter2"));
        assert_eq!(overrides.enabled, None, "nothing said, nothing overridden");
    }

    /// `--no-web` means something on the dashboard and nothing on `agent-console web`,
    /// where the server *is* the command -- so it is rejected there rather than ignored.
    #[test]
    fn only_the_dashboard_accepts_no_web() {
        assert_eq!(parsed(&["--no-web"], true).enabled, Some(false));

        let rejected = parse_web_options(["--no-web".to_owned()].into_iter(), "web option", false)
            .unwrap_err();
        assert_eq!(rejected.kind(), io::ErrorKind::InvalidInput);
        assert!(rejected.to_string().contains("unknown web option"));
    }

    #[test]
    fn a_flag_missing_its_value_or_given_a_bad_one_is_reported() {
        for args in [vec!["--host"], vec!["--port"], vec!["--auth"]] {
            let error = parse_web_options(args.iter().map(|arg| (*arg).to_owned()), "option", true)
                .unwrap_err();
            assert!(
                error.to_string().contains("requires a value"),
                "{args:?} -> {error}"
            );
        }
        let error = parse_web_options(
            ["--port".to_owned(), "http".to_owned()].into_iter(),
            "option",
            true,
        )
        .unwrap_err();
        assert!(error.to_string().contains("invalid --port value: http"));
    }

    #[test]
    fn help_is_recognized_wherever_it_appears_in_the_options() {
        assert!(matches!(
            parse_web_options(
                ["--port".to_owned(), "1".to_owned(), "--help".to_owned()].into_iter(),
                "option",
                true,
            )
            .unwrap(),
            ParsedOptions::Help
        ));
    }

    /// The header line is the only place an auto-started server can announce itself: the
    /// terminal is in the alternate screen, so anything printed to stdout is already gone.
    #[test]
    fn the_header_web_line_carries_the_url_the_token_and_the_exposure() {
        assert_eq!(web_status_line(&WebStatus::Disabled), None);

        let serving = web_status_line(&WebStatus::Serving {
            url: "http://127.0.0.1:7878/?token=abc".into(),
            auth: "random token in the URL".into(),
            exposed: false,
        })
        .expect("a running server has to show its address");
        let text = serving
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("http://127.0.0.1:7878/?token=abc"));
        assert!(!text.contains("reachable from the network"));

        let exposed = web_status_line(&WebStatus::Serving {
            url: "http://0.0.0.0:7878/".into(),
            auth: "HTTP Basic auth as user \"alice\"".into(),
            exposed: true,
        })
        .expect("a running server has to show its address");
        let spans = exposed
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>();
        let warning = spans
            .iter()
            .position(|span| span.contains("reachable from the network"))
            .expect("a non-loopback bind has to be marked on screen");
        let credential = spans
            .iter()
            .position(|span| span.contains("alice"))
            .expect("the header names the credential");
        assert!(
            warning < credential,
            "the exposure warning goes first so a narrow terminal clips the credential instead"
        );

        let off = web_status_line(&WebStatus::Unavailable(
            "127.0.0.1:7878 is already in use".into(),
        ))
        .expect("a server that did not start has to say so");
        let text = off
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("off"));
        assert!(text.contains("already in use"));
    }
}
