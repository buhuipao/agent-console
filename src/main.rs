mod app;
mod clipboard;
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

use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

use app::{App, DialogField, TextDialogKind};
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
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("--version" | "-V") => {
            println!("agent-console {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("--help" | "-h") => {
            print!("{}", cli_help());
            Ok(())
        }
        Some("hook") => run_hook(args.next()),
        Some("doctor") => run_doctor(),
        Some("pty-daemon") => args
            .next()
            .map(PathBuf::from)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "pty-daemon requires a socket path",
                )
            })
            .and_then(|socket| pty::run_pty_daemon(&socket)),
        Some("pty-daemon-stop") => args
            .next()
            .map(PathBuf::from)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "pty-daemon-stop requires a socket path",
                )
            })
            .and_then(|socket| pty::stop_pty_daemon(&socket)),
        Some(other) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown command: {other}"),
        )),
        None => run_dashboard(),
    }
}

fn cli_help() -> &'static str {
    concat!(
        "Agent Console ",
        env!("CARGO_PKG_VERSION"),
        "\n\n",
        "Usage:\n",
        "  agent-console             Open the dashboard\n",
        "  agent-console doctor      Check providers and terminal prerequisites\n",
        "  agent-console --help      Show this help\n",
        "  agent-console --version   Show the version\n",
    )
}

fn run_hook(provider: Option<String>) -> io::Result<()> {
    use std::io::Read;

    let provider = match provider.as_deref() {
        Some("codex") => AgentKind::Codex,
        Some("claude") => AgentKind::Claude,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "hook provider must be codex or claude",
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

fn run_doctor() -> io::Result<()> {
    let config = config::AgentConsoleConfig::load()?;
    let discovery = discovery::DiscoveryPaths::from_environment();
    let mut providers = 0;
    let mut failures = 0;
    let enabled = crate::providers::enabled();
    println!(
        "ok   providers enabled: {}",
        enabled
            .iter()
            .map(|adapter| adapter.kind.label())
            .collect::<Vec<_>>()
            .join(", ")
    );
    for provider in enabled.iter().map(|adapter| adapter.kind) {
        let name = provider.label();
        match doctor::check_configured_provider(&config, provider) {
            doctor::ProviderStatus::Available(version) => {
                providers += 1;
                println!("ok   {name}: {version}");
                match doctor::version_support(provider, &version) {
                    doctor::VersionSupport::Supported => {
                        println!("ok   {name} version: supported")
                    }
                    doctor::VersionSupport::TooOld => {
                        println!("fail {name} version: below the supported minimum");
                        failures += 1;
                    }
                    doctor::VersionSupport::Unknown => {
                        println!("info {name} version: could not parse; compatibility unverified")
                    }
                }
                for capability in [
                    doctor::ProviderCapability::Resume,
                    doctor::ProviderCapability::Hooks,
                    doctor::ProviderCapability::Summary,
                ] {
                    let status = doctor::check_provider_capability(&config, provider, capability);
                    if print_doctor_status(&format!("{name} {}", capability.label()), status) {
                        failures += 1;
                    }
                }
            }
            doctor::ProviderStatus::Unavailable(error) => println!("info {name}: {error}"),
        }
    }
    if let Some(paths) = discovery {
        if crate::providers::is_enabled(AgentKind::Codex) {
            println!(
                "{} Codex sessions: {}",
                if paths.codex_sessions.is_dir() {
                    "ok  "
                } else {
                    "info"
                },
                paths.codex_sessions.display()
            );
        }
        if crate::providers::is_enabled(AgentKind::Claude) {
            println!(
                "{} Claude projects: {}",
                if paths.claude_projects.is_dir() {
                    "ok  "
                } else {
                    "info"
                },
                paths.claude_projects.display()
            );
        }
    }
    let state = store::state_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cannot resolve state directory"))?;
    if print_doctor_status("state permissions/SQLite", doctor::check_state(&state)) {
        failures += 1;
    }
    if print_doctor_status("hook ingress/index", doctor::check_hook_ingress(&state)) {
        failures += 1;
    }
    if print_doctor_status("clipboard", doctor::check_clipboard()) {
        failures += 1;
    }
    if print_doctor_status("PTY daemon", doctor::check_daemon(&state)) {
        failures += 1;
    }
    if let Some(path) = diagnostics::path() {
        println!("ok   rotating diagnostics: {}", path.display());
    }
    if providers == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "neither codex nor claude is available",
        ));
    }
    if failures > 0 {
        return Err(io::Error::other(format!(
            "doctor found {failures} required capability failure(s)"
        )));
    }
    Ok(())
}

fn print_doctor_status(label: &str, status: doctor::ProviderStatus) -> bool {
    match status {
        doctor::ProviderStatus::Available(detail) => {
            println!("ok   {label}: {detail}");
            false
        }
        doctor::ProviderStatus::Unavailable(detail) => {
            println!("fail {label}: {detail}");
            true
        }
    }
}

