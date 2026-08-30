use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs, io,
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};

use uuid::Uuid;

use crate::{
    clipboard,
    config::AgentConsoleConfig,
    diagnostics,
    discovery::{self, DiscoveryCache, DiscoveryPaths},
    events::{self, NormalizedEvent},
    model::{AgentKind, Session, SessionStatus, SessionSummary, unix_timestamp},
    pty::{
        TerminalManager, WorkspaceChrome, WorkspaceExit, WorkspaceFocus, WorkspaceInputOutcome,
        WorkspaceSearchUpdate, WorkspaceSession, bracketed_paste, staged_shell_text,
    },
    store::StateStore,
    summary::{SummaryBackend, SummaryJob, SummaryWorker},
};

const DISCOVERY_INTERVAL: Duration = Duration::from_secs(2);
const EVENT_REFRESH_INTERVAL: Duration = Duration::from_millis(500);
const SUMMARY_DEBOUNCE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy)]
struct SummaryPolicy {
    min_interval: Duration,
    failure_backoff: Duration,
    circuit_failures: u32,
    circuit_cooldown: Duration,
}

impl From<&AgentConsoleConfig> for SummaryPolicy {
    fn from(config: &AgentConsoleConfig) -> Self {
        Self {
            min_interval: Duration::from_secs(config.summary.min_interval_seconds),
            failure_backoff: Duration::from_secs(config.summary.failure_backoff_seconds),
            circuit_failures: config.summary.circuit_failures,
            circuit_cooldown: Duration::from_secs(config.summary.circuit_cooldown_seconds),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DialogField {
    Provider,
    Cwd,
}

#[derive(Clone, Debug)]
pub struct NewSessionDialog {
    pub provider: AgentKind,
    pub cwd: String,
    pub cwd_cursor: usize,
    pub cwd_replace_on_input: bool,
    pub cwd_completion_index: usize,
    pub cwd_completion_accepted: bool,
    pub field: DialogField,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextDialogKind {
    Search,
    Alias,
}

#[derive(Clone, Debug)]
pub struct TextDialog {
    pub kind: TextDialogKind,
    pub value: String,
    original_value: String,
    original_selected: usize,
}

#[derive(Clone, Debug, Default)]
struct SessionFilter {
    query: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeNotification {
    /// Stable across the whole life of the entry, and unique even after a restart, so a web
    /// client can remember which alerts *it* has seen without a per-process counter that
    /// would restart at zero and silently suppress fresh alerts.
    pub id: String,
    pub session_key: String,
    pub status: SessionStatus,
    pub message: String,
    /// Unix seconds, so several clients can order and de-duplicate the same queue.
    pub created_at: u64,
    read: bool,
}

impl RuntimeNotification {
    /// The shared, TUI-facing read flag. It is deliberately not settable from outside this
    /// module: only entering a session, leaving a critical status, or an explicit
    /// `mark_notification_read` clears it.
    pub fn is_read(&self) -> bool {
        self.read
    }
}

struct DiscoveryWorker {
    requests: SyncSender<DiscoveryPaths>,
    results: Receiver<Vec<Session>>,
    in_flight: bool,
    stopped: bool,
}

impl DiscoveryWorker {
    fn start(cache: DiscoveryCache) -> io::Result<Self> {
        Self::start_with_runner(cache, discovery::discover_cached)
    }

    fn start_with_runner<F>(mut cache: DiscoveryCache, runner: F) -> io::Result<Self>
    where
        F: Fn(&DiscoveryPaths, &mut DiscoveryCache) -> Vec<Session> + Send + 'static,
    {
        let (request_tx, request_rx) = mpsc::sync_channel::<DiscoveryPaths>(1);
        let (result_tx, result_rx) = mpsc::channel();
        thread::Builder::new()
            .name("agent-console-discovery".into())
            .spawn(move || {
                while let Ok(paths) = request_rx.recv() {
                    let sessions = runner(&paths, &mut cache);
                    if result_tx.send(sessions).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            requests: request_tx,
            results: result_rx,
            in_flight: false,
            stopped: false,
        })
    }

    fn request(&mut self, paths: DiscoveryPaths) -> Result<bool, &'static str> {
        if self.in_flight || self.stopped {
            return Ok(false);
        }
        match self.requests.try_send(paths) {
            Ok(()) => {
                self.in_flight = true;
                Ok(true)
            }
            Err(TrySendError::Full(_)) => Ok(false),
            Err(TrySendError::Disconnected(_)) => {
                self.stopped = true;
                Err("session discovery worker stopped accepting refresh requests")
            }
        }
    }

    fn poll(&mut self) -> Result<Option<Vec<Session>>, &'static str> {
        if self.stopped {
            return Ok(None);
        }
        match self.results.try_recv() {
            Ok(sessions) => {
                self.in_flight = false;
                Ok(Some(sessions))
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                self.in_flight = false;
                self.stopped = true;
                Err("session discovery worker stopped unexpectedly")
            }
        }
    }

    #[cfg(test)]
    fn disconnected() -> Self {
        let (requests, request_rx) = mpsc::sync_channel(1);
        drop(request_rx);
        let (result_tx, results) = mpsc::channel();
        drop(result_tx);
        Self {
            requests,
            results,
            in_flight: false,
            stopped: false,
        }
    }
}

pub struct RuntimeState {
    pub sessions: Vec<Session>,
    pub selected: usize,
    pub tick_count: u64,
    pub banner: Option<String>,
    pub summary_busy: Option<String>,
    discovery_paths: DiscoveryPaths,
    discovery_worker: DiscoveryWorker,
    last_discovery: Instant,
    last_event_refresh: Instant,
    changed_at: HashMap<String, Instant>,
    observed_fingerprints: HashMap<String, String>,
    summary_queue: VecDeque<String>,
    summary_queued: HashSet<String>,
    last_summary_attempt: HashMap<String, Instant>,
    summary_retry_at: HashMap<String, Instant>,
    summary_failures: HashMap<String, u32>,
    provider_failures: HashMap<AgentKind, u32>,
    provider_circuit_until: HashMap<AgentKind, Instant>,
    summary_policy: SummaryPolicy,
    observed_statuses: HashMap<String, SessionStatus>,
    notifications: VecDeque<RuntimeNotification>,
    /// Whether an alert is dropped for the session the dashboard already has selected.
    /// True for the TUI, where that session is on screen and an alert about it is noise.
    /// The web server turns it off: it has many clients and no single selected session, so
    /// the suppression would only lose alerts nobody ever saw.
    suppress_selected_notifications: bool,
    filter: SessionFilter,
    store: StateStore,
    event_index: events::EventIndex,
    summary_worker: SummaryWorker,
}

/// What the dashboard says about the web server running beside it.
///
/// The dashboard owns the terminal, so a server that started (or failed to) has nowhere else
/// to say so: anything printed to stdout is hidden the moment the alternate screen opens.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum WebStatus {
    /// Not started, because it was switched off with `--no-web` or `[web] enabled = false`.
    #[default]
    Disabled,
    Serving {
        /// The address to open, carrying the token when the server runs in token mode --
        /// without it an auto-started server would be unusable.
        url: String,
        /// Which credential it asks for. Never contains a password.
        auth: String,
        /// Whether the bind is reachable from beyond this machine.
        exposed: bool,
    },
    /// It could not start. The string is the reason, e.g. a port already in use.
    Unavailable(String),
}

/// One open workspace, from the dashboard's point of view.
///
/// It owns what the workspace loop used to keep on the stack: the session and focus the
/// current attach is on, the attach itself, and the bookkeeping that has to outlive a single
/// attach. Keeping it out of `App` is the point -- the caller holds this while the `App` lock
/// it needs for each frame is repeatedly taken and dropped.
pub struct WorkspaceDrive {
    session: Session,
    focus: WorkspaceFocus,
    force_takeover: bool,
    touched_agents: HashSet<String>,
    /// The agent keys that were alive when the current attach opened, which is what lets a
    /// tick tell a rekeyed session from a new one.
    alive: HashSet<String>,
    pending_rekeys: Vec<(String, String)>,
    attached: Option<WorkspaceSession>,
}

impl WorkspaceDrive {
    /// Applies polled input against this session's terminals, with the `App` unlocked.
    pub fn apply_input(&mut self) -> io::Result<WorkspaceInputOutcome> {
        match &mut self.attached {
            Some(attached) => attached.apply_input(&self.session),
            None => Ok(WorkspaceInputOutcome::default()),
        }
    }

    /// Repaints, with the `App` unlocked. `chrome` came from [`App::workspace_chrome`].
    pub fn render(&mut self, chrome: WorkspaceChrome) -> io::Result<Option<WorkspaceExit>> {
        match &mut self.attached {
            Some(attached) => attached.render(chrome),
            None => Ok(Some(WorkspaceExit::Dashboard)),
        }
    }

    /// Waits for the next workspace input with the `App` unlocked. See
    /// [`crate::pty::WorkspaceSession::wait`].
    pub fn wait(&mut self, timeout: Duration) -> io::Result<()> {
        match &mut self.attached {
            Some(attached) => attached.wait(timeout),
            None => Ok(()),
        }
    }
}

/// What a workspace frame calls to refresh its session list, preview and notifications.
///
/// Discovery can rename a session under an open workspace; the rename is recorded here and
/// applied once the attach closes, because the attach itself is addressed by the old key.
fn observe_workspace(
    runtime: &mut RuntimeState,
    alive: &mut HashSet<String>,
    pending_rekeys: &mut Vec<(String, String)>,
    search_update: Option<WorkspaceSearchUpdate>,
) -> WorkspaceChrome {
    if let Some(search_update) = search_update {
        runtime.apply_workspace_search(search_update);
    }
    for (old_key, new_key) in runtime.tick(alive) {
        if alive.remove(&old_key) {
            alive.insert(new_key.clone());
        }
        pending_rekeys.push((old_key, new_key));
    }
    runtime.workspace_chrome()
}

pub struct App {
    runtime: RuntimeState,
    pub dialog: Option<NewSessionDialog>,
    pub text_dialog: Option<TextDialog>,
    pub help_open: bool,
    startup_cwd: PathBuf,
    pub terminals: TerminalManager,
    config: AgentConsoleConfig,
    web_status: WebStatus,
}

impl Deref for App {
    type Target = RuntimeState;

    fn deref(&self) -> &Self::Target {
        &self.runtime
    }
}

impl DerefMut for App {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.runtime
    }
}

impl App {
    pub fn load(startup_cwd: PathBuf) -> io::Result<Self> {
        let config = AgentConsoleConfig::load()?;
        let discovery_paths = DiscoveryPaths::from_environment().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "cannot find the home directory")
        })?;
        let (store, warning) = StateStore::from_environment()?;
        let mut event_index = events::EventIndex::open(&store.root)?;
        let backend = SummaryBackend::from_environment();
        let summary_worker = SummaryWorker::start(
            backend,
            store.root.clone(),
            store.schema_path(),
            config.clone(),
        )?;
        let mut discovery_cache = DiscoveryCache::default();
        let mut sessions = discovery::discover_cached(&discovery_paths, &mut discovery_cache);
        let discovery_worker = DiscoveryWorker::start(discovery_cache)?;
        for session in &mut sessions {
            store.apply(session);
            apply_event_inbox(&mut event_index, &store.events_dir(), session);
        }
        let summary_policy = SummaryPolicy::from(&config);
        let mut summary_queue = VecDeque::new();
        let mut summary_queued = HashSet::new();
        if let Some(selected) = sessions.first()
            && selected.summary_fingerprint != selected.transcript_fingerprint
        {
            summary_queue.push_back(selected.key.clone());
            summary_queued.insert(selected.key.clone());
        }
        let mut app = Self {
            runtime: RuntimeState {
                sessions,
                selected: 0,
                tick_count: 0,
                banner: warning,
                summary_busy: None,
                discovery_paths,
                discovery_worker,
                last_discovery: Instant::now(),
                last_event_refresh: Instant::now(),
                changed_at: HashMap::new(),
                observed_fingerprints: HashMap::new(),
                summary_queue,
                summary_queued,
                last_summary_attempt: HashMap::new(),
                summary_retry_at: HashMap::new(),
                summary_failures: HashMap::new(),
                provider_failures: HashMap::new(),
                provider_circuit_until: HashMap::new(),
                summary_policy,
                observed_statuses: HashMap::new(),
                notifications: VecDeque::new(),
                suppress_selected_notifications: true,
                filter: SessionFilter::default(),
                store,
                event_index,
                summary_worker,
            },
            dialog: None,
            text_dialog: None,
            help_open: false,
            startup_cwd,
            terminals: TerminalManager::new(config.clone()),
            config,
            web_status: WebStatus::Disabled,
        };
        app.normalize_selection();
        app.observe_changes();
        app.capture_notifications();
        Ok(app)
    }

    pub fn selected_session(&self) -> Option<&Session> {
        self.runtime
            .session_display_order()
            .contains(&self.selected)
            .then(|| self.sessions.get(self.selected))
            .flatten()
    }

    pub fn select_next(&mut self) {
        let order = self.session_display_order();
        if order.is_empty() {
            return;
        }
        let position = order
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0);
        self.selected = order[(position + 1) % order.len()];
        self.queue_selected_if_needed();
    }

    pub fn select_previous(&mut self) {
        let order = self.session_display_order();
        if order.is_empty() {
            return;
        }
        let position = order
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0);
        self.selected = order[position.checked_sub(1).unwrap_or(order.len() - 1)];
        self.queue_selected_if_needed();
    }

    pub fn session_display_order(&self) -> Vec<usize> {
        self.runtime.session_display_order()
    }

    pub fn status_counts(&self) -> (usize, usize, usize, usize) {
        session_status_counts(&self.sessions)
    }

    pub fn open_new_dialog(&mut self) {
        let cwd = self.startup_cwd.clone();
        self.open_new_dialog_at(&cwd);
    }

    fn open_new_dialog_at(&mut self, cwd: &Path) {
        let provider = AgentKind::Codex;
        let cwd = cwd.display().to_string();
        let cwd_cursor = cwd.chars().count();
        self.dialog = Some(NewSessionDialog {
            provider,
            cwd,
            cwd_cursor,
            cwd_replace_on_input: true,
            cwd_completion_index: 0,
            cwd_completion_accepted: false,
            field: DialogField::Provider,
            error: None,
        });
    }

    pub fn cancel_dialog(&mut self) {
        self.dialog = None;
    }

    pub fn dashboard_action(&self, key: &str) -> Option<&'static str> {
        self.config.dashboard_action(key)
    }

    pub fn dashboard_key_label(&self, action: &str) -> String {
        self.config
            .dashboard_keys(action)
            .first()
            .map(|key| crate::config::format_key_label(key))
            .unwrap_or_else(|| "unbound".into())
    }

    /// The key bindings, with the web server's address spliced into the dashboard column.
    ///
    /// Splicing rather than appending: the help panel splits these lines into columns at the
    /// `WORKSPACE · ...` headings, so anything added after them lands in the wrong column and
    /// anything added before the first heading breaks the split.
    pub fn help_lines(&self) -> Vec<String> {
        let mut lines = self.config.help_bindings();
        let web = match &self.web_status {
            WebStatus::Disabled => return lines,
            WebStatus::Serving { url, auth, exposed } => {
                let mut web = vec!["WEB UI".to_owned(), short_url(url)];
                web.push(auth.clone());
                if *exposed {
                    web.push("reachable from the network".to_owned());
                }
                web
            }
            WebStatus::Unavailable(reason) => vec!["WEB UI".to_owned(), format!("off: {reason}")],
        };
        let at = lines
            .iter()
            .position(|line| line.starts_with("WORKSPACE"))
            .unwrap_or(lines.len());
        lines.splice(at..at, web);
        lines
    }

    pub fn set_web_status(&mut self, status: WebStatus) {
        self.web_status = status;
    }

    pub fn web_status(&self) -> &WebStatus {
        &self.web_status
    }

    pub fn open_search_dialog(&mut self) {
        let value = self.runtime.filter.query.clone();
        self.text_dialog = Some(TextDialog {
            kind: TextDialogKind::Search,
            original_value: value.clone(),
            original_selected: self.selected,
            value,
        });
    }

    pub fn open_alias_dialog(&mut self) {
        let Some(session) = self.selected_session() else {
            self.banner = Some("no selected session".into());
            return;
        };
        let value = self
            .runtime
            .store
            .alias(&session.key)
            .unwrap_or_default()
            .to_owned();
        self.text_dialog = Some(TextDialog {
            kind: TextDialogKind::Alias,
            original_value: value.clone(),
            original_selected: self.selected,
            value,
        });
    }

    pub fn push_text_dialog_character(&mut self, character: char) {
        if let Some(dialog) = &mut self.text_dialog {
            dialog.value.push(character);
        }
        self.preview_search_dialog();
    }

    pub fn pop_text_dialog_character(&mut self) {
        if let Some(dialog) = &mut self.text_dialog {
            dialog.value.pop();
        }
        self.preview_search_dialog();
    }

    fn preview_search_dialog(&mut self) {
        let Some(dialog) = self
            .text_dialog
            .as_ref()
            .filter(|dialog| dialog.kind == TextDialogKind::Search)
        else {
            return;
        };
        self.runtime.filter.query = dialog.value.trim().to_lowercase();
        self.normalize_selection();
    }

    pub fn commit_text_dialog(&mut self) -> Result<(), String> {
        let dialog = self
            .text_dialog
            .take()
            .ok_or_else(|| "no text dialog".to_owned())?;
        match dialog.kind {
            TextDialogKind::Search => {
                self.runtime.filter.query = dialog.value.trim().to_lowercase();
                self.normalize_selection();
                self.banner = Some(self.filter_summary());
            }
            TextDialogKind::Alias => {
                let key = self
                    .selected_session()
                    .map(|session| session.key.clone())
                    .ok_or_else(|| "no selected session".to_owned())?;
                let value = dialog.value.trim();
                self.runtime
                    .store
                    .set_alias(&key, (!value.is_empty()).then(|| value.to_owned()));
                self.runtime
                    .store
                    .save_incremental()
                    .map_err(|error| error.to_string())?;
                self.banner = Some(if value.is_empty() {
                    "ALIAS CLEARED · session display falls back to summary".into()
                } else {
                    "ALIAS SAVED · user title takes precedence over summaries".into()
                });
            }
        }
        Ok(())
    }

    pub fn cancel_text_dialog(&mut self) {
        let Some(dialog) = self.text_dialog.take() else {
            return;
        };
        if dialog.kind == TextDialogKind::Search {
            self.runtime.filter.query = dialog.original_value.trim().to_lowercase();
            self.selected = dialog.original_selected;
            self.normalize_selection();
        }
    }

    pub fn toggle_selected_archive(&mut self) -> Result<bool, String> {
        let key = self
            .selected_session()
            .map(|session| session.key.clone())
            .ok_or_else(|| "no selected session".to_owned())?;
        let archived = self.runtime.store.toggle_archived(&key);
        self.runtime
            .store
            .save_incremental()
            .map_err(|error| error.to_string())?;
        self.normalize_selection();
        self.banner = Some(if archived {
            "SESSION ARCHIVED · moved to the Archived group".into()
        } else {
            "SESSION RESTORED · returned to its workspace group".into()
        });
        Ok(archived)
    }

    pub fn session_title(&self, session: &Session) -> String {
        self.runtime
            .store
            .alias(&session.key)
            .map(str::to_owned)
            .unwrap_or_else(|| session.list_title())
    }

    pub fn session_archived(&self, session: &Session) -> bool {
        self.runtime.store.archived(&session.key)
    }

    /// The alias a session was renamed to, if any.
    pub fn session_alias(&self, key: &str) -> Option<&str> {
        self.runtime.store.alias(key)
    }

    /// Sets or clears a session's alias, exactly as the TUI's rename dialog does in
    /// `commit_text_dialog`: the value is trimmed, an empty value clears the alias, and the
    /// change is written through `StateStore` immediately so a rename made in the browser is
    /// the title the TUI shows too.
    ///
    /// Keyed rather than selection-based, so renaming from one client cannot move the
    /// selection other clients are reading.
    pub fn set_session_alias(&mut self, key: &str, alias: Option<&str>) -> Result<(), String> {
        if !self.sessions.iter().any(|session| session.key == key) {
            return Err(format!("no session with key {key}"));
        }
        let alias = alias
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        self.runtime.store.set_alias(key, alias);
        self.runtime
            .store
            .save_incremental()
            .map_err(|error| error.to_string())
    }

    pub fn session_shell_count(&self, session: &Session) -> usize {
        self.terminals.shell_count(&session.key)
    }

    pub fn filter_summary(&self) -> String {
        self.runtime.filter_summary()
    }

    fn normalize_selection(&mut self) {
        self.runtime.normalize_selection();
    }

    pub fn create_from_dialog(&mut self) -> Result<(), String> {
        let dialog = self
            .dialog
            .as_ref()
            .cloned()
            .ok_or_else(|| "no dialog".to_owned())?;
        let cwd = expand_tilde(dialog.cwd.trim());
        if !cwd.is_dir() {
            return Err(format!("{} is not a directory", cwd.display()));
        }
        let id = Uuid::new_v4().to_string();
        let key = Session::stable_key(dialog.provider, &id);
        let name = cwd
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("session")
            .to_owned();
        self.sessions.insert(
            0,
            Session {
                key,
                provider_session_id: id,
                name,
                search_terms: Vec::new(),
                first_prompt: None,
                agent: dialog.provider,
                status: SessionStatus::Idle,
                cwd,
                branch: None,
                transcript_path: None,
                transcript_modified_at: unix_timestamp(),
                transcript_fingerprint: "new".into(),
                summary_fingerprint: String::new(),
                summary_updated_at: None,
                summary_error: None,
                summary: SessionSummary::default(),
                recent_activity: vec!["New managed session".into()],
                pending_decisions: Vec::new(),
                pending_shell_injection: None,
                managed_alive: false,
                unavailable_reason: None,
                discovered_after_startup: true,
            },
        );
        self.selected = 0;
        self.dialog = None;
        Ok(())
    }

    pub fn enter_selected_agent(&mut self, current_exe: &Path) -> io::Result<WorkspaceDrive> {
        self.enter_selected_agent_with_takeover(current_exe, false)
    }

    pub fn force_enter_selected_agent(&mut self, current_exe: &Path) -> io::Result<WorkspaceDrive> {
        self.enter_selected_agent_with_takeover(current_exe, true)
    }

    fn enter_selected_agent_with_takeover(
        &mut self,
        current_exe: &Path,
        force_takeover: bool,
    ) -> io::Result<WorkspaceDrive> {
        let session = self.prepare_agent_entry(current_exe)?;
        self.open_workspace(session, WorkspaceFocus::Agent, force_takeover)
    }

    fn prepare_agent_entry(&mut self, current_exe: &Path) -> io::Result<Session> {
        self.activate_workspace_agent(current_exe)
    }

    pub fn enter_selected_shell(&mut self, current_exe: &Path) -> io::Result<WorkspaceDrive> {
        let session = self.prepare_selected_view(current_exe)?;
        if !session.cwd.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "session working directory does not exist",
            ));
        }
        let size = crossterm::terminal::size().unwrap_or((80, 24));
        self.terminals.add_shell(&session, size)?;
        self.open_workspace(session, WorkspaceFocus::Shell, false)
    }

    /// Opens a workspace and hands it back instead of running it here.
    ///
    /// The caller steps it with [`App::step_workspace`], which is what lets the lock it took
    /// to reach this `App` be dropped between frames rather than held for the whole time a
    /// session is open.
    fn open_workspace(
        &mut self,
        session: Session,
        focus: WorkspaceFocus,
        force_takeover: bool,
    ) -> io::Result<WorkspaceDrive> {
        let mut touched_agents = HashSet::new();
        if focus == WorkspaceFocus::Agent {
            touched_agents.insert(session.key.clone());
        }
        let mut drive = WorkspaceDrive {
            session,
            focus,
            force_takeover,
            touched_agents,
            alive: HashSet::new(),
            pending_rekeys: Vec::new(),
            attached: None,
        };
        self.attach_workspace(&mut drive)?;
        Ok(drive)
    }

    /// Applies one workspace exit, reporting whether the workspace as a whole is finished.
    ///
    /// This is the body of the loop that used to wrap the attach: every arm either chooses the
    /// next session and focus to attach, or leaves for the dashboard.
    fn advance_workspace(
        &mut self,
        drive: &mut WorkspaceDrive,
        exit: WorkspaceExit,
        current_exe: &Path,
    ) -> io::Result<bool> {
        match exit {
            WorkspaceExit::Dashboard => return Ok(true),
            WorkspaceExit::Alert => {
                self.runtime.jump_to_next_notification();
                return Ok(true);
            }
            WorkspaceExit::ActivateSession => match self.activate_workspace_agent(current_exe) {
                Ok(selected) => {
                    drive.session = selected;
                    drive.focus = WorkspaceFocus::Agent;
                    drive.touched_agents.insert(drive.session.key.clone());
                }
                Err(error) => {
                    drive.session = self.prepare_selected_view(current_exe)?;
                    self.terminals
                        .set_notice(&drive.session.key, format!("cannot open agent: {error}"));
                    drive.focus = WorkspaceFocus::Sessions;
                }
            },
            WorkspaceExit::FocusShell => {
                drive.session = self.prepare_selected_view(current_exe)?;
                drive.focus = WorkspaceFocus::Shell;
            }
            WorkspaceExit::NewSession => {
                self.open_new_dialog_at(&drive.session.cwd);
                return Ok(true);
            }
            WorkspaceExit::OpenShell => {
                drive.session = self.prepare_selected_view(current_exe)?;
                if !drive.session.cwd.is_dir() {
                    self.terminals.set_notice(
                        &drive.session.key,
                        "cannot open shell: working directory no longer exists".into(),
                    );
                    drive.focus = WorkspaceFocus::Sessions;
                } else {
                    let size = crossterm::terminal::size().unwrap_or((80, 24));
                    match self.terminals.add_shell(&drive.session, size) {
                        Ok(_) => drive.focus = WorkspaceFocus::Shell,
                        Err(error) => {
                            self.terminals.set_notice(
                                &drive.session.key,
                                format!("cannot open shell: {error}"),
                            );
                            drive.focus = WorkspaceFocus::Sessions;
                        }
                    }
                }
            }
            WorkspaceExit::ToggleArchive => {
                let notice = match self.toggle_selected_archive() {
                    Ok(true) => "SESSION ARCHIVED · moved to the Archived group".into(),
                    Ok(false) => "SESSION RESTORED · returned to its workspace group".into(),
                    Err(error) => format!("cannot change archive state: {error}"),
                };
                drive.session = self.prepare_selected_view(current_exe)?;
                self.terminals.set_notice(&drive.session.key, notice);
                drive.focus = WorkspaceFocus::Sessions;
            }
            WorkspaceExit::RefreshSessions => {
                if self.selected_session().is_some() {
                    drive.session = self.prepare_selected_view(current_exe)?;
                }
                drive.focus = WorkspaceFocus::Sessions;
            }
            WorkspaceExit::PreviousSession(next_focus) => {
                self.select_previous();
                drive.focus = next_focus;
                drive.session = self.prepare_workspace_target(current_exe, next_focus)?;
                if next_focus == WorkspaceFocus::Agent
                    && self.terminals.agent_alive(&drive.session.key)
                {
                    drive.touched_agents.insert(drive.session.key.clone());
                }
            }
            WorkspaceExit::NextSession(next_focus) => {
                self.select_next();
                drive.focus = next_focus;
                drive.session = self.prepare_workspace_target(current_exe, next_focus)?;
                if next_focus == WorkspaceFocus::Agent
                    && self.terminals.agent_alive(&drive.session.key)
                {
                    drive.touched_agents.insert(drive.session.key.clone());
                }
            }
        }
        drive.force_takeover = false;
        Ok(false)
    }

    fn prepare_workspace_target(
        &mut self,
        current_exe: &Path,
        focus: WorkspaceFocus,
    ) -> io::Result<Session> {
        if focus == WorkspaceFocus::Sessions {
            return self.prepare_selected_view(current_exe);
        }
        match self.activate_workspace_agent(current_exe) {
            Ok(session) => Ok(session),
            Err(error) => {
                let session = self.prepare_selected_view(current_exe)?;
                self.terminals
                    .set_notice(&session.key, format!("cannot open agent: {error}"));
                Ok(session)
            }
        }
    }

    /// Opens the next attach of an already-running workspace.
    fn attach_workspace(&mut self, drive: &mut WorkspaceDrive) -> io::Result<()> {
        drive.alive = self.terminals.alive_keys().into_iter().collect();
        drive.pending_rekeys.clear();
        let chrome = self.workspace_frame_chrome(drive, None);
        drive.attached = Some(self.terminals.begin_workspace(
            &drive.session,
            drive.focus,
            drive.force_takeover,
            chrome,
        )?);
        Ok(())
    }

    /// The `App`-lock half of a workspace frame, and the only part of one that needs this
    /// lock at all: a tick of the shared runtime, plus the session list, preview and alerts
    /// the frame is about to draw.
    ///
    /// It is a snapshot on purpose. Everything else a frame does -- polling the daemon,
    /// parsing output, writing a screen -- then runs against that snapshot with this lock
    /// released, which is what keeps the web API answering while a busy agent repaints.
    pub fn workspace_frame_chrome(
        &mut self,
        drive: &mut WorkspaceDrive,
        search: Option<WorkspaceSearchUpdate>,
    ) -> WorkspaceChrome {
        observe_workspace(
            &mut self.runtime,
            &mut drive.alive,
            &mut drive.pending_rekeys,
            search,
        )
    }

    /// Applies a workspace exit: closes the current attach, moves to whatever the exit asked
    /// for, and opens the next one. `true` means the workspace as a whole is finished.
    pub fn advance_workspace_attach(
        &mut self,
        drive: &mut WorkspaceDrive,
        exit: WorkspaceExit,
        current_exe: &Path,
    ) -> io::Result<bool> {
        self.close_attach(drive)?;
        if self.advance_workspace(drive, exit, current_exe)? {
            self.finish_workspace(&drive.touched_agents)?;
            return Ok(true);
        }
        self.attach_workspace(drive)?;
        Ok(false)
    }

    /// Ends the current attach: restores the terminal, drops the input lease, and applies the
    /// session rekeys discovery reported while it was up.
    ///
    /// Rekeying waits until here rather than happening as it is discovered because the open
    /// attach is looked up by session key every frame; moving that key out from under it
    /// mid-attach would lose the terminal.
    fn close_attach(&mut self, drive: &mut WorkspaceDrive) -> io::Result<()> {
        let finished = drive
            .attached
            .take()
            .map_or(Ok(()), WorkspaceSession::finish);
        for (old_key, new_key) in std::mem::take(&mut drive.pending_rekeys) {
            self.terminals.rekey(&old_key, new_key);
        }
        finished
    }

    /// Tears down a workspace whose step reported an error, so neither the terminal nor the
    /// input lease is left in workspace mode. The caller reports the original error.
    pub fn abandon_workspace(&mut self, mut drive: WorkspaceDrive) -> io::Result<()> {
        self.close_attach(&mut drive)
    }

    fn prepare_selected_agent(&mut self, current_exe: &Path) -> io::Result<Session> {
        let session = self
            .selected_session()
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no selected session"))?;
        if let Some(reason) = &session.unavailable_reason {
            return Err(io::Error::new(io::ErrorKind::NotFound, reason.clone()));
        }
        let size = crossterm::terminal::size().unwrap_or((80, 24));
        self.terminals
            .ensure_session_view(&session, current_exe, size)?;
        let agent_alive = self.terminals.agent_alive(&session.key);
        let managed_fingerprint = self
            .runtime
            .store
            .managed_transcript_fingerprint(&session.key);
        match managed_agent_refresh(&session, managed_fingerprint, agent_alive) {
            ManagedAgentRefresh::Restart => {
                self.terminals.terminate_agent(&session.key);
                self.terminals.set_notice(
                    &session.key,
                    "restarted idle agent because its transcript was updated externally".into(),
                );
            }
            ManagedAgentRefresh::Conflict => {
                return Err(io::Error::other(format!(
                    "{} agent is {} and its transcript changed externally; return after it becomes idle or force takeover",
                    session.agent.label(),
                    session.status.label()
                )));
            }
            ManagedAgentRefresh::Reuse => {}
        }
        let new_session = session.transcript_path.is_none();
        let terminal = self
            .terminals
            .ensure_agent(&session, current_exe, new_session, size)?;
        terminal.wait_for_first_output(Duration::from_secs(2));
        if let Some(staged) = &session.pending_shell_injection {
            terminal.write(&bracketed_paste(staged))?;
            let selected_index = self.runtime.selected;
            if let Some(selected) = self.runtime.sessions.get_mut(selected_index) {
                selected.pending_shell_injection = None;
                self.runtime.store.update(selected);
                self.runtime.store.save_incremental()?;
            }
        }
        self.runtime.store.set_managed_transcript_fingerprint(
            &session.key,
            Some(session.transcript_fingerprint.clone()),
        );
        self.runtime.store.save_incremental()?;
        Ok(session)
    }

    fn activate_workspace_agent(&mut self, current_exe: &Path) -> io::Result<Session> {
        let session = self.prepare_selected_view(current_exe)?;
        let session = if self.terminals.agent_alive(&session.key) {
            session
        } else {
            self.prepare_selected_agent(current_exe)?
        };
        self.runtime.mark_notifications_read(&session.key);
        Ok(session)
    }

    fn prepare_selected_view(&mut self, current_exe: &Path) -> io::Result<Session> {
        let session = self
            .selected_session()
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no selected session"))?;
        let size = crossterm::terminal::size().unwrap_or((80, 24));
        self.terminals
            .ensure_session_view(&session, current_exe, size)?;
        Ok(session)
    }

    fn finish_workspace(&mut self, touched_agents: &HashSet<String>) -> io::Result<()> {
        self.refresh_process_state();
        self.runtime.refresh_event_state();
        self.refresh_now();
        for key in touched_agents {
            if let Some(session) = self
                .runtime
                .sessions
                .iter()
                .find(|session| &session.key == key)
            {
                let fingerprint = session
                    .transcript_path
                    .as_deref()
                    .and_then(discovery::transcript_fingerprint)
                    .unwrap_or_else(|| session.transcript_fingerprint.clone());
                self.runtime
                    .store
                    .set_managed_transcript_fingerprint(key, Some(fingerprint));
            }
        }
        self.runtime.store.save_incremental()
    }

    pub fn copy_shell_capture(&mut self) -> Result<(), String> {
        let session = self
            .selected_session()
            .ok_or_else(|| "no selected session".to_owned())?;
        let capture = self
            .terminals
            .shell_capture(&session.key)
            .ok_or_else(|| "open the session shell first".to_owned())?;
        if capture.trim().is_empty() {
            return Err("shell has no captured output".into());
        }
        clipboard::copy(&capture)?;
        self.banner = Some("COPIED · selected shell output is on the clipboard".into());
        Ok(())
    }

    pub fn stage_shell_capture(&mut self) -> Result<(), String> {
        let session = self
            .selected_session()
            .ok_or_else(|| "no selected session".to_owned())?;
        let capture = self
            .terminals
            .shell_capture(&session.key)
            .ok_or_else(|| "open the session shell first".to_owned())?;
        let staged = staged_shell_text(&session.cwd, &capture)
            .ok_or_else(|| "shell has no captured output".to_owned())?;
        let selected_index = self.runtime.selected;
        let selected = self
            .runtime
            .sessions
            .get_mut(selected_index)
            .ok_or_else(|| "no selected session".to_owned())?;
        selected.pending_shell_injection = Some(staged);
        self.runtime.store.update(selected);
        self.runtime
            .store
            .save_incremental()
            .map_err(|error| error.to_string())?;
        self.banner = Some("STAGED · Enter will paste shell output without submitting".into());
        Ok(())
    }

    pub fn tick(&mut self) {
        self.refresh_process_state();
        let alive = self
            .terminals
            .alive_keys()
            .into_iter()
            .collect::<HashSet<_>>();
        for (old_key, new_key) in self.runtime.tick(&alive) {
            self.terminals.rekey(&old_key, new_key);
        }
    }

    pub fn refresh_now(&mut self) {
        let alive = self
            .terminals
            .alive_keys()
            .into_iter()
            .collect::<HashSet<_>>();
        for (old_key, new_key) in self.runtime.poll_discovery(&alive) {
            self.terminals.rekey(&old_key, new_key);
        }
        self.runtime.request_discovery();
    }

    pub fn shutdown(&mut self) -> io::Result<()> {
        for session in &self.runtime.sessions {
            self.runtime.store.update(session);
        }
        self.terminals.shutdown();
        self.runtime.store.save_clean_exit()
    }

    fn refresh_process_state(&mut self) {
        for session in &mut self.runtime.sessions {
            session.managed_alive = self.terminals.agent_alive(&session.key);
        }
    }
}

