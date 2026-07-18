use std::{
    collections::HashMap,
    env, fs, io,
    io::Write,
    path::{Path, PathBuf},
};

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::model::{Session, SessionSummary};

const STATE_VERSION: u32 = 1;
const EVENT_GENERATION_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct CachedSession {
    pub summary: SessionSummary,
    pub summary_fingerprint: String,
    pub summary_updated_at: Option<u64>,
    pub pending_shell_injection: Option<String>,
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub managed_transcript_fingerprint: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PersistedState {
    version: u32,
    sessions: HashMap<String, CachedSession>,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            sessions: HashMap::new(),
        }
    }
}

#[derive(Debug)]
pub struct StateStore {
    pub root: PathBuf,
    state: PersistedState,
    connection: Connection,
    defer_writes_until_clean_exit: bool,
}

impl StateStore {
    pub fn from_environment() -> io::Result<(Self, Option<String>)> {
        let root = state_dir().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "cannot resolve state directory")
        })?;
        Self::load(root)
    }

    pub fn load(root: PathBuf) -> io::Result<(Self, Option<String>)> {
        ensure_private_dir(&root)?;
        let database_path = root.join("state.db");
        let connection = Connection::open(&database_path).map_err(io::Error::other)?;
        make_private_file(&database_path)?;
        initialize_database(&connection)?;
        let database_state = load_database_state(&connection)?;
        let migration_complete = database_meta(&connection, "legacy_state_imported")?.is_some();
        let path = root.join("state.json");
        if migration_complete || !database_state.sessions.is_empty() || !path.exists() {
            if !migration_complete {
                set_database_meta(&connection, "legacy_state_imported", "1")?;
            }
            return Ok((
                Self {
                    root,
                    state: database_state,
                    connection,
                    defer_writes_until_clean_exit: false,
                },
                None,
            ));
        }

        let bytes = fs::read(&path)?;
        match serde_json::from_slice::<PersistedState>(&bytes) {
            Ok(state) if state.version == STATE_VERSION => Ok((
                {
                    let store = Self {
                        root,
                        state,
                        connection,
                        defer_writes_until_clean_exit: false,
                    };
                    store.write()?;
                    set_database_meta(&store.connection, "legacy_state_imported", "1")?;
                    store
                },
                None,
            )),
            Ok(mut state) => {
                state.version = STATE_VERSION;
                Ok((
                    Self {
                        root,
                        state,
                        connection,
                        defer_writes_until_clean_exit: true,
                    },
                    Some(
                        "state cache has an unsupported version; it will be replaced on clean exit"
                            .into(),
                    ),
                ))
            }
            Err(error) => Ok((
                Self {
                    root,
                    state: PersistedState::default(),
                    connection,
                    defer_writes_until_clean_exit: true,
                },
                Some(format!(
                    "state cache is malformed; ignored until clean exit: {error}"
                )),
            )),
        }
    }

    pub fn apply(&self, session: &mut Session) {
        let Some(cached) = self.state.sessions.get(&session.key) else {
            return;
        };
        session.summary.clone_from(&cached.summary);
        session
            .summary_fingerprint
            .clone_from(&cached.summary_fingerprint);
        session.summary_updated_at = cached.summary_updated_at;
        session
            .pending_shell_injection
            .clone_from(&cached.pending_shell_injection);
        session.apply_deterministic_status(
            false,
            session.status == crate::model::SessionStatus::Failed,
        );
    }

    pub fn update(&mut self, session: &Session) {
        let metadata = self
            .state
            .sessions
            .get(&session.key)
            .map(|cached| {
                (
                    cached.alias.clone(),
                    cached.archived,
                    cached.profile.clone(),
                    cached.managed_transcript_fingerprint.clone(),
                )
            })
            .unwrap_or_default();
        self.state.sessions.insert(
            session.key.clone(),
            CachedSession {
                summary: session.summary.clone(),
                summary_fingerprint: session.summary_fingerprint.clone(),
                summary_updated_at: session.summary_updated_at,
                pending_shell_injection: session.pending_shell_injection.clone(),
                alias: metadata.0,
                archived: metadata.1,
                profile: metadata.2,
                managed_transcript_fingerprint: metadata.3,
            },
        );
    }

    pub fn alias(&self, key: &str) -> Option<&str> {
        self.state
            .sessions
            .get(key)
            .and_then(|cached| cached.alias.as_deref())
    }

    pub fn set_alias(&mut self, key: &str, alias: Option<String>) {
        self.state.sessions.entry(key.to_owned()).or_default().alias = alias;
    }

    pub fn archived(&self, key: &str) -> bool {
        self.state
            .sessions
            .get(key)
            .is_some_and(|cached| cached.archived)
    }

    pub fn toggle_archived(&mut self, key: &str) -> bool {
        let cached = self.state.sessions.entry(key.to_owned()).or_default();
        cached.archived = !cached.archived;
        cached.archived
    }

    pub fn rekey(&mut self, old_key: &str, new_key: &str) {
        if old_key != new_key
            && let Some(old) = self.state.sessions.remove(old_key)
        {
            self.state.sessions.entry(new_key.to_owned()).or_insert(old);
        }
    }

    pub fn profile(&self, key: &str) -> Option<&str> {
        self.state
            .sessions
            .get(key)
            .and_then(|cached| cached.profile.as_deref())
    }

    pub fn set_profile(&mut self, key: &str, profile: Option<String>) {
        self.state
            .sessions
            .entry(key.to_owned())
            .or_default()
            .profile = profile;
    }

    pub fn managed_transcript_fingerprint(&self, key: &str) -> Option<&str> {
        self.state
            .sessions
            .get(key)
            .and_then(|cached| cached.managed_transcript_fingerprint.as_deref())
    }

    pub fn set_managed_transcript_fingerprint(&mut self, key: &str, fingerprint: Option<String>) {
        self.state
            .sessions
            .entry(key.to_owned())
            .or_default()
            .managed_transcript_fingerprint = fingerprint;
    }

    pub fn save_incremental(&self) -> io::Result<()> {
        if self.defer_writes_until_clean_exit {
            return Ok(());
        }
        self.write()
    }

    pub fn save_clean_exit(&mut self) -> io::Result<()> {
        self.defer_writes_until_clean_exit = false;
        self.write()?;
        set_database_meta(&self.connection, "legacy_state_imported", "1")
    }

    pub fn schema_path(&self) -> PathBuf {
        self.root.join("summary-schema.json")
    }

    pub fn events_dir(&self) -> PathBuf {
        self.root.join("events")
    }

    fn write(&self) -> io::Result<()> {
        ensure_private_dir(&self.root)?;
        self.connection
            .execute_batch("BEGIN IMMEDIATE; DELETE FROM session_cache;")
            .map_err(io::Error::other)?;
        let result = self.state.sessions.iter().try_for_each(|(key, cached)| {
            let data = serde_json::to_string(cached).map_err(io::Error::other)?;
            self.connection
                .execute(
                    "INSERT INTO session_cache(session_key, data_json) VALUES (?1, ?2)",
                    params![key, data],
                )
                .map_err(io::Error::other)?;
            Ok::<_, io::Error>(())
        });
        match result {
            Ok(()) => self
                .connection
                .execute_batch("COMMIT")
                .map_err(io::Error::other),
            Err(error) => {
                let _ = self.connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }?;
        make_private_file(&self.root.join("state.db"))?;
        for suffix in ["state.db-wal", "state.db-shm"] {
            let path = self.root.join(suffix);
            if path.exists() {
                make_private_file(&path)?;
            }
        }
        Ok(())
    }
}