fn run_dashboard() -> io::Result<()> {
    let startup_cwd = env::current_dir()?;
    let mut app = App::load(startup_cwd)?;
    enable_raw_mode()?;
    let cleanup = TerminalCleanup;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = run(&mut terminal, &mut app);
    let restore = restore_terminal(&mut terminal);
    let shutdown = app.shutdown();
    drop(cleanup);
    result.and(restore).and(shutdown)
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

fn run(terminal: &mut DashboardTerminal, app: &mut App) -> io::Result<()> {
    let current_exe = env::current_exe()?;
    loop {
        terminal.draw(|frame| draw(frame, app))?;
        if event::poll(std::time::Duration::from_millis(100))? {
            let event = event::read()?;
            if let Event::Mouse(mouse) = event {
                handle_dashboard_mouse(app, mouse);
                app.tick();
                continue;
            }
            let Event::Key(key) = event else {
                app.tick();
                continue;
            };
            if key.kind != KeyEventKind::Press {
                app.tick();
                continue;
            }
            if app.dialog.is_some() {
                handle_dialog_key(app, key.code, &current_exe, terminal)?;
            } else if app.text_dialog.is_some() {
                handle_text_dialog_key(app, key.code);
            } else {
                let key_name = dashboard_key_name(key);
                if app.help_open {
                    if key.code == KeyCode::Esc || app.dashboard_action(&key_name) == Some("help") {
                        app.help_open = false;
                    }
                    app.tick();
                    continue;
                }
                if let Some(action) = app.dashboard_action(&key_name)
                    && handle_dashboard_action(app, action, &current_exe, terminal)
                {
                    return Ok(());
                }
            }
        }
        app.tick();
    }
}

fn handle_dashboard_action(
    app: &mut App,
    action: &str,
    current_exe: &std::path::Path,
    terminal: &mut DashboardTerminal,
) -> bool {
    match action {
        "quit" => return true,
        "next" => app.select_next(),
        "previous" => app.select_previous(),
        "enter" => attach_agent(app, current_exe, terminal),
        "takeover" => force_attach_agent(app, current_exe, terminal),
        "shell" => attach_shell(app, current_exe, terminal),
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
    false
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
    terminal: &mut DashboardTerminal,
) -> io::Result<()> {
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
        KeyCode::Left | KeyCode::Right | KeyCode::Char('h') | KeyCode::Char('l')
            if app
                .dialog
                .as_ref()
                .is_some_and(|dialog| dialog.field == DialogField::Provider) =>
        {
            if let Some(dialog) = &mut app.dialog {
                dialog.provider = match dialog.provider {
                    AgentKind::Codex => AgentKind::Claude,
                    AgentKind::Claude => AgentKind::Codex,
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
            Ok(()) => attach_agent(app, current_exe, terminal),
            Err(error) => {
                if let Some(dialog) = &mut app.dialog {
                    dialog.error = Some(error);
                }
            }
        },
        _ => {}
    }
    Ok(())
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

fn workspace_directory_completions(value: &str, home: Option<&Path>) -> Vec<String> {
    let (lookup, tilde) = if let Some(rest) = value.strip_prefix("~/") {
        let Some(home) = home else {
            return Vec::new();
        };
        (home.join(rest), Some(home))
    } else {
        (PathBuf::from(value), None)
    };
    let ends_with_separator = value.ends_with(std::path::MAIN_SEPARATOR);
    let (parent, prefix) = if ends_with_separator {
        (lookup.as_path(), "")
    } else {
        (
            lookup.parent().unwrap_or_else(|| Path::new(".")),
            lookup
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(""),
        )
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut matches = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            name.starts_with(prefix).then(|| {
                let path = parent.join(name);
                let display = tilde.map_or_else(
                    || path.display().to_string(),
                    |home| {
                        path.strip_prefix(home).map_or_else(
                            |_| path.display().to_string(),
                            |rest| format!("~/{}", rest.display()),
                        )
                    },
                );
                format!("{display}{}", std::path::MAIN_SEPARATOR)
            })
        })
        .collect::<Vec<_>>();
    matches.sort_by_key(|value| value.to_lowercase());
    matches
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

fn attach_agent(app: &mut App, current_exe: &std::path::Path, terminal: &mut DashboardTerminal) {
    if let Err(error) = app.enter_selected_agent(current_exe) {
        app.banner = Some(format!("cannot enter agent: {error}"));
    }
    let _ = execute!(
        terminal.backend_mut(),
        EnableMouseCapture,
        Clear(ClearType::All)
    );
    terminal.swap_buffers();
}

fn force_attach_agent(
    app: &mut App,
    current_exe: &std::path::Path,
    terminal: &mut DashboardTerminal,
) {
    if let Err(error) = app.force_enter_selected_agent(current_exe) {
        app.banner = Some(format!("cannot take over agent: {error}"));
    }
    let _ = execute!(
        terminal.backend_mut(),
        EnableMouseCapture,
        Clear(ClearType::All)
    );
    terminal.swap_buffers();
}

fn attach_shell(app: &mut App, current_exe: &std::path::Path, terminal: &mut DashboardTerminal) {
    if let Err(error) = app.enter_selected_shell(current_exe) {
        app.banner = Some(format!("cannot enter shell: {error}"));
    }
    let _ = execute!(
        terminal.backend_mut(),
        EnableMouseCapture,
        Clear(ClearType::All)
    );
    terminal.swap_buffers();
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
    frame.render_widget(
        Paragraph::new(Line::from(spans)).block(Block::default().borders(Borders::BOTTOM)),
        area,
    );
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
                format!("{} ", session.agent.short_label()),
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
    fn workspace_completion_lists_only_matching_directories_and_preserves_tilde() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("alpha-one")).unwrap();
        fs::create_dir_all(root.path().join("alpha-two")).unwrap();
        fs::write(root.path().join("alpha-file"), "not a directory").unwrap();

        let absolute = format!("{}/alpha", root.path().display());
        assert_eq!(
            workspace_directory_completions(&absolute, Some(root.path())),
            vec![
                format!("{}/alpha-one/", root.path().display()),
                format!("{}/alpha-two/", root.path().display()),
            ]
        );
        assert_eq!(
            workspace_directory_completions("~/alpha", Some(root.path())),
            vec!["~/alpha-one/", "~/alpha-two/"]
        );
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
}