impl RuntimeState {
    fn tick(&mut self, alive: &HashSet<String>) -> Vec<(String, String)> {
        self.tick_count = self.tick_count.wrapping_add(1);
        self.collect_summary_results();
        let rekeys = self.poll_discovery(alive);
        if !self.discovery_worker.stopped && self.last_discovery.elapsed() >= DISCOVERY_INTERVAL {
            self.request_discovery();
        }
        if self.last_event_refresh.elapsed() >= EVENT_REFRESH_INTERVAL {
            self.refresh_event_state();
            self.observe_changes();
            self.last_event_refresh = Instant::now();
        }
        self.capture_notifications();
        self.schedule_summary();
        rekeys
    }

    fn request_discovery(&mut self) {
        if let Err(error) = self.discovery_worker.request(self.discovery_paths.clone()) {
            self.report_discovery_failure(error);
        }
    }

    fn poll_discovery(&mut self, alive: &HashSet<String>) -> Vec<(String, String)> {
        match self.discovery_worker.poll() {
            Ok(Some(discovered)) => self.apply_discovered(discovered, alive),
            Ok(None) => Vec::new(),
            Err(error) => {
                self.report_discovery_failure(error);
                Vec::new()
            }
        }
    }

    fn report_discovery_failure(&mut self, error: &str) {
        diagnostics::record(error);
        self.banner = Some(format!(
            "DISCOVERY STOPPED · {error} · restart Agent Console"
        ));
    }