fn initialize_database(connection: &Connection) -> io::Result<()> {
    connection
        .execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=2000;
             PRAGMA user_version=1;
             CREATE TABLE IF NOT EXISTS session_cache (
                 session_key TEXT PRIMARY KEY,
                 data_json TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS app_meta (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )
        .map_err(io::Error::other)
}

fn load_database_state(connection: &Connection) -> io::Result<PersistedState> {
    let mut statement = connection
        .prepare("SELECT session_key, data_json FROM session_cache")
        .map_err(io::Error::other)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(io::Error::other)?;
    let mut state = PersistedState::default();
    for row in rows {
        let (key, data) = row.map_err(io::Error::other)?;
        let cached = serde_json::from_str(&data).map_err(io::Error::other)?;
        state.sessions.insert(key, cached);
    }
    Ok(state)
}

fn database_meta(connection: &Connection, key: &str) -> io::Result<Option<String>> {
    match connection.query_row("SELECT value FROM app_meta WHERE key = ?1", [key], |row| {
        row.get(0)
    }) {
        Ok(value) => Ok(Some(value)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(io::Error::other(error)),
    }
}

fn set_database_meta(connection: &Connection, key: &str, value: &str) -> io::Result<()> {
    connection
        .execute(
            "INSERT INTO app_meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key, value],
        )
        .map(|_| ())
        .map_err(io::Error::other)
}

pub fn state_dir() -> Option<PathBuf> {
    if let Some(path) = env::var_os("AGENT_CONSOLE_STATE_DIR") {
        return Some(PathBuf::from(path));
    }
    if let Some(path) = env::var_os("XDG_STATE_HOME") {
        return Some(PathBuf::from(path).join("agent-console"));
    }
    dirs::home_dir().map(|home| home.join(".local/state/agent-console"))
}

pub fn atomic_append_jsonl(path: &Path, line: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let mut record = Vec::with_capacity(line.len() + 1);
    record.extend_from_slice(line);
    if !record.ends_with(b"\n") {
        record.push(b'\n');
    }
    rotate_jsonl_if_full(path, record.len())?;
    let mut options = fs::OpenOptions::new();
    options.create(true).append(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    make_private_file(path)?;
    file.write_all(&record)
}

pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn make_private_file(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    make_private_file(path)?;
    file.write_all(bytes)
}

pub fn rotated_jsonl_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".1");
    PathBuf::from(value)
}

fn rotate_jsonl_if_full(path: &Path, incoming: usize) -> io::Result<()> {
    let Ok(metadata) = fs::metadata(path) else {
        return Ok(());
    };
    if metadata.len() as usize + incoming <= EVENT_GENERATION_BYTES {
        return Ok(());
    }
    let archive = rotated_jsonl_path(path);
    match fs::remove_file(&archive) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::rename(path, &archive)?;
    make_private_file(&archive)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::model::{AgentKind, SessionStatus};

    fn session() -> Session {
        Session {
            key: "claude:id".into(),
            provider_session_id: "id".into(),
            name: "repo".into(),
            agent: AgentKind::Claude,
            status: SessionStatus::Idle,
            cwd: "/tmp".into(),
            branch: None,
            transcript_path: None,
            transcript_modified_at: 1,
            transcript_fingerprint: "1:2".into(),
            summary_fingerprint: "1:2".into(),
            summary_updated_at: Some(3),
            summary_error: None,
            summary: SessionSummary {
                task: "test persistence".into(),
                ..SessionSummary::default()
            },
            recent_activity: Vec::new(),
            pending_decisions: Vec::new(),
            pending_shell_injection: Some("captured".into()),
            managed_alive: false,
            unavailable_reason: None,
            discovered_after_startup: false,
        }
    }

    #[test]
    fn state_round_trip() {
        let root = tempdir().unwrap();
        let (mut store, warning) = StateStore::load(root.path().to_owned()).unwrap();
        assert!(warning.is_none());
        store.update(&session());
        store.set_alias("claude:id", Some("release blocker".into()));
        assert!(store.toggle_archived("claude:id"));
        store.set_profile("claude:id", Some("work".into()));
        store.set_managed_transcript_fingerprint("claude:id", Some("mtime:length".into()));
        store.save_clean_exit().unwrap();
        assert!(root.path().join("state.db").is_file());
        assert!(!root.path().join("state.json").exists());

        let (loaded, warning) = StateStore::load(root.path().to_owned()).unwrap();
        assert!(warning.is_none());
        let mut restored = session();
        restored.summary = SessionSummary::default();
        restored.pending_shell_injection = None;
        loaded.apply(&mut restored);
        assert_eq!(restored.summary.task, "test persistence");
        assert_eq!(
            restored.pending_shell_injection.as_deref(),
            Some("captured")
        );
        assert_eq!(loaded.alias("claude:id"), Some("release blocker"));
        assert!(loaded.archived("claude:id"));
        assert_eq!(loaded.profile("claude:id"), Some("work"));
        assert_eq!(
            loaded.managed_transcript_fingerprint("claude:id"),
            Some("mtime:length")
        );
    }

    #[test]
    fn legacy_json_state_is_imported_once_into_sqlite() {
        let root = tempdir().unwrap();
        let mut legacy = PersistedState::default();
        legacy.sessions.insert(
            "claude:id".into(),
            CachedSession {
                alias: Some("legacy alias".into()),
                ..CachedSession::default()
            },
        );
        fs::write(
            root.path().join("state.json"),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();

        let (store, warning) = StateStore::load(root.path().to_owned()).unwrap();
        assert!(warning.is_none());
        assert_eq!(store.alias("claude:id"), Some("legacy alias"));
        assert!(root.path().join("state.db").is_file());

        fs::write(root.path().join("state.json"), b"now malformed").unwrap();
        let (reloaded, warning) = StateStore::load(root.path().to_owned()).unwrap();
        assert!(warning.is_none());
        assert_eq!(reloaded.alias("claude:id"), Some("legacy alias"));
    }

    #[test]
    fn malformed_state_is_deferred_then_recovered_on_clean_exit() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("state.json"), b"not json").unwrap();
        let (mut store, warning) = StateStore::load(root.path().to_owned()).unwrap();
        assert!(warning.is_some());
        store.save_incremental().unwrap();
        assert_eq!(
            fs::read(root.path().join("state.json")).unwrap(),
            b"not json"
        );
        store.save_clean_exit().unwrap();
        let (reloaded, warning) = StateStore::load(root.path().to_owned()).unwrap();
        assert!(warning.is_none());
        assert!(reloaded.state.sessions.is_empty());
    }

    #[test]
    fn event_append_is_private_and_retains_a_bounded_valid_tail() {
        let root = tempdir().unwrap();
        let path = root.path().join("private/events/test.jsonl");
        let payload = vec![b'x'; 1024];
        for _ in 0..700 {
            atomic_append_jsonl(&path, &payload).unwrap();
        }
        let bytes = fs::read(&path).unwrap();
        let archive = rotated_jsonl_path(&path);
        let archived = fs::read(&archive).unwrap();
        assert!(bytes.len() <= EVENT_GENERATION_BYTES);
        assert!(archived.len() <= EVENT_GENERATION_BYTES);
        assert!(bytes.len() + archived.len() <= EVENT_GENERATION_BYTES * 2);
        assert!(bytes.ends_with(b"\n"));
        for generation in [&bytes, &archived] {
            assert!(generation.split(|byte| *byte == b'\n').all(|line| {
                line.is_empty()
                    || (line.len() == payload.len() && line.iter().all(|byte| *byte == b'x'))
            }));
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
