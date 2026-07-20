use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs, io,
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    time::{Duration, Instant, UNIX_EPOCH},
};

use uuid::Uuid;

use crate::{
    clipboard,
    config::AgentConsoleConfig,
    discovery::{self, DiscoveryCache, DiscoveryPaths},
    events::{self, NormalizedEvent},
    model::{AgentKind, Session, SessionStatus, SessionSummary, unix_timestamp},
    pty::{
        TerminalManager, WorkspaceChrome, WorkspaceExit, WorkspaceFocus, bracketed_paste,
        staged_shell_text,
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
    pub session_key: String,
    pub status: SessionStatus,
    pub message: String,
    read: bool,
}

pub struct RuntimeState {
    pub sessions: Vec<Session>,
    pub selected: usize,
    pub tick_count: u64,
    pub banner: Option<String>,
    pub summary_busy: Option<String>,
    discovery_paths: DiscoveryPaths,
    discovery_cache: DiscoveryCache,
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
    filter: SessionFilter,
    store: StateStore,
    event_index: events::EventIndex,
    summary_worker: SummaryWorker,
}

pub struct App {
    runtime: RuntimeState,
    pub dialog: Option<NewSessionDialog>,
    pub text_dialog: Option<TextDialog>,
    pub help_open: bool,
    startup_cwd: PathBuf,
    pub terminals: TerminalManager,
    config: AgentConsoleConfig,
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
                discovery_cache,
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

    pub fn help_lines(&self) -> Vec<String> {
        self.config.help_bindings()
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

    pub fn session_shell_count(&self, session: &Session) -> usize {
        self.terminals.shell_count(&session.key)
    }

    pub fn filter_summary(&self) -> String {
        self.runtime.filter_summary()
    }

    fn normalize_selection(&mut self) {
        let order = self.runtime.session_display_order();
        if !order.contains(&self.runtime.selected)
            && let Some(first) = order.first()
        {
            self.runtime.selected = *first;
        }
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

    pub fn enter_selected_agent(&mut self, current_exe: &Path) -> io::Result<()> {
        self.enter_selected_agent_with_takeover(current_exe, false)
    }

    pub fn force_enter_selected_agent(&mut self, current_exe: &Path) -> io::Result<()> {
        self.enter_selected_agent_with_takeover(current_exe, true)
    }

    fn enter_selected_agent_with_takeover(
        &mut self,
        current_exe: &Path,
        force_takeover: bool,
    ) -> io::Result<()> {
        let session = self.prepare_agent_entry(current_exe)?;
        let touched =
            self.run_workspace(current_exe, session, WorkspaceFocus::Agent, force_takeover)?;
        self.finish_workspace(&touched)?;
        Ok(())
    }

    fn prepare_agent_entry(&mut self, current_exe: &Path) -> io::Result<Session> {
        self.activate_workspace_agent(current_exe)
    }

    pub fn enter_selected_shell(&mut self, current_exe: &Path) -> io::Result<()> {
        let session = self.prepare_selected_view(current_exe)?;
        if !session.cwd.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "session working directory does not exist",
            ));
        }
        let size = crossterm::terminal::size().unwrap_or((80, 24));
        self.terminals.add_shell(&session, size)?;
        let touched = self.run_workspace(current_exe, session, WorkspaceFocus::Shell, false)?;
        self.finish_workspace(&touched)?;
        Ok(())
    }

    fn run_workspace(
        &mut self,
        current_exe: &Path,
        mut session: Session,
        mut focus: WorkspaceFocus,
        mut force_takeover: bool,
    ) -> io::Result<HashSet<String>> {
        let mut touched_agents = HashSet::new();
        if focus == WorkspaceFocus::Agent {
            touched_agents.insert(session.key.clone());
        }
        loop {
            match self.attach_workspace(&session, focus, force_takeover)? {
                WorkspaceExit::Dashboard => return Ok(touched_agents),
                WorkspaceExit::Alert => {
                    self.runtime.jump_to_next_notification();
                    return Ok(touched_agents);
                }
                WorkspaceExit::ActivateSession => {
                    match self.activate_workspace_agent(current_exe) {
                        Ok(selected) => {
                            session = selected;
                            focus = WorkspaceFocus::Agent;
                            touched_agents.insert(session.key.clone());
                        }
                        Err(error) => {
                            session = self.prepare_selected_view(current_exe)?;
                            self.terminals
                                .set_notice(&session.key, format!("cannot open agent: {error}"));
                            focus = WorkspaceFocus::Sessions;
                        }
                    }
                }
                WorkspaceExit::FocusShell => {
                    session = self.prepare_selected_view(current_exe)?;
                    focus = WorkspaceFocus::Shell;
                }
                WorkspaceExit::NewSession => {
                    self.open_new_dialog_at(&session.cwd);
                    return Ok(touched_agents);
                }
                WorkspaceExit::OpenShell => {
                    session = self.prepare_selected_view(current_exe)?;
                    if !session.cwd.is_dir() {
                        self.terminals.set_notice(
                            &session.key,
                            "cannot open shell: working directory no longer exists".into(),
                        );
                        focus = WorkspaceFocus::Sessions;
                    } else {
                        let size = crossterm::terminal::size().unwrap_or((80, 24));
                        match self.terminals.add_shell(&session, size) {
                            Ok(_) => focus = WorkspaceFocus::Shell,
                            Err(error) => {
                                self.terminals.set_notice(
                                    &session.key,
                                    format!("cannot open shell: {error}"),
                                );
                                focus = WorkspaceFocus::Sessions;
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
                    session = self.prepare_selected_view(current_exe)?;
                    self.terminals.set_notice(&session.key, notice);
                    focus = WorkspaceFocus::Sessions;
                }
                WorkspaceExit::PreviousSession(next_focus) => {
                    self.select_previous();
                    focus = next_focus;
                    session = self.prepare_workspace_target(current_exe, focus)?;
                    if focus == WorkspaceFocus::Agent && self.terminals.agent_alive(&session.key) {
                        touched_agents.insert(session.key.clone());
                    }
                }
                WorkspaceExit::NextSession(next_focus) => {
                    self.select_next();
                    focus = next_focus;
                    session = self.prepare_workspace_target(current_exe, focus)?;
                    if focus == WorkspaceFocus::Agent && self.terminals.agent_alive(&session.key) {
                        touched_agents.insert(session.key.clone());
                    }
                }
            }
            force_takeover = false;
        }
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

    fn attach_workspace(
        &mut self,
        session: &Session,
        focus: WorkspaceFocus,
        force_takeover: bool,
    ) -> io::Result<WorkspaceExit> {
        let mut alive = self
            .terminals
            .alive_keys()
            .into_iter()
            .collect::<HashSet<_>>();
        let mut pending_rekeys = Vec::new();
        let result = self
            .terminals
            .attach_workspace(session, focus, force_takeover, || {
                let rekeys = self.runtime.tick(&alive);
                for (old_key, new_key) in rekeys {
                    if alive.remove(&old_key) {
                        alive.insert(new_key.clone());
                    }
                    pending_rekeys.push((old_key, new_key));
                }
                self.runtime.workspace_chrome()
            });
        for (old_key, new_key) in pending_rekeys {
            self.terminals.rekey(&old_key, new_key);
        }
        result
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
        if self.terminals.agent_alive(&session.key) {
            return Ok(session);
        }
        self.prepare_selected_agent(current_exe)
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
                self.runtime.store.set_managed_transcript_fingerprint(
                    key,
                    Some(session.transcript_fingerprint.clone()),
                );
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
        for (old_key, new_key) in self.runtime.refresh_now(&alive) {
            self.terminals.rekey(&old_key, new_key);
        }
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
        let rekeys = if self.last_discovery.elapsed() >= DISCOVERY_INTERVAL {
            self.refresh_now(alive)
        } else {
            Vec::new()
        };
        if self.last_event_refresh.elapsed() >= EVENT_REFRESH_INTERVAL {
            self.refresh_event_state();
            self.observe_changes();
            self.last_event_refresh = Instant::now();
        }
        self.capture_notifications();
        self.schedule_summary();
        rekeys
    }

    fn refresh_now(&mut self, alive: &HashSet<String>) -> Vec<(String, String)> {
        let mut selected_key = self
            .sessions
            .get(self.selected)
            .map(|session| session.key.clone());
        let old = self
            .sessions
            .drain(..)
            .map(|session| (session.key.clone(), session))
            .collect::<HashMap<_, _>>();
        let discovery_paths = self.discovery_paths.clone();
        let mut discovered =
            discovery::discover_cached(&discovery_paths, &mut self.discovery_cache);
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
                session.summary = previous.summary;
                if session.summary.task.trim().is_empty() {
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

    fn session_is_visible(&self, session: &Session) -> bool {
        if self.filter.query.is_empty() {
            return true;
        }
        let alias = self.store.alias(&session.key).unwrap_or_default();
        let haystack = format!(
            "{alias} {} {} {} {} {} {} {} {}",
            session.summary.task,
            session.name,
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
        haystack.contains(&self.filter.query)
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
        for index in self.session_display_order() {
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
                "{}{} {} {}",
                if archived { "⌁" } else { "" },
                workspace_status_symbol(session.status),
                session.agent.short_label(),
                self.session_title(session)
            ));
        }
        let preview = self
            .sessions
            .get(self.selected)
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
                if !session.summary.task.trim().is_empty() {
                    preview.push(format!("task  {}", session.summary.task.trim()));
                }
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
            status_counts: session_status_counts(&self.sessions),
            preview,
            notification: self.active_notification().map(|notification| {
                let title = self
                    .sessions
                    .iter()
                    .find(|session| session.key == notification.session_key)
                    .map(|session| self.session_title(session))
                    .unwrap_or_else(|| "session".into());
                format!("{title}: {}", notification.message)
            }),
        }
    }

    fn capture_notifications(&mut self) {
        let selected_key = self
            .sessions
            .get(self.selected)
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
            if previous.is_some_and(|previous| previous != status)
                && critical
                && selected_key.as_deref() != Some(&key)
            {
                self.notifications.push_back(RuntimeNotification {
                    session_key: key,
                    status,
                    message,
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
        let session = self
            .sessions
            .get(self.selected)
            .ok_or_else(|| "no selected session".to_owned())?;
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
                },
                discovery_cache: DiscoveryCache::default(),
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
                },
                discovery_cache: DiscoveryCache::default(),
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
        };
        // Provisional sessions are retained by refresh.
        app.refresh_now();
        assert_eq!(app.selected_session().unwrap().key, "codex:two");
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
    fn workspace_sidebar_groups_sessions_and_uses_task_titles() {
        let mut app = App::test_fixture();
        app.sessions[0].summary.task = "refresh tokens".into();
        app.sessions[0].status = SessionStatus::Working;
        let mut same_workspace = app.sessions[0].clone();
        same_workspace.key = "claude:timeout".into();
        same_workspace.agent = AgentKind::Claude;
        same_workspace.status = SessionStatus::Waiting;
        same_workspace.summary.task = "fix timeout".into();
        let mut other_workspace = app.sessions[0].clone();
        other_workspace.key = "codex:frontend".into();
        other_workspace.name = "frontend".into();
        other_workspace.cwd = "/tmp/frontend".into();
        other_workspace.status = SessionStatus::Failed;
        other_workspace.summary.task = "update navbar".into();
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
        app.sessions.push(claude);

        app.open_search_dialog();
        app.text_dialog.as_mut().unwrap().value = "latency".into();
        app.commit_text_dialog().unwrap();
        assert_eq!(app.session_display_order(), vec![1]);
        assert_eq!(app.selected, 1);

        for query in ["claude", "other", "idle"] {
            app.open_search_dialog();
            app.text_dialog.as_mut().unwrap().value = query.into();
            app.commit_text_dialog().unwrap();
            assert_eq!(app.session_display_order(), vec![1]);
        }
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

    fn fixture_session(key: &str) -> Session {
        Session {
            key: key.into(),
            provider_session_id: key.split(':').nth(1).unwrap().into(),
            name: key.into(),
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