    #[cfg(test)]
    fn refresh_now(&mut self, alive: &HashSet<String>) -> Vec<(String, String)> {
        let mut cache = DiscoveryCache::default();
        let discovered = discovery::discover_cached(&self.discovery_paths, &mut cache);
        self.apply_discovered(discovered, alive)
    }

    fn apply_discovered(
        &mut self,
        mut discovered: Vec<Session>,
        alive: &HashSet<String>,
    ) -> Vec<(String, String)> {
        let mut selected_key = self
            .sessions
            .get(self.selected)
            .map(|session| session.key.clone());
        let old = self
            .sessions
            .drain(..)
            .map(|session| (session.key.clone(), session))
            .collect::<HashMap<_, _>>();
        let now = Instant::now();
        let mut old = old;
        let mut effective_alive = alive.clone();
        let provisional_keys = old
            .values()
            .filter(|session| {
                session.agent == AgentKind::Codex
                    && session.transcript_path.is_none()
                    && effective_alive.contains(&session.key)
            })
            .map(|session| session.key.clone())
            .collect::<Vec<_>>();
        let mut claimed = HashSet::new();
        let mut rekeys = Vec::new();
        for provisional_key in provisional_keys {
            let Some(provisional) = old.get(&provisional_key) else {
                continue;
            };
            let candidate = discovered
                .iter()
                .filter(|session| {
                    session.agent == AgentKind::Codex
                        && session.cwd == provisional.cwd
                        && session.transcript_modified_at.saturating_add(5)
                            >= provisional.transcript_modified_at
                        && !old.contains_key(&session.key)
                        && !claimed.contains(&session.key)
                })
                .max_by_key(|session| session.transcript_modified_at)
                .map(|session| (session.key.clone(), session.provider_session_id.clone()));
            let Some((new_key, provider_session_id)) = candidate else {
                continue;
            };
            claimed.insert(new_key.clone());
            if let Some(mut provisional) = old.remove(&provisional_key) {
                if effective_alive.remove(&provisional_key) {
                    effective_alive.insert(new_key.clone());
                }
                rekeys.push((provisional_key.clone(), new_key.clone()));
                self.store.rekey(&provisional_key, &new_key);
                provisional.key = new_key.clone();
                provisional.provider_session_id = provider_session_id;
                old.insert(new_key.clone(), provisional);
                if selected_key.as_deref() == Some(&provisional_key) {
                    selected_key = Some(new_key);
                }
            }
        }
        let events_dir = self.store.events_dir();
        for session in &mut discovered {
            if let Some(previous) = old.remove(&session.key) {
                let changed = previous.transcript_fingerprint != session.transcript_fingerprint;
                let discovered_task = std::mem::take(&mut session.summary.task);
                if previous.first_prompt.is_some() {
                    session.first_prompt.clone_from(&previous.first_prompt);
                }
                session.summary = previous.summary;
                // Caches written before provider command wrappers were filtered
                // still carry one as the task; the parsed prompt wins over it.
                if session.summary.task.trim().is_empty()
                    || discovery::is_internal_context(&session.summary.task)
                {
                    session.summary.task = discovered_task;
                }
                session
                    .summary_fingerprint
                    .clone_from(&previous.summary_fingerprint);
                session.summary_updated_at = previous.summary_updated_at;
                session.summary_error = previous.summary_error;
                session.pending_shell_injection = previous.pending_shell_injection;
                session.pending_decisions = previous.pending_decisions;
                session.managed_alive = effective_alive.contains(&session.key);
                if changed {
                    self.changed_at.insert(session.key.clone(), now);
                }
            } else {
                self.store.apply(session);
                session.discovered_after_startup = true;
            }
            apply_event_inbox(&mut self.event_index, &events_dir, session);
        }

        for mut session in old.into_values() {
            if session.transcript_path.is_none() || effective_alive.contains(&session.key) {
                session.managed_alive = effective_alive.contains(&session.key);
                apply_event_inbox(&mut self.event_index, &events_dir, &mut session);
                discovered.push(session);
            }
        }
        discovered.sort_by(|left, right| {
            right.managed_alive.cmp(&left.managed_alive).then_with(|| {
                right
                    .transcript_modified_at
                    .cmp(&left.transcript_modified_at)
            })
        });
        self.sessions = discovered;
        self.selected = selected_key
            .as_ref()
            .and_then(|key| self.sessions.iter().position(|session| &session.key == key))
            .unwrap_or(0)
            .min(self.sessions.len().saturating_sub(1));
        let visible = self.session_display_order();
        if !visible.contains(&self.selected)
            && let Some(first) = visible.first()
        {
            self.selected = *first;
        }
        self.last_discovery = Instant::now();
        self.queue_changed_sessions();
        self.queue_selected_if_needed();
        self.capture_notifications();
        rekeys
    }

    fn session_display_order(&self) -> Vec<usize> {
        let mut workspaces = Vec::new();
        for session in self.sessions.iter().filter(|session| {
            self.session_is_visible(session) && !self.store.archived(&session.key)
        }) {
            if !workspaces.contains(&session.cwd) {
                workspaces.push(session.cwd.clone());
            }
        }
        let mut order = workspaces
            .into_iter()
            .flat_map(|workspace| {
                self.sessions
                    .iter()
                    .enumerate()
                    .filter_map(move |(index, session)| {
                        (session.cwd == workspace
                            && self.session_is_visible(session)
                            && !self.store.archived(&session.key))
                        .then_some(index)
                    })
            })
            .collect::<Vec<_>>();
        order.extend(
            self.sessions
                .iter()
                .enumerate()
                .filter_map(|(index, session)| {
                    (self.session_is_visible(session) && self.store.archived(&session.key))
                        .then_some(index)
                }),
        );
        order
    }

    fn normalize_selection(&mut self) {
        let order = self.session_display_order();
        if !order.contains(&self.selected)
            && let Some(first) = order.first()
        {
            self.selected = *first;
        }
    }

    fn apply_workspace_search(&mut self, update: WorkspaceSearchUpdate) {
        match update {
            WorkspaceSearchUpdate::Preview(query) => {
                self.filter.query = query.trim().to_lowercase();
                self.normalize_selection();
            }
            WorkspaceSearchUpdate::Cancel {
                query,
                selected_session_key,
            } => {
                self.filter.query = query;
                if let Some(index) = selected_session_key
                    .and_then(|key| self.sessions.iter().position(|session| session.key == key))
                {
                    self.selected = index;
                }
                self.normalize_selection();
            }
        }
    }

    fn session_is_visible(&self, session: &Session) -> bool {
        self.session_matches(session, &self.filter.query)
    }

    /// Whether a session matches a search query, using exactly the fields and the
    /// case-insensitive substring rule the TUI's own search dialog applies.
    ///
    /// Takes the query as an argument rather than reading `self.filter` so a caller can
    /// filter for one request without changing what anyone else sees -- the web server has
    /// many clients, and one browser's search must not reorder another's list.
    pub fn session_matches(&self, session: &Session, query: &str) -> bool {
        // Checked before lowercasing: this runs for every session on every dashboard render,
        // and an unfiltered dashboard is the common case.
        let query = query.trim();
        if query.is_empty() {
            return true;
        }
        let query = query.to_lowercase();
        let alias = self.store.alias(&session.key).unwrap_or_default();
        let haystack = format!(
            "{alias} {} {} {} {} {} {} {} {} {}",
            session.summary.task,
            session.name,
            session.search_terms.join(" "),
            session.cwd.display(),
            session.branch.as_deref().unwrap_or_default(),
            session.provider_session_id,
            session.agent.label(),
            session.status.label(),
            if self.store.archived(&session.key) {
                "archived"
            } else {
                "active"
            }
        )
        .to_lowercase();
        haystack.contains(&query)
    }

    fn session_title(&self, session: &Session) -> String {
        self.store
            .alias(&session.key)
            .map(str::to_owned)
            .unwrap_or_else(|| session.list_title())
    }

    fn filter_summary(&self) -> String {
        let query = if self.filter.query.is_empty() {
            "no search".to_owned()
        } else {
            format!("search={}", self.filter.query)
        };
        format!("filters: {query}")
    }

    fn workspace_chrome(&self) -> WorkspaceChrome {
        let mut lines = Vec::new();
        let mut selected_row = 0;
        let mut last_workspace = None;
        let mut archived_group = false;
        let order = self.session_display_order();
        let selected_visible = order.contains(&self.selected);
        for index in order {
            let session = &self.sessions[index];
            let archived = self.store.archived(&session.key);
            if archived && !archived_group {
                lines.push("▾ Archived".into());
                archived_group = true;
                last_workspace = None;
            } else if !archived && last_workspace != Some(&session.cwd) {
                let label = session
                    .cwd
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or_else(|| session.cwd.to_str().unwrap_or("workspace"));
                lines.push(format!("▾ {label}"));
                last_workspace = Some(&session.cwd);
            }
            if index == self.selected {
                selected_row = lines.len();
            }
            lines.push(format!(
                "{}{} {:<3} {}",
                if archived { "⌁" } else { "" },
                workspace_status_symbol(session.status),
                session.agent.short_label(),
                self.session_title(session)
            ));
        }
        let preview = selected_visible
            .then(|| self.sessions.get(self.selected))
            .flatten()
            .map(|session| {
                let mut preview = vec![
                    format!(
                        "{} · {} · {}",
                        self.session_title(session),
                        session.agent.label(),
                        session.status.label()
                    ),
                    format!("path  {}", session.cwd.display()),
                    format!("git   {}", session.branch.as_deref().unwrap_or("no branch")),
                ];
                if let Some(prompt) = session
                    .first_prompt
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    preview.push(format!("asked {prompt}"));
                }
                preview.extend(summary_preview_lines(session));
                preview.push(String::new());
                preview.push("RECENT TRANSCRIPT".into());
                if session.recent_activity.is_empty() {
                    preview.push("No transcript activity available".into());
                } else {
                    preview.extend(session.recent_activity.iter().rev().take(12).rev().cloned());
                }
                preview.push(String::new());
                preview.push("Enter opens or resumes this agent".into());
                preview
            })
            .unwrap_or_else(|| vec!["No selected session".into()]);
        WorkspaceChrome {
            sessions: lines,
            selected: selected_row,
            selected_session_key: selected_visible
                .then(|| self.sessions.get(self.selected))
                .flatten()
                .map(|session| session.key.clone()),
            search_query: self.filter.query.clone(),
            status_counts: session_status_counts(&self.sessions),
            preview,
            notification: self
                .active_notification()
                .map(|notification| {
                    let title = self
                        .sessions
                        .iter()
                        .find(|session| session.key == notification.session_key)
                        .map(|session| self.session_title(session))
                        .unwrap_or_else(|| "session".into());
                    format!("{title}: {}", notification.message)
                })
                .or_else(|| {
                    self.banner
                        .as_deref()
                        .filter(|banner| banner.starts_with("DISCOVERY STOPPED"))
                        .map(str::to_owned)
                }),
        }
    }

    fn capture_notifications(&mut self) {
        let selected_key = self
            .suppress_selected_notifications
            .then(|| self.sessions.get(self.selected))
            .flatten()
            .map(|session| session.key.clone());
        let current = self
            .sessions
            .iter()
            .map(|session| {
                let message = match session.status {
                    SessionStatus::Waiting => session
                        .pending_decisions
                        .first()
                        .map(|decision| decision.question.clone())
                        .unwrap_or_else(|| "Waiting for your decision".into()),
                    SessionStatus::Failed => session
                        .summary
                        .blockers
                        .first()
                        .cloned()
                        .or_else(|| session.summary_error.clone())
                        .or_else(|| session.recent_activity.last().cloned())
                        .unwrap_or_else(|| "The last turn failed".into()),
                    SessionStatus::Working | SessionStatus::Idle => String::new(),
                };
                (session.key.clone(), session.status, message)
            })
            .collect::<Vec<_>>();
        let live_keys = current
            .iter()
            .map(|(key, _, _)| key.clone())
            .collect::<HashSet<_>>();
        for (key, status, message) in current {
            let previous = self.observed_statuses.insert(key.clone(), status);
            let critical = matches!(status, SessionStatus::Waiting | SessionStatus::Failed);
            if !critical {
                self.mark_notifications_read(&key);
            }
            if previous.is_some_and(|previous| previous != status)
                && critical
                && selected_key.as_deref() != Some(&key)
            {
                self.notifications.push_back(RuntimeNotification {
                    id: Uuid::new_v4().to_string(),
                    session_key: key,
                    status,
                    message,
                    created_at: unix_timestamp(),
                    read: false,
                });
            }
        }
        self.observed_statuses
            .retain(|key, _| live_keys.contains(key));
        while self.notifications.len() > 100 {
            self.notifications.pop_front();
        }
    }

    pub fn unread_notification_count(&self) -> usize {
        self.notifications
            .iter()
            .filter(|notification| !notification.read)
            .count()
    }

    /// The retained alert queue, oldest first -- the order `jump_to_next_notification`
    /// walks it in. Entries stay after being read (up to the 100-entry cap), so a client
    /// that arrives late still sees the recent history rather than an empty list.
    pub fn notifications(&self) -> impl ExactSizeIterator<Item = &RuntimeNotification> {
        self.notifications.iter()
    }

    /// Marks one alert read by id. Idempotent, and reports whether the id was known so a
    /// caller can answer 404 for an id from a queue this process no longer retains.
    pub fn mark_notification_read(&mut self, id: &str) -> bool {
        let Some(notification) = self
            .notifications
            .iter_mut()
            .find(|notification| notification.id == id)
        else {
            return false;
        };
        notification.read = true;
        true
    }

    /// Marks every retained alert read and reports how many were still unread, which is what
    /// a "clear all" acknowledges.
    pub fn mark_all_notifications_read(&mut self) -> usize {
        let mut cleared = 0;
        for notification in &mut self.notifications {
            if !notification.read {
                notification.read = true;
                cleared += 1;
            }
        }
        cleared
    }

    /// Turns off dropping an alert for the currently selected session. The TUI wants that
    /// suppression (the session is on screen); a multi-client server does not, because
    /// "selected" there is an artefact of whichever request last moved it.
    pub fn set_selected_notification_suppression(&mut self, suppress: bool) {
        self.suppress_selected_notifications = suppress;
    }

    pub fn active_notification(&self) -> Option<&RuntimeNotification> {
        self.notifications
            .iter()
            .find(|notification| !notification.read)
    }

    pub fn jump_to_next_notification(&mut self) -> bool {
        let Some(notification) = self
            .notifications
            .iter_mut()
            .find(|notification| !notification.read)
        else {
            return false;
        };
        let Some(index) = self
            .sessions
            .iter()
            .position(|session| session.key == notification.session_key)
        else {
            notification.read = true;
            return false;
        };
        self.selected = index;
        self.filter = SessionFilter::default();
        notification.read = true;
        true
    }

    fn mark_notifications_read(&mut self, session_key: &str) {
        for notification in &mut self.notifications {
            if notification.session_key == session_key {
                notification.read = true;
            }
        }
    }

    fn refresh_event_state(&mut self) {
        let events_dir = self.store.events_dir();
        for session in &mut self.sessions {
            apply_event_inbox(&mut self.event_index, &events_dir, session);
        }
    }

    fn queue_changed_sessions(&mut self) {
        let now = Instant::now();
        let ready = self
            .changed_at
            .iter()
            .filter(|(_, changed)| now.duration_since(**changed) >= SUMMARY_DEBOUNCE)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in ready {
            if let Some(session) = self.sessions.iter().find(|session| session.key == key)
                && session.summary_fingerprint != self.effective_fingerprint(session)
            {
                self.queue_summary(key.clone());
            }
            self.changed_at.remove(&key);
        }
    }

    fn observe_changes(&mut self) {
        let fingerprints = self
            .sessions
            .iter()
            .map(|session| (session.key.clone(), self.effective_fingerprint(session)))
            .collect::<Vec<_>>();
        for (key, fingerprint) in fingerprints {
            match self.observed_fingerprints.get(&key) {
                Some(previous) if previous != &fingerprint => {
                    self.changed_at.insert(key.clone(), Instant::now());
                }
                Some(_) => {}
                None => {}
            }
            self.observed_fingerprints.insert(key, fingerprint);
        }
        let live_keys = self
            .sessions
            .iter()
            .map(|session| session.key.clone())
            .collect::<HashSet<_>>();
        self.observed_fingerprints
            .retain(|key, _| live_keys.contains(key));
    }

    fn queue_selected_if_needed(&mut self) {
        let pending_key = self.sessions.get(self.selected).and_then(|session| {
            (session.summary_fingerprint != self.effective_fingerprint(session))
                .then(|| session.key.clone())
        });
        if let Some(key) = pending_key {
            self.queue_summary(key);
        }
    }

    fn queue_summary(&mut self, key: String) {
        if self.summary_queued.insert(key.clone()) {
            self.summary_queue.push_back(key);
        }
    }

    fn queue_summary_front(&mut self, key: String) {
        if self.summary_queued.insert(key.clone()) {
            self.summary_queue.push_front(key);
        }
    }

    fn next_eligible_summary(&mut self, now: Instant) -> Option<String> {
        let candidates = self.summary_queue.len();
        for _ in 0..candidates {
            let key = self.summary_queue.pop_front()?;
            self.summary_queued.remove(&key);
            let Some(session) = self.sessions.iter().find(|session| session.key == key) else {
                continue;
            };
            let throttled = self
                .last_summary_attempt
                .get(&key)
                .is_some_and(|last| now.duration_since(*last) < self.summary_policy.min_interval);
            let backing_off = self
                .summary_retry_at
                .get(&key)
                .is_some_and(|retry| *retry > now);
            let circuit_open = self
                .provider_circuit_until
                .get(&session.agent)
                .is_some_and(|until| *until > now);
            if throttled || backing_off || circuit_open {
                self.queue_summary(key);
                continue;
            }
            return Some(key);
        }
        None
    }

    fn schedule_summary(&mut self) {
        if self.summary_busy.is_some() || self.summary_worker.backend == SummaryBackend::Off {
            return;
        }
        self.queue_changed_sessions();
        let now = Instant::now();
        let key = self.next_eligible_summary(now);
        let Some(key) = key else {
            return;
        };
        let Some(session) = self.sessions.iter().find(|session| session.key == key) else {
            return;
        };
        let fingerprint = self.effective_fingerprint(session);
        let mut records = session.recent_activity.clone();
        records.extend(self.session_events(session).into_iter().map(event_record));
        let job = SummaryJob {
            session_key: key.clone(),
            provider: session.agent,
            fingerprint,
            previous: session.summary.clone(),
            records,
        };
        match self.summary_worker.enqueue(job) {
            Ok(()) => {
                self.summary_busy = Some(key.clone());
                self.last_summary_attempt.insert(key, now);
            }
            Err(error) => {
                self.queue_summary(key);
                self.banner = Some(error);
            }
        }
    }

    fn record_summary_failure(&mut self, key: &str, provider: AgentKind, now: Instant) {
        let failures = self.summary_failures.entry(key.to_owned()).or_default();
        *failures = failures.saturating_add(1);
        let exponent = failures.saturating_sub(1).min(6);
        let delay = self
            .summary_policy
            .failure_backoff
            .saturating_mul(1_u32 << exponent);
        self.summary_retry_at.insert(key.to_owned(), now + delay);
        let provider_failures = self.provider_failures.entry(provider).or_default();
        *provider_failures = provider_failures.saturating_add(1);
        if *provider_failures >= self.summary_policy.circuit_failures {
            self.provider_circuit_until
                .insert(provider, now + self.summary_policy.circuit_cooldown);
        }
    }

    pub fn retry_selected_summary(&mut self) -> Result<(), String> {
        let key = self
            .sessions
            .get(self.selected)
            .map(|session| session.key.clone())
            .ok_or_else(|| "no selected session".to_owned())?;
        self.retry_summary(&key)
    }

    /// Retries one session's summary by key, clearing the same per-session backoff and
    /// per-provider circuit breaker `retry_selected_summary` clears and jumping it to the
    /// front of the queue. Keyed rather than selection-based so a request can name its own
    /// session instead of moving a selection several clients share.
    pub fn retry_summary(&mut self, key: &str) -> Result<(), String> {
        let session = self
            .sessions
            .iter()
            .find(|session| session.key == key)
            .ok_or_else(|| format!("no session with key {key}"))?;
        let key = session.key.clone();
        let provider = session.agent;
        self.summary_retry_at.remove(&key);
        self.last_summary_attempt.remove(&key);
        self.provider_circuit_until.remove(&provider);
        self.provider_failures.remove(&provider);
        self.summary_queued.remove(&key);
        self.summary_queue.retain(|queued| queued != &key);
        self.queue_summary_front(key);
        self.banner = Some("summary retry queued".into());
        Ok(())
    }

    fn collect_summary_results(&mut self) {
        while let Some(result) = self.summary_worker.try_result() {
            self.summary_busy = None;
            let Some(index) = self
                .sessions
                .iter()
                .position(|session| session.key == result.session_key)
            else {
                continue;
            };
            let provider = self.sessions[index].agent;
            let session_key = self.sessions[index].key.clone();
            let succeeded = match result.result {
                Ok(mut summary) => {
                    let session = &mut self.sessions[index];
                    summary.status = session.status;
                    summary.needs_user.clone_from(&session.pending_decisions);
                    session.summary = summary;
                    session.summary_fingerprint = result.fingerprint;
                    session.summary_updated_at = Some(unix_timestamp());
                    session.summary_error = None;
                    true
                }
                Err(error) => {
                    diagnostics::record(&format!(
                        "summary failed for {session_key} ({provider:?}): {error}"
                    ));
                    self.sessions[index].summary_error = Some(error);
                    false
                }
            };
            self.store.update(&self.sessions[index]);
            if succeeded {
                self.summary_failures.remove(&session_key);
                self.summary_retry_at.remove(&session_key);
                self.provider_failures.remove(&provider);
                self.provider_circuit_until.remove(&provider);
            } else {
                self.record_summary_failure(&session_key, provider, Instant::now());
            }
            let needs_requeue = {
                let session = &self.sessions[index];
                self.effective_fingerprint(session) != session.summary_fingerprint
            };
            if needs_requeue {
                self.queue_summary(session_key);
            }
            if let Err(error) = self.store.save_incremental() {
                self.banner = Some(format!("cannot save summary cache: {error}"));
            }
        }
    }

    fn effective_fingerprint(&self, session: &Session) -> String {
        let path = events::event_file(
            &self.store.events_dir(),
            session.agent,
            &session.provider_session_id,
        );
        let event_fingerprint = fs::metadata(path)
            .ok()
            .map(|metadata| {
                let modified = metadata
                    .modified()
                    .ok()
                    .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |value| value.as_secs());
                format!("{modified}:{}", metadata.len())
            })
            .unwrap_or_default();
        format!("{}|{event_fingerprint}", session.transcript_fingerprint)
    }

    fn session_events(&self, session: &Session) -> Vec<NormalizedEvent> {
        self.event_index
            .events(session.agent, &session.provider_session_id)
            .unwrap_or_default()
    }
}

/// The rolling summary belongs in the preview, not in the session title, so the
/// title stays the first prompt while this tracks what the agent is doing now.
fn summary_preview_lines(session: &Session) -> Vec<String> {
    let summary = &session.summary;
    let mut lines = Vec::new();
    for (label, value) in [
        ("task ", summary.task.as_str()),
        ("now  ", summary.current_action.as_str()),
        ("next ", summary.next_step.as_str()),
    ] {
        let value = value.trim();
        if !value.is_empty() {
            lines.push(format!("{label} {value}"));
        }
    }
    if let Some(blocker) = summary
        .blockers
        .first()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        lines.push(format!("block {blocker}"));
    }
    if let Some(error) = session.summary_error.as_deref() {
        lines.push(format!("summary stale: {error}"));
    }
    lines
}

fn session_status_counts(sessions: &[Session]) -> (usize, usize, usize, usize) {
    sessions.iter().fold((0, 0, 0, 0), |mut counts, session| {
        match session.status {
            SessionStatus::Working => counts.0 += 1,
            SessionStatus::Waiting => counts.1 += 1,
            SessionStatus::Idle => counts.2 += 1,
            SessionStatus::Failed => counts.3 += 1,
        }
        counts
    })
}

impl App {
    #[cfg(test)]
    pub fn test_fixture() -> Self {
        let root = std::env::temp_dir().join(format!("agent-console-ui-{}", Uuid::new_v4()));
        let (store, _) = StateStore::load(root.clone()).unwrap();
        let event_index = events::EventIndex::open(&root).unwrap();
        let summary_worker = SummaryWorker::start(
            SummaryBackend::Off,
            root.clone(),
            root.join("summary-schema.json"),
            AgentConsoleConfig::default(),
        )
        .unwrap();
        let mut session = Session {
            key: "codex:test".into(),
            provider_session_id: "test".into(),
            name: "backend-api".into(),
            search_terms: Vec::new(),
            first_prompt: None,
            agent: AgentKind::Codex,
            status: SessionStatus::Working,
            cwd: "/tmp/backend-api".into(),
            branch: Some("feat/session-api".into()),
            transcript_path: None,
            transcript_modified_at: unix_timestamp(),
            transcript_fingerprint: "test".into(),
            summary_fingerprint: "test".into(),
            summary_updated_at: Some(unix_timestamp()),
            summary_error: None,
            summary: SessionSummary {
                task: "Implement refresh-token rotation".into(),
                progress: vec!["Added RefreshTokenStore".into()],
                current_action: "Running authentication tests".into(),
                next_step: "Check expired-token edge case".into(),
                ..SessionSummary::default()
            },
            recent_activity: vec!["3 tests passed".into()],
            pending_decisions: Vec::new(),
            pending_shell_injection: None,
            managed_alive: true,
            unavailable_reason: None,
            discovered_after_startup: false,
        };
        session.summary.status = SessionStatus::Working;
        Self {
            runtime: RuntimeState {
                sessions: vec![session],
                selected: 0,
                tick_count: 0,
                banner: None,
                summary_busy: None,
                discovery_paths: DiscoveryPaths {
                    codex_sessions: root.join("codex"),
                    claude_projects: root.join("claude"),
                    pi_sessions: PathBuf::new(),
                },
                discovery_worker: DiscoveryWorker::start(DiscoveryCache::default()).unwrap(),
                last_discovery: Instant::now(),
                last_event_refresh: Instant::now(),
                changed_at: HashMap::new(),
                observed_fingerprints: HashMap::new(),
                summary_queue: VecDeque::new(),
                summary_queued: HashSet::new(),
                last_summary_attempt: HashMap::new(),
                summary_retry_at: HashMap::new(),
                summary_failures: HashMap::new(),
                provider_failures: HashMap::new(),
                provider_circuit_until: HashMap::new(),
                summary_policy: SummaryPolicy::from(&AgentConsoleConfig::default()),
                observed_statuses: HashMap::new(),
                notifications: VecDeque::new(),
                suppress_selected_notifications: true,
                filter: SessionFilter::default(),
                store,
                event_index,
                summary_worker,
            },
            dialog: None,
            text_dialog: None,
            help_open: false,
            startup_cwd: "/tmp".into(),
            terminals: TerminalManager::default(),
            config: AgentConsoleConfig::default(),
            web_status: WebStatus::Disabled,
        }
    }
}

fn event_record(event: NormalizedEvent) -> String {
    format!("{:?}: {}", event.kind, event.text)
}

fn apply_event_inbox(
    event_index: &mut events::EventIndex,
    events_dir: &Path,
    session: &mut Session,
) {
    let path = events::event_file(events_dir, session.agent, &session.provider_session_id);
    if let Ok(events) =
        event_index.refresh_session(&path, session.agent, &session.provider_session_id)
    {
        events::apply_events(session, &events);
    } else {
        session.apply_deterministic_status(false, session.status == SessionStatus::Failed);
    }
}

fn workspace_status_symbol(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Working => "●",
        SessionStatus::Waiting => "!",
        SessionStatus::Failed => "×",
        SessionStatus::Idle => "○",
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ManagedAgentRefresh {
    Reuse,
    Restart,
    Conflict,
}

fn managed_agent_refresh(
    session: &Session,
    managed_fingerprint: Option<&str>,
    agent_alive: bool,
) -> ManagedAgentRefresh {
    let stale = agent_alive
        && session.transcript_path.is_some()
        && managed_fingerprint != Some(session.transcript_fingerprint.as_str());
    if !stale {
        return ManagedAgentRefresh::Reuse;
    }
    if matches!(session.status, SessionStatus::Idle | SessionStatus::Failed) {
        ManagedAgentRefresh::Restart
    } else {
        ManagedAgentRefresh::Conflict
    }
}

/// The address without its query string, for the help panel's narrow column.
///
/// The full URL, token and all, is on the dashboard's own header line; a token clipped in
/// half by a column boundary would be worse than not showing one at all.
fn short_url(url: &str) -> String {
    url.split_once('?')
        .map_or(url, |(base, _)| base)
        .trim_end_matches('/')
        .to_owned()
}

fn expand_tilde(value: &str) -> PathBuf {
    if value == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(value));
    }
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(value)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn refresh_preserves_selection_by_stable_key() {
        let root = tempdir().unwrap();
        let codex = root.path().join("codex");
        let claude = root.path().join("claude");
        let state = root.path().join("state");
        fs::create_dir_all(&codex).unwrap();
        fs::create_dir_all(&claude).unwrap();
        // This test exercises the merge independently of global HOME by constructing an App.
        let (store, _) = StateStore::load(state.clone()).unwrap();
        let event_index = events::EventIndex::open(&state).unwrap();
        let worker = SummaryWorker::start(
            SummaryBackend::Off,
            state.clone(),
            state.join("schema.json"),
            AgentConsoleConfig::default(),
        )
        .unwrap();
        let mut app = App {
            runtime: RuntimeState {
                sessions: vec![fixture_session("codex:one"), fixture_session("codex:two")],
                selected: 1,
                tick_count: 0,
                banner: None,
                summary_busy: None,
                discovery_paths: DiscoveryPaths {
                    codex_sessions: codex,
                    claude_projects: claude,
                    pi_sessions: PathBuf::new(),
                },
                discovery_worker: DiscoveryWorker::start(DiscoveryCache::default()).unwrap(),
                last_discovery: Instant::now(),
                last_event_refresh: Instant::now(),
                changed_at: HashMap::new(),
                observed_fingerprints: HashMap::new(),
                summary_queue: VecDeque::new(),
                summary_queued: HashSet::new(),
                last_summary_attempt: HashMap::new(),
                summary_retry_at: HashMap::new(),
                summary_failures: HashMap::new(),
                provider_failures: HashMap::new(),
                provider_circuit_until: HashMap::new(),
                summary_policy: SummaryPolicy::from(&AgentConsoleConfig::default()),
                observed_statuses: HashMap::new(),
                notifications: VecDeque::new(),
                suppress_selected_notifications: true,
                filter: SessionFilter::default(),
                store,
                event_index,
                summary_worker: worker,
            },
            dialog: None,
            text_dialog: None,
            help_open: false,
            startup_cwd: root.path().to_owned(),
            terminals: TerminalManager::default(),
            config: AgentConsoleConfig::default(),
            web_status: WebStatus::Disabled,
        };
        // Provisional sessions are retained by refresh.
        app.refresh_now();
        assert_eq!(app.selected_session().unwrap().key, "codex:two");
    }

    #[test]
    fn periodic_discovery_does_not_block_tick_or_queue_duplicate_scans() {
        let mut app = App::test_fixture();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        app.runtime.discovery_worker =
            DiscoveryWorker::start_with_runner(DiscoveryCache::default(), move |_, _| {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                finished_tx.send(()).unwrap();
                Vec::new()
            })
            .unwrap();
        app.runtime.last_discovery = Instant::now() - DISCOVERY_INTERVAL;

        let started_at = Instant::now();
        app.runtime.tick(&HashSet::new());
        assert!(
            started_at.elapsed() < Duration::from_millis(500),
            "the UI tick must only enqueue discovery"
        );
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        for _ in 0..3 {
            let started_at = Instant::now();
            app.runtime.tick(&HashSet::new());
            assert!(
                started_at.elapsed() < Duration::from_millis(500),
                "an in-flight scan must not block or accept a duplicate"
            );
        }
        assert!(started_rx.try_recv().is_err());

        release_tx.send(()).unwrap();
        finished_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while app.runtime.discovery_worker.in_flight && Instant::now() < deadline {
            app.runtime.tick(&HashSet::new());
            thread::yield_now();
        }
        assert!(!app.runtime.discovery_worker.in_flight);
        assert!(started_rx.try_recv().is_err());
    }

    #[test]
    fn a_stopped_discovery_worker_is_reported_to_the_user() {
        let mut app = App::test_fixture();
        app.runtime.discovery_worker = DiscoveryWorker::disconnected();

        app.runtime.tick(&HashSet::new());

        assert!(app.runtime.discovery_worker.stopped);
        assert!(
            app.runtime
                .banner
                .as_deref()
                .is_some_and(|banner| banner.contains("DISCOVERY STOPPED")),
            "background discovery failure should be visible: {:?}",
            app.runtime.banner
        );
        assert!(
            app.runtime
                .workspace_chrome()
                .notification
                .as_deref()
                .is_some_and(|notice| notice.contains("DISCOVERY STOPPED"))
        );
    }

    #[test]
    fn finishing_workspace_records_the_current_transcript_metadata_without_scanning() {
        let root = tempdir().unwrap();
        let transcript_path = root.path().join("rollout-finished.jsonl");
        fs::write(&transcript_path, b"new transcript bytes").unwrap();
        let expected = discovery::transcript_fingerprint(&transcript_path).unwrap();
        let mut app = App::test_fixture();
        app.runtime.sessions[0].transcript_path = Some(transcript_path);
        app.runtime.sessions[0].transcript_fingerprint = "stale-in-memory".into();

        app.finish_workspace(&HashSet::from(["codex:test".to_owned()]))
            .unwrap();

        assert_eq!(
            app.runtime
                .store
                .managed_transcript_fingerprint("codex:test"),
            Some(expected.as_str())
        );
    }

    #[test]
    fn new_session_dialog_validates_cwd_and_creates_stable_session() {
        let root = tempdir().unwrap();
        let mut app = App::test_fixture();
        app.open_new_dialog();
        let dialog = app.dialog.as_mut().unwrap();
        dialog.provider = AgentKind::Claude;
        dialog.cwd = root.path().display().to_string();
        app.create_from_dialog().unwrap();
        let session = app.selected_session().unwrap();
        assert_eq!(session.agent, AgentKind::Claude);
        assert_eq!(session.cwd, root.path());
        assert!(Uuid::parse_str(&session.provider_session_id).is_ok());
        assert!(session.transcript_path.is_none());
    }

    #[test]
    fn workspace_sidebar_groups_sessions_and_uses_first_prompt_titles() {
        let mut app = App::test_fixture();
        app.sessions[0].first_prompt = Some("refresh tokens".into());
        app.sessions[0].status = SessionStatus::Working;
        let mut same_workspace = app.sessions[0].clone();
        same_workspace.key = "claude:timeout".into();
        same_workspace.agent = AgentKind::Claude;
        same_workspace.status = SessionStatus::Waiting;
        same_workspace.first_prompt = Some("fix timeout".into());
        let mut other_workspace = app.sessions[0].clone();
        other_workspace.key = "codex:frontend".into();
        other_workspace.name = "frontend".into();
        other_workspace.cwd = "/tmp/frontend".into();
        other_workspace.status = SessionStatus::Failed;
        other_workspace.first_prompt = Some("update navbar".into());
        app.sessions = vec![app.sessions[0].clone(), same_workspace, other_workspace];

        let chrome = app.workspace_chrome();

        assert_eq!(
            chrome.sessions,
            [
                "▾ backend-api",
                "● Cdx refresh tokens",
                "! Cla fix timeout",
                "▾ frontend",
                "× Cdx update navbar",
            ]
        );
        assert_eq!(chrome.selected, 1);
        assert_eq!(chrome.status_counts, (1, 1, 0, 1));
    }

    #[test]
    fn the_preview_carries_the_first_prompt_and_the_rolling_summary() {
        let mut app = App::test_fixture();
        app.sessions[0].first_prompt = Some("Add signed releases".into());
        app.sessions[0].summary.task = "Rework the notarization step".into();
        app.sessions[0].summary.current_action = "Stapling the ticket".into();
        app.sessions[0].summary.next_step = "Re-run the release job".into();
        app.sessions[0].summary.blockers = vec!["Apple service is rate limiting".into()];
        app.selected = 0;

        let preview = app.workspace_chrome().preview.join("\n");

        assert!(preview.contains("asked Add signed releases"), "{preview}");
        assert!(
            preview.contains("Rework the notarization step"),
            "{preview}"
        );
        assert!(preview.contains("Stapling the ticket"), "{preview}");
        assert!(preview.contains("Re-run the release job"), "{preview}");
        assert!(
            preview.contains("Apple service is rate limiting"),
            "{preview}"
        );
        assert!(
            preview.starts_with("Add signed releases · "),
            "the preview header keeps the title: {preview}"
        );
    }

    #[test]
    fn navigation_follows_the_grouped_workspace_order() {
        let mut app = App::test_fixture();
        let mut first = app.sessions[0].clone();
        first.key = "codex:a1".into();
        first.cwd = "/tmp/a".into();
        let mut second = first.clone();
        second.key = "codex:b1".into();
        second.cwd = "/tmp/b".into();
        let mut third = first.clone();
        third.key = "codex:a2".into();
        app.sessions = vec![first, second, third];
        app.selected = 0;

        app.select_next();
        assert_eq!(app.selected_session().unwrap().key, "codex:a2");
        app.select_next();
        assert_eq!(app.selected_session().unwrap().key, "codex:b1");
        app.select_previous();
        assert_eq!(app.selected_session().unwrap().key, "codex:a2");
    }

    #[test]
    fn runtime_updates_workspace_chrome_while_agent_view_is_open() {
        let mut app = App::test_fixture();
        let hook = serde_json::json!({
            "session_id": "test",
            "hook_event_name": "PermissionRequest",
            "request_id": "runtime-waiting",
            "message": "Choose a deployment target"
        });
        events::ingest_hook(AgentKind::Codex, &hook, &app.runtime.store.events_dir()).unwrap();
        app.runtime.last_event_refresh = Instant::now() - EVENT_REFRESH_INTERVAL;

        app.runtime.tick(&HashSet::new());

        assert_eq!(app.sessions[0].status, SessionStatus::Waiting);
        assert!(
            app.runtime
                .workspace_chrome()
                .sessions
                .iter()
                .any(|line| line.starts_with("! Cdx"))
        );
    }

    #[test]
    fn background_status_transition_notifies_once_and_jumps_directly() {
        let mut app = App::test_fixture();
        let mut background = fixture_session("codex:background");
        background.summary.task = "Deploy release".into();
        app.sessions.push(background);
        app.runtime.capture_notifications();

        app.sessions[1].status = SessionStatus::Waiting;
        app.sessions[1]
            .pending_decisions
            .push(crate::model::Decision {
                id: "release-target".into(),
                question: "Choose the release target".into(),
            });
        app.runtime.capture_notifications();
        app.runtime.capture_notifications();

        assert_eq!(app.runtime.unread_notification_count(), 1);
        assert_eq!(
            app.runtime.active_notification().unwrap().message,
            "Choose the release target"
        );
        assert!(app.runtime.jump_to_next_notification());
        assert_eq!(app.selected, 1);
        assert_eq!(app.runtime.unread_notification_count(), 0);

        app.selected = 0;
        app.sessions[1].pending_decisions.clear();
        app.sessions[1].status = SessionStatus::Idle;
        app.runtime.capture_notifications();
        app.sessions[1].status = SessionStatus::Failed;
        app.sessions[1].summary.blockers = vec!["Integration test failed".into()];
        app.runtime.capture_notifications();
        assert_eq!(app.runtime.unread_notification_count(), 1);
        assert_eq!(
            app.runtime.active_notification().unwrap().message,
            "Integration test failed"
        );
    }

    #[test]
    fn background_notification_clears_when_the_session_recovers_or_is_opened() {
        let mut app = App::test_fixture();
        let mut background = fixture_session("codex:background");
        background.status = SessionStatus::Idle;
        app.sessions.push(background);
        app.runtime.capture_notifications();

        app.sessions[1].status = SessionStatus::Waiting;
        app.runtime.capture_notifications();
        assert_eq!(app.runtime.unread_notification_count(), 1);

        app.sessions[1].status = SessionStatus::Working;
        app.runtime.capture_notifications();

        assert_eq!(app.runtime.unread_notification_count(), 0);
        assert!(app.runtime.active_notification().is_none());

        app.sessions[1].status = SessionStatus::Waiting;
        app.runtime.capture_notifications();
        assert_eq!(app.runtime.unread_notification_count(), 1);

        app.runtime.mark_notifications_read("codex:background");
        assert_eq!(app.runtime.unread_notification_count(), 0);
    }

    #[test]
    fn summary_queue_skips_backoff_and_open_circuit_without_starving_other_provider() {
        let mut app = App::test_fixture();
        let mut claude = fixture_session("claude:ready");
        claude.agent = AgentKind::Claude;
        app.sessions.push(claude);
        app.runtime.queue_summary("codex:test".into());
        app.runtime.queue_summary("claude:ready".into());
        app.runtime.summary_retry_at.insert(
            "codex:test".into(),
            Instant::now() + Duration::from_secs(60),
        );

        assert_eq!(
            app.runtime.next_eligible_summary(Instant::now()),
            Some("claude:ready".into())
        );

        app.runtime.queue_summary("codex:test".into());
        app.runtime
            .provider_circuit_until
            .insert(AgentKind::Codex, Instant::now() + Duration::from_secs(60));
        assert_eq!(app.runtime.next_eligible_summary(Instant::now()), None);
    }

    #[test]
    fn summary_failures_back_off_exponentially_and_open_provider_circuit() {
        let mut app = App::test_fixture();
        app.runtime.summary_policy.failure_backoff = Duration::from_secs(2);
        app.runtime.summary_policy.circuit_failures = 2;
        app.runtime.summary_policy.circuit_cooldown = Duration::from_secs(30);
        let now = Instant::now();

        app.runtime
            .record_summary_failure("codex:test", AgentKind::Codex, now);
        assert_eq!(
            app.runtime.summary_retry_at["codex:test"],
            now + Duration::from_secs(2)
        );
        assert!(
            !app.runtime
                .provider_circuit_until
                .contains_key(&AgentKind::Codex)
        );

        app.runtime
            .record_summary_failure("codex:test", AgentKind::Codex, now);
        assert_eq!(
            app.runtime.summary_retry_at["codex:test"],
            now + Duration::from_secs(4)
        );
        assert_eq!(
            app.runtime.provider_circuit_until[&AgentKind::Codex],
            now + Duration::from_secs(30)
        );
    }

    #[test]
    fn manual_summary_retry_clears_all_gates() {
        let mut app = App::test_fixture();
        let now = Instant::now();
        app.runtime
            .summary_retry_at
            .insert("codex:test".into(), now + Duration::from_secs(60));
        app.runtime
            .last_summary_attempt
            .insert("codex:test".into(), now);
        app.runtime
            .provider_circuit_until
            .insert(AgentKind::Codex, now + Duration::from_secs(60));
        app.runtime.provider_failures.insert(AgentKind::Codex, 3);

        app.retry_selected_summary().unwrap();

        assert!(!app.runtime.summary_retry_at.contains_key("codex:test"));
        assert!(!app.runtime.last_summary_attempt.contains_key("codex:test"));
        assert!(
            !app.runtime
                .provider_circuit_until
                .contains_key(&AgentKind::Codex)
        );
        assert_eq!(
            app.runtime.summary_queue.front().map(String::as_str),
            Some("codex:test")
        );
    }

    #[test]
    fn archived_sessions_move_to_the_end_and_remain_selectable_for_restore() {
        let mut app = App::test_fixture();
        let mut second = fixture_session("claude:second");
        second.agent = AgentKind::Claude;
        second.cwd = app.sessions[0].cwd.clone();
        second.summary.task = "Investigate latency".into();
        app.sessions.push(second);
        app.selected = 1;

        app.open_alias_dialog();
        app.text_dialog.as_mut().unwrap().value = "urgent release".into();
        app.commit_text_dialog().unwrap();
        let updated = app.sessions[1].clone();
        app.runtime.store.update(&updated);

        assert_eq!(app.session_title(&app.sessions[1]), "urgent release");
        assert_eq!(app.session_display_order(), vec![0, 1]);

        app.toggle_selected_archive().unwrap();
        assert_eq!(app.session_display_order(), vec![0, 1]);
        assert_eq!(app.selected, 1);
        assert!(app.session_archived(&app.sessions[1]));

        app.toggle_selected_archive().unwrap();
        app.selected = 0;
        app.toggle_selected_archive().unwrap();
        assert_eq!(app.session_display_order(), vec![1, 0]);
        assert_eq!(app.selected, 0);
        app.toggle_selected_archive().unwrap();
        assert_eq!(app.session_display_order(), vec![0, 1]);
        assert_eq!(app.session_title(&app.sessions[1]), "urgent release");
    }

    #[test]
    fn search_matches_task_provider_workspace_and_status() {
        let mut app = App::test_fixture();
        let mut claude = fixture_session("claude:latency");
        claude.agent = AgentKind::Claude;
        claude.cwd = "/tmp/other".into();
        claude.summary.task = "Investigate API latency".into();
        claude.search_terms = vec!["OIDC rollout".into(), "pr #4869 deepmap/airflow".into()];
        app.sessions.push(claude);

        app.open_search_dialog();
        app.text_dialog.as_mut().unwrap().value = "latency".into();
        app.commit_text_dialog().unwrap();
        assert_eq!(app.session_display_order(), vec![1]);
        assert_eq!(app.selected, 1);

        for query in [
            "claude",
            "other",
            "idle",
            "oidc rollout",
            "pr #4869",
            "deepmap/airflow",
        ] {
            app.open_search_dialog();
            app.text_dialog.as_mut().unwrap().value = query.into();
            app.commit_text_dialog().unwrap();
            assert_eq!(app.session_display_order(), vec![1]);
        }
    }

    #[test]
    fn workspace_search_preview_and_cancel_restore_the_original_session() {
        let mut app = App::test_fixture();
        let original_key = app.sessions[0].key.clone();
        let mut claude = fixture_session("claude:latency");
        claude.agent = AgentKind::Claude;
        claude.summary.task = "Investigate API latency".into();
        app.sessions.push(claude);

        app.runtime
            .apply_workspace_search(WorkspaceSearchUpdate::Preview("LATENCY".into()));
        assert_eq!(app.runtime.filter.query, "latency");
        assert_eq!(app.session_display_order(), vec![1]);
        assert_eq!(app.selected, 1);

        app.runtime
            .apply_workspace_search(WorkspaceSearchUpdate::Cancel {
                query: String::new(),
                selected_session_key: Some(original_key),
            });
        assert!(app.runtime.filter.query.is_empty());
        assert_eq!(app.selected, 0);
        assert_eq!(app.session_display_order(), vec![0, 1]);
    }

    #[test]
    fn stale_idle_agent_restarts_but_active_agent_is_preserved() {
        let mut session = fixture_session("codex:stale");
        session.transcript_path = Some("/tmp/session.jsonl".into());
        session.transcript_fingerprint = "new-fingerprint".into();

        assert_eq!(
            managed_agent_refresh(&session, Some("new-fingerprint"), true),
            ManagedAgentRefresh::Reuse
        );
        assert_eq!(
            managed_agent_refresh(&session, Some("old-fingerprint"), true),
            ManagedAgentRefresh::Restart
        );
        assert_eq!(
            managed_agent_refresh(&session, None, true),
            ManagedAgentRefresh::Restart,
            "legacy managed agents have no baseline and must be refreshed once idle"
        );

        session.status = SessionStatus::Working;
        assert_eq!(
            managed_agent_refresh(&session, Some("old-fingerprint"), true),
            ManagedAgentRefresh::Conflict
        );
        assert_eq!(
            managed_agent_refresh(&session, Some("old-fingerprint"), false),
            ManagedAgentRefresh::Reuse
        );
    }

    #[test]
    fn legacy_daemon_agent_is_hydrated_before_stale_restart_decision() {
        let root = tempdir().unwrap();
        let launches = root.path().join("launches");
        let provider = root.path().join("fake-codex");
        fs::write(
            &provider,
            format!(
                "#!/bin/sh\nprintf x >> '{}'\nprintf 'fake agent ready\\n'\nexec /bin/cat\n",
                launches.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&provider).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&provider, permissions).unwrap();
        }
        let config = AgentConsoleConfig::parse(
            &format!("[providers]\ncodex = [\"{}\"]\n", provider.display()),
            Path::new("config.toml"),
        )
        .unwrap();
        let mut session = fixture_session("codex:legacy");
        session.transcript_path = Some(root.path().join("session.jsonl"));
        session.transcript_fingerprint = "latest".into();
        let mut terminals = TerminalManager::new_local(config.clone());
        terminals
            .ensure_agent(&session, Path::new("/tmp/agent-console"), false, (80, 24))
            .unwrap()
            .wait_for_first_output(Duration::from_secs(1));
        assert_eq!(fs::read_to_string(&launches).unwrap(), "x");

        let mut app = App::test_fixture();
        app.runtime.sessions = vec![session];
        app.runtime.selected = 0;
        app.terminals = terminals;
        app.config = config;
        app.prepare_selected_agent(Path::new("/tmp/agent-console"))
            .unwrap();

        assert_eq!(
            fs::read_to_string(&launches).unwrap(),
            "xx",
            "an already-running legacy agent must be discovered, classified stale, and restarted"
        );
    }

    #[test]
    fn dashboard_entry_reuses_a_live_managed_agent_after_its_transcript_changes() {
        let root = tempdir().unwrap();
        let launches = root.path().join("launches");
        let provider = root.path().join("fake-codex");
        fs::write(
            &provider,
            format!(
                "#!/bin/sh\nprintf x >> '{}'\nprintf 'fake agent ready\\n'\nexec /bin/cat\n",
                launches.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&provider).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&provider, permissions).unwrap();
        }
        let config = AgentConsoleConfig::parse(
            &format!("[providers]\ncodex = [\"{}\"]\n", provider.display()),
            Path::new("config.toml"),
        )
        .unwrap();
        let mut session = fixture_session("codex:live");
        session.status = SessionStatus::Working;
        session.transcript_path = Some(root.path().join("session.jsonl"));
        session.transcript_fingerprint = "new-fingerprint".into();
        let mut terminals = TerminalManager::new_local(config.clone());
        terminals
            .ensure_agent(&session, Path::new("/tmp/agent-console"), false, (80, 24))
            .unwrap()
            .wait_for_first_output(Duration::from_secs(1));

        let mut app = App::test_fixture();
        app.runtime.sessions = vec![session];
        app.runtime.selected = 0;
        app.runtime
            .store
            .set_managed_transcript_fingerprint("codex:live", Some("old-fingerprint".into()));
        app.terminals = terminals;
        app.config = config;

        let selected = app
            .prepare_agent_entry(Path::new("/tmp/agent-console"))
            .unwrap();

        assert_eq!(selected.key, "codex:live");
        assert_eq!(
            fs::read_to_string(&launches).unwrap(),
            "x",
            "re-entering must attach the existing PTY instead of restarting the provider"
        );
    }

    #[test]
    fn provisional_session_adopts_the_first_discovered_prompt_as_its_title() {
        let root = tempdir().unwrap();
        let codex_sessions = root.path().join("codex");
        fs::create_dir_all(&codex_sessions).unwrap();
        fs::write(
            codex_sessions.join("rollout-new-session.jsonl"),
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"actual-id\",\"cwd\":\"/tmp\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"Implement signed releases\"}}\n"
            ),
        )
        .unwrap();

        let mut app = App::test_fixture();
        let mut provisional = fixture_session("codex:provisional-id");
        provisional.transcript_modified_at = unix_timestamp();
        provisional.managed_alive = true;
        app.runtime.sessions = vec![provisional];
        app.runtime.selected = 0;
        app.runtime.discovery_paths = DiscoveryPaths {
            codex_sessions,
            claude_projects: root.path().join("claude"),
            pi_sessions: PathBuf::new(),
        };
        let alive = HashSet::from(["codex:provisional-id".to_owned()]);

        let rekeys = app.runtime.refresh_now(&alive);

        assert_eq!(
            rekeys,
            vec![("codex:provisional-id".into(), "codex:actual-id".into())]
        );
        assert_eq!(app.runtime.sessions[0].key, "codex:actual-id");
        assert_eq!(
            app.session_title(&app.runtime.sessions[0]),
            "Implement signed releases",
            "the parsed prompt must replace the provisional session-id title"
        );
    }

    #[test]
    fn refresh_never_replaces_a_known_first_prompt() {
        let root = tempdir().unwrap();
        let codex_sessions = root.path().join("codex");
        fs::create_dir_all(&codex_sessions).unwrap();
        fs::write(
            codex_sessions.join("rollout-known-title.jsonl"),
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"known-title\",\"cwd\":\"/tmp\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"A later prompt must not replace the title\"}}\n"
            ),
        )
        .unwrap();

        let mut app = App::test_fixture();
        let mut previous = fixture_session("codex:known-title");
        previous.first_prompt = Some("Keep this stable title".into());
        previous.transcript_fingerprint = "stale".into();
        app.runtime.sessions = vec![previous];
        app.runtime.selected = 0;
        app.runtime.discovery_paths = DiscoveryPaths {
            codex_sessions,
            claude_projects: root.path().join("claude"),
            pi_sessions: PathBuf::new(),
        };

        app.runtime.refresh_now(&HashSet::new());

        assert_eq!(
            app.runtime.sessions[0].first_prompt.as_deref(),
            Some("Keep this stable title")
        );
    }

    #[test]
    fn a_cached_command_wrapper_title_is_replaced_by_the_parsed_prompt() {
        let root = tempdir().unwrap();
        let codex_sessions = root.path().join("codex");
        fs::create_dir_all(&codex_sessions).unwrap();
        fs::write(
            codex_sessions.join("rollout-wrapped.jsonl"),
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"actual-id\",\"cwd\":\"/tmp\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"Audit the keybindings\"}}\n"
            ),
        )
        .unwrap();

        let mut app = App::test_fixture();
        let mut cached = fixture_session("codex:actual-id");
        // Written by a build that still accepted provider command wrappers.
        cached.summary.task = "<user_shell_command>\n<command>\ngit status\n</command>".into();
        cached.transcript_fingerprint = "stale".into();
        app.runtime.sessions = vec![cached];
        app.runtime.selected = 0;
        app.runtime.discovery_paths = DiscoveryPaths {
            codex_sessions,
            claude_projects: root.path().join("claude"),
            pi_sessions: PathBuf::new(),
        };

        app.runtime.refresh_now(&HashSet::new());

        assert_eq!(
            app.session_title(&app.runtime.sessions[0]),
            "Audit the keybindings",
            "a cached wrapper title must not survive a refresh"
        );
    }

    fn fixture_session(key: &str) -> Session {
        Session {
            key: key.into(),
            provider_session_id: key.split(':').nth(1).unwrap().into(),
            name: key.into(),
            search_terms: Vec::new(),
            first_prompt: None,
            agent: AgentKind::Codex,
            status: SessionStatus::Idle,
            cwd: "/tmp".into(),
            branch: None,
            transcript_path: None,
            transcript_modified_at: 0,
            transcript_fingerprint: "new".into(),
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
}
