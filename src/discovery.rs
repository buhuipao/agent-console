use std::{
    collections::{HashMap, HashSet, VecDeque},
    env, fs,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, UNIX_EPOCH},
};

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::{
    model::{AgentKind, Session, SessionStatus, SessionSummary},
    providers::{self, ProviderAdapter},
};

const MAX_PROVIDER_FILES: usize = 60;
const MAX_VISIBLE_SESSIONS: usize = 50;
const RECENT_SESSION_SECONDS: u64 = 7 * 24 * 60 * 60;
const HEAD_BYTES: usize = 64 * 1024;
const TAIL_BYTES: usize = 128 * 1024;
const MAX_ACTIVITY: usize = 12;

#[derive(Clone, Debug)]
pub struct DiscoveryPaths {
    pub codex_sessions: PathBuf,
    pub claude_projects: PathBuf,
    pub pi_sessions: PathBuf,
}

#[derive(Default)]
pub struct DiscoveryCache {
    entries: HashMap<PathBuf, (String, Option<Session>)>,
    codex_threads: Option<CodexThreadCache>,
}

#[derive(Clone, Default)]
struct CodexThreadMetadata {
    name: Option<String>,
    search_terms: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileFingerprint {
    modified_nanos: u128,
    len: u64,
}

#[derive(Clone)]
struct CodexThreadCache {
    database_path: PathBuf,
    database_fingerprint: Option<FileFingerprint>,
    wal_fingerprint: Option<FileFingerprint>,
    metadata: HashMap<String, CodexThreadMetadata>,
}

impl DiscoveryPaths {
    pub fn from_environment() -> Option<Self> {
        let home = dirs::home_dir()?;
        let codex_home = env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"));
        // pi reads the same variable to decide where its own agent directory lives, so a user
        // who moved it keeps a dashboard that finds their sessions.
        let pi_home = env::var_os("PI_CODING_AGENT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".pi/agent"));
        Some(Self {
            codex_sessions: codex_home.join("sessions"),
            claude_projects: home.join(".claude/projects"),
            pi_sessions: pi_home.join("sessions"),
        })
    }

    pub fn root(&self, provider: AgentKind) -> &Path {
        match provider {
            AgentKind::Codex => &self.codex_sessions,
            AgentKind::Claude => &self.claude_projects,
            AgentKind::Pi => &self.pi_sessions,
        }
    }
}

#[cfg(test)]
pub fn discover(paths: &DiscoveryPaths) -> Vec<Session> {
    discover_cached(paths, &mut DiscoveryCache::default())
}

pub fn discover_cached(paths: &DiscoveryPaths, cache: &mut DiscoveryCache) -> Vec<Session> {
    let mut sessions = Vec::new();
    let mut seen = HashSet::new();
    for adapter in providers::enabled() {
        let root = paths.root(adapter.kind);
        let files = provider_files(root, adapter.accepts);
        seen.extend(files.iter().cloned());
        let mut parsed = parse_cached_files(files, adapter, cache);
        if let Some(enrich) = adapter.enrich {
            enrich(root, &mut parsed, cache);
        }
        sessions.extend(parsed);
    }
    cache.entries.retain(|path, _| seen.contains(path));
    sessions.sort_by(|left, right| {
        right
            .transcript_modified_at
            .cmp(&left.transcript_modified_at)
            .then_with(|| left.key.cmp(&right.key))
    });
    let cutoff = crate::model::unix_timestamp().saturating_sub(RECENT_SESSION_SECONDS);
    sessions.retain(|session| session.transcript_modified_at >= cutoff);
    sessions.truncate(MAX_VISIBLE_SESSIONS);
    sessions
}

/// Apply the optional Codex thread database over parsed Codex sessions. The
/// database is read-only enrichment; an absent or incompatible one must not
/// hide transcript sessions.
pub(crate) fn enrich_codex(
    sessions_root: &Path,
    sessions: &mut [Session],
    cache: &mut DiscoveryCache,
) {
    let metadata = load_codex_thread_metadata(sessions_root, cache);
    if metadata.is_empty() {
        return;
    }
    for session in sessions {
        let Some(metadata) = metadata.get(&session.provider_session_id) else {
            continue;
        };
        if let Some(name) = metadata
            .name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
        {
            session.name = name.to_owned();
        }
        for term in &metadata.search_terms {
            push_search_term(&mut session.search_terms, term.clone());
        }
    }
}

fn load_codex_thread_metadata(
    sessions_root: &Path,
    cache: &mut DiscoveryCache,
) -> HashMap<String, CodexThreadMetadata> {
    let Some(codex_home) = sessions_root.parent() else {
        return HashMap::new();
    };
    let Some(database_path) = fs::read_dir(codex_home)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            let version = name
                .strip_prefix("state_")?
                .strip_suffix(".sqlite")?
                .parse::<u32>()
                .ok()?;
            Some((version, entry.path()))
        })
        .max_by_key(|(version, _)| *version)
        .map(|(_, path)| path)
    else {
        return cache
            .codex_threads
            .as_ref()
            .map(|cached| cached.metadata.clone())
            .unwrap_or_default();
    };
    let database_fingerprint = file_fingerprint(&database_path);
    let wal_fingerprint = file_fingerprint(&database_path.with_extension("sqlite-wal"));
    if let Some(cached) = cache.codex_threads.as_ref()
        && cached.database_path == database_path
        && cached.database_fingerprint == database_fingerprint
        && cached.wal_fingerprint == wal_fingerprint
    {
        return cached.metadata.clone();
    }

    let loaded = query_codex_thread_metadata(&database_path);
    match loaded {
        Ok(metadata) => {
            cache.codex_threads = Some(CodexThreadCache {
                database_path,
                database_fingerprint,
                wal_fingerprint,
                metadata: metadata.clone(),
            });
            metadata
        }
        Err(_) => cache
            .codex_threads
            .as_ref()
            .map(|cached| cached.metadata.clone())
            .unwrap_or_default(),
    }
}

fn query_codex_thread_metadata(
    database_path: &Path,
) -> rusqlite::Result<HashMap<String, CodexThreadMetadata>> {
    let connection = Connection::open_with_flags(database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(Duration::from_millis(25))?;
    let mut statement =
        connection.prepare("SELECT id, name, title, first_user_message, preview FROM threads")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    })?;
    let rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .map(|(id, name, title, first_prompt, preview)| {
            let mut metadata = CodexThreadMetadata {
                name,
                ..CodexThreadMetadata::default()
            };
            for term in [metadata.name.clone(), title, first_prompt, preview]
                .into_iter()
                .flatten()
            {
                push_search_term(&mut metadata.search_terms, term);
            }
            (id, metadata)
        })
        .collect())
}

#[cfg(test)]
fn discover_codex(root: &Path) -> Vec<Session> {
    let adapter = providers::adapter(AgentKind::Codex);
    provider_files(root, adapter.accepts)
        .into_iter()
        .filter_map(|path| (adapter.parse)(&path, None).ok().flatten())
        .collect()
}

fn provider_files(root: &Path, accept: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_files(root, &accept, &mut files);
    files.sort_by_key(|path| std::cmp::Reverse(file_modified(path)));
    files.truncate(MAX_PROVIDER_FILES);
    files
}

fn parse_cached_files(
    files: Vec<PathBuf>,
    adapter: &ProviderAdapter,
    cache: &mut DiscoveryCache,
) -> Vec<Session> {
    files
        .into_iter()
        .filter_map(|path| {
            let fingerprint = transcript_fingerprint(&path)?;
            if let Some((cached_fingerprint, session)) = cache.entries.get(&path)
                && cached_fingerprint == &fingerprint
            {
                return session.clone();
            }
            let cached_prompt = cache
                .entries
                .get(&path)
                .and_then(|(_, session)| session.as_ref())
                .and_then(|session| {
                    session
                        .first_prompt
                        .as_deref()
                        .map(|prompt| (session.provider_session_id.as_str(), prompt))
                });
            let parsed = (adapter.parse)(&path, cached_prompt).ok()?;
            cache.entries.insert(path, (fingerprint, parsed.clone()));
            parsed
        })
        .collect()
}

pub(crate) fn transcript_fingerprint(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(format!("{modified}:{}", metadata.len()))
}

fn file_fingerprint(path: &Path) -> Option<FileFingerprint> {
    let metadata = fs::metadata(path).ok()?;
    let modified_nanos = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(FileFingerprint {
        modified_nanos,
        len: metadata.len(),
    })
}

fn collect_files(root: &Path, accept: &impl Fn(&Path) -> bool, output: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            collect_files(&path, accept, output);
        } else if kind.is_file() && accept(&path) {
            output.push(path);
        }
    }
}

#[cfg(test)]
pub(crate) fn parse_codex(path: &Path) -> io::Result<Option<Session>> {
    parse_codex_with_cached_prompt(path, None)
}

pub(crate) fn parse_codex_with_cached_prompt(
    path: &Path,
    cached_prompt: Option<(&str, &str)>,
) -> io::Result<Option<Session>> {
    let (lines, fingerprint, modified_at) = bounded_jsonl(path)?;
    let mut id = None;
    let mut cwd = None;
    let mut branch = None;
    let mut provider_name = None;
    let mut first_prompt = None;
    let mut search_terms = Vec::new();
    let mut task = String::new();
    let mut activity = VecDeque::new();
    let mut failed = false;

    for line in lines {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let record_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        let payload = value.get("payload").unwrap_or(&Value::Null);
        let payload_type = payload.get("type").and_then(Value::as_str).unwrap_or("");

        if record_type == "session_meta" {
            if id.is_none() {
                id = raw_string_field(payload, "id");
                let is_subagent = payload.get("thread_source").and_then(Value::as_str)
                    == Some("subagent")
                    || payload
                        .get("source")
                        .and_then(|source| source.get("subagent"))
                        .is_some();
                if is_subagent {
                    return Ok(None);
                }
            }
            provider_name = raw_string_field(payload, "name").or(provider_name);
            for field in ["title", "first_user_message", "preview"] {
                if let Some(value) = raw_string_field(payload, field) {
                    push_search_term(&mut search_terms, value);
                }
            }
            cwd = raw_string_field(payload, "cwd").map(PathBuf::from).or(cwd);
        }
        if record_type == "turn_context" {
            cwd = raw_string_field(payload, "cwd").map(PathBuf::from).or(cwd);
        }

        match (record_type, payload_type) {
            ("event_msg", "user_message") => {
                if let Some(text) =
                    string_field(payload, "message").and_then(|text| user_prompt_text(&text))
                {
                    first_prompt.get_or_insert_with(|| text.clone());
                    task.clone_from(&text);
                    push_activity(&mut activity, format!("You: {text}"));
                }
            }
            ("event_msg", "agent_message") => {
                if let Some(text) = string_field(payload, "message") {
                    push_activity(&mut activity, format!("Agent: {text}"));
                }
            }
            ("event_msg", "task_started") => {
                push_activity(&mut activity, "Turn started".to_owned());
            }
            ("event_msg", "task_complete") => {
                if let Some(text) = string_field(payload, "last_agent_message") {
                    push_activity(&mut activity, format!("Agent: {text}"));
                }
            }
            ("event_msg", "turn_aborted") | ("event_msg", "stream_error") => {
                failed = true;
                push_activity(&mut activity, "Turn failed".to_owned());
            }
            ("response_item", "message") => {
                let text = content_text(payload.get("content"));
                let role = payload
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("agent");
                if !text.is_empty() {
                    if role == "user" {
                        if let Some(text) = user_prompt_text(&text) {
                            first_prompt.get_or_insert_with(|| text.clone());
                            task.clone_from(&text);
                            push_activity(&mut activity, format!("You: {text}"));
                        }
                    } else {
                        push_activity(&mut activity, format!("Agent: {text}"));
                    }
                }
            }
            ("response_item", "function_call") | ("response_item", "custom_tool_call") => {
                let name = string_field(payload, "name")
                    .or_else(|| string_field(payload, "tool"))
                    .unwrap_or_else(|| "tool".to_owned());
                push_activity(&mut activity, format!("Tool: {name}"));
            }
            _ => {}
        }

        if branch.is_none() {
            branch = raw_string_field(payload, "branch");
        }
    }

    let (Some(id), Some(cwd)) = (id, cwd) else {
        return Ok(None);
    };
    if let Some((_, prompt)) =
        cached_prompt.filter(|(provider_session_id, _)| *provider_session_id == id)
    {
        first_prompt = Some(prompt.to_owned());
    } else if let Some(prompt) = first_prompt_in_jsonl(path, codex_prompt_from_record)? {
        first_prompt = Some(prompt);
    }
    if let Some(value) = provider_name.clone() {
        push_search_term(&mut search_terms, value);
    }
    if let Some(value) = first_prompt.clone() {
        push_search_term(&mut search_terms, value);
    }
    let name = provider_name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| directory_name(&cwd));
    let mut summary = SessionSummary {
        task: task.clone(),
        current_action: activity.back().cloned().unwrap_or_default(),
        ..SessionSummary::default()
    };
    let status = if failed {
        SessionStatus::Failed
    } else {
        SessionStatus::Idle
    };
    summary.status = status;

    Ok(Some(Session {
        key: Session::stable_key(AgentKind::Codex, &id),
        provider_session_id: id,
        name,
        search_terms,
        first_prompt,
        agent: AgentKind::Codex,
        status,
        cwd: cwd.clone(),
        branch,
        transcript_path: Some(path.to_owned()),
        transcript_modified_at: modified_at,
        transcript_fingerprint: fingerprint,
        summary_fingerprint: String::new(),
        summary_updated_at: None,
        summary_error: None,
        summary,
        recent_activity: activity.into_iter().collect(),
        pending_decisions: Vec::new(),
        pending_shell_injection: None,
        managed_alive: false,
        unavailable_reason: (!cwd.is_dir()).then(|| "working directory no longer exists".into()),
        discovered_after_startup: false,
    }))
}

#[cfg(test)]
pub(crate) fn parse_claude(path: &Path) -> io::Result<Option<Session>> {
    parse_claude_with_cached_prompt(path, None)
}

pub(crate) fn parse_claude_with_cached_prompt(
    path: &Path,
    cached_prompt: Option<(&str, &str)>,
) -> io::Result<Option<Session>> {
    let (scanned_first_prompt, is_sidechain) = scan_claude_identity(path)?;
    if is_sidechain {
        return Ok(None);
    }
    let (lines, fingerprint, modified_at) = bounded_jsonl(path)?;
    let mut id = path
        .file_stem()
        .and_then(|value| value.to_str())
        .map(str::to_owned);
    let mut cwd = None;
    let mut branch = None;
    let mut agent_name = None;
    let mut custom_title = None;
    let mut ai_title = None;
    let mut first_prompt = None;
    let mut latest_prompt = None;
    let mut conversation_summary = None;
    let mut tag = None;
    let mut pr_url = None;
    let mut pr_number = None;
    let mut pr_repository = None;
    let mut task = String::new();
    let mut activity = VecDeque::new();
    let mut failed = false;

    for line in lines {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        id = raw_string_field(&value, "sessionId").or(id);
        cwd = raw_string_field(&value, "cwd").map(PathBuf::from).or(cwd);
        branch = raw_string_field(&value, "gitBranch").or(branch);
        let record_type = value.get("type").and_then(Value::as_str).unwrap_or("");

        match record_type {
            "agent-name" => agent_name = string_field(&value, "agentName").or(agent_name),
            "custom-title" => {
                custom_title = string_field(&value, "customTitle")
                    .or_else(|| string_field(&value, "title"))
                    .or(custom_title);
            }
            "ai-title" => {
                ai_title = string_field(&value, "aiTitle")
                    .or_else(|| string_field(&value, "title"))
                    .or(ai_title);
            }
            "last-prompt" => {
                latest_prompt = string_field(&value, "lastPrompt").or(latest_prompt);
            }
            "summary" => {
                conversation_summary = string_field(&value, "summary").or(conversation_summary);
            }
            "tag" => tag = raw_string_field(&value, "tag").or(tag),
            "pr-link" => {
                pr_url = string_field(&value, "prUrl").or(pr_url);
                pr_number = scalar_string_field(&value, "prNumber").or(pr_number);
                pr_repository = string_field(&value, "prRepository").or(pr_repository);
            }
            _ => {}
        }

        if record_type == "user" || record_type == "assistant" {
            let message = value.get("message").unwrap_or(&Value::Null);
            let content = message.get("content");
            let text = content_text(content);
            let has_tool_result = content
                .and_then(Value::as_array)
                .is_some_and(|items| item_type_present(items, "tool_result"));
            let tools = tool_names(content);
            if record_type == "user" && !has_tool_result && !text.is_empty() {
                if let Some(text) = user_prompt_text(&text) {
                    first_prompt.get_or_insert_with(|| text.clone());
                    latest_prompt = Some(text.clone());
                    task.clone_from(&text);
                    push_activity(&mut activity, format!("You: {text}"));
                }
            } else if record_type == "assistant" && !text.is_empty() {
                push_activity(&mut activity, format!("Agent: {text}"));
            } else if has_tool_result && !text.is_empty() {
                push_activity(&mut activity, format!("Tool result: {text}"));
            }
            for tool in tools {
                push_activity(&mut activity, format!("Tool: {tool}"));
            }
        }

        if record_type == "system"
            && value.get("subtype").and_then(Value::as_str) == Some("api_error")
        {
            failed = true;
            push_activity(&mut activity, "API request failed".to_owned());
        }
    }

    let (Some(id), Some(cwd)) = (id, cwd) else {
        return Ok(None);
    };
    if is_claude_mem_observer(&cwd) {
        return Ok(None);
    }
    if let Some((_, prompt)) =
        cached_prompt.filter(|(provider_session_id, _)| *provider_session_id == id)
    {
        first_prompt = Some(prompt.to_owned());
    } else if let Some(prompt) = scanned_first_prompt {
        first_prompt = Some(prompt);
    }
    let mut search_terms = Vec::new();
    for term in [
        agent_name.clone(),
        custom_title.clone(),
        ai_title.clone(),
        first_prompt.clone(),
        latest_prompt,
        conversation_summary,
        tag,
        pr_url,
        pr_repository,
    ]
    .into_iter()
    .flatten()
    {
        push_search_term(&mut search_terms, term);
    }
    if let Some(number) = pr_number {
        push_search_term(&mut search_terms, number.clone());
        push_search_term(&mut search_terms, format!("pr #{number}"));
    }
    let name = agent_name
        .or(custom_title)
        .or(ai_title)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| directory_name(&cwd));
    let status = if failed {
        SessionStatus::Failed
    } else {
        SessionStatus::Idle
    };
    let summary = SessionSummary {
        task,
        status,
        current_action: activity.back().cloned().unwrap_or_default(),
        ..SessionSummary::default()
    };

    Ok(Some(Session {
        key: Session::stable_key(AgentKind::Claude, &id),
        provider_session_id: id,
        name,
        search_terms,
        first_prompt,
        agent: AgentKind::Claude,
        status,
        cwd: cwd.clone(),
        branch,
        transcript_path: Some(path.to_owned()),
        transcript_modified_at: modified_at,
        transcript_fingerprint: fingerprint,
        summary_fingerprint: String::new(),
        summary_updated_at: None,
        summary_error: None,
        summary,
        recent_activity: activity.into_iter().collect(),
        pending_decisions: Vec::new(),
        pending_shell_injection: None,
        managed_alive: false,
        unavailable_reason: (!cwd.is_dir()).then(|| "working directory no longer exists".into()),
        discovered_after_startup: false,
    }))
}

#[cfg(test)]
pub(crate) fn parse_pi(path: &Path) -> io::Result<Option<Session>> {
    parse_pi_with_cached_prompt(path, None)
}

/// One pi session file (`~/.pi/agent/sessions/--<encoded-cwd>--/<stamp>_<uuid>.jsonl`).
///
/// pi stores a conversation *tree*: every entry carries `id`/`parentId`, and `/tree` can move
/// the leaf back onto an earlier branch. The dashboard shows what the session has been doing,
/// not which branch is live, so this reads the file in write order rather than walking the
/// branch -- the same way the Codex and Claude readers treat their own transcripts.
pub(crate) fn parse_pi_with_cached_prompt(
    path: &Path,
    cached_prompt: Option<(&str, &str)>,
) -> io::Result<Option<Session>> {
    let (lines, fingerprint, modified_at) = bounded_jsonl(path)?;
    let mut id = pi_session_id(path);
    let mut cwd = None;
    let mut session_name = None;
    let mut model = None;
    let mut first_prompt = None;
    let mut latest_prompt = None;
    let mut conversation_summary = None;
    let mut task = String::new();
    let mut activity = VecDeque::new();
    let mut failed = false;

    for line in lines {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str).unwrap_or("") {
            "session" => {
                id = raw_string_field(&value, "id").or(id);
                cwd = raw_string_field(&value, "cwd").map(PathBuf::from).or(cwd);
            }
            "session_info" => session_name = string_field(&value, "name").or(session_name),
            "model_change" => model = string_field(&value, "modelId").or(model),
            "compaction" | "branch_summary" => {
                conversation_summary = string_field(&value, "summary").or(conversation_summary);
            }
            // `custom_message` is deliberately absent: an extension\'s injected context has no
            // `role`, so it is not something the user or the agent did. The web conversation
            // view renders it; this activity feed does not.
            "message" => {
                let message = value.get("message").unwrap_or(&value);
                pi_absorb_message(
                    message,
                    &mut first_prompt,
                    &mut latest_prompt,
                    &mut task,
                    &mut activity,
                    &mut failed,
                );
            }
            _ => {}
        }
    }

    let (Some(id), Some(cwd)) = (id, cwd) else {
        return Ok(None);
    };
    if let Some((_, prompt)) =
        cached_prompt.filter(|(provider_session_id, _)| *provider_session_id == id)
    {
        first_prompt = Some(prompt.to_owned());
    } else if let Some(prompt) = first_prompt_in_jsonl(path, pi_prompt_from_record)? {
        // Only on a cache miss: this reads to EOF when no user turn yields text, and doing it
        // unconditionally made every poll re-read the whole transcript from byte 0.
        first_prompt = Some(prompt);
    }
    let mut search_terms = Vec::new();
    for term in [
        session_name.clone(),
        first_prompt.clone(),
        latest_prompt,
        conversation_summary,
        model,
    ]
    .into_iter()
    .flatten()
    {
        push_search_term(&mut search_terms, term);
    }
    let name = session_name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| directory_name(&cwd));
    let status = if failed {
        SessionStatus::Failed
    } else {
        SessionStatus::Idle
    };
    let summary = SessionSummary {
        task,
        status,
        current_action: activity.back().cloned().unwrap_or_default(),
        ..SessionSummary::default()
    };

    Ok(Some(Session {
        key: Session::stable_key(AgentKind::Pi, &id),
        provider_session_id: id,
        name,
        search_terms,
        first_prompt,
        agent: AgentKind::Pi,
        status,
        cwd: cwd.clone(),
        branch: None,
        transcript_path: Some(path.to_owned()),
        transcript_modified_at: modified_at,
        transcript_fingerprint: fingerprint,
        summary_fingerprint: String::new(),
        summary_updated_at: None,
        summary_error: None,
        summary,
        recent_activity: activity.into_iter().collect(),
        pending_decisions: Vec::new(),
        pending_shell_injection: None,
        managed_alive: false,
        unavailable_reason: (!cwd.is_dir()).then(|| "working directory no longer exists".into()),
        discovered_after_startup: false,
    }))
}

/// The session id pi puts in the file name (`<stamp>_<id>.jsonl`), used only until the header
/// line supplies the authoritative one -- which it always does for a usable session, since a
/// file with no header has no `cwd` either and is rejected outright.
///
/// It still matters: pi splits the name at the last `_`, so an id containing one is truncated
/// here, and the header is what repairs it.
fn pi_session_id(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|value| value.to_str())
        .and_then(|stem| stem.rsplit_once('_'))
        .map(|(_, id)| id.to_owned())
}

fn pi_absorb_message(
    message: &Value,
    first_prompt: &mut Option<String>,
    latest_prompt: &mut Option<String>,
    task: &mut String,
    activity: &mut VecDeque<String>,
    failed: &mut bool,
) {
    let content = message.get("content");
    let text = content_text(content);
    match message.get("role").and_then(Value::as_str).unwrap_or("") {
        "user" => {
            if let Some(text) = user_prompt_text(&text) {
                first_prompt.get_or_insert_with(|| text.clone());
                *latest_prompt = Some(text.clone());
                task.clone_from(&text);
                push_activity(activity, format!("You: {text}"));
            }
        }
        "assistant" => {
            if !text.is_empty() {
                push_activity(activity, format!("Agent: {text}"));
            }
            for tool in pi_tool_names(content) {
                push_activity(activity, format!("Tool: {tool}"));
            }
            // `stopReason` is how pi records a turn that died in the provider; the human-facing
            // reason lives in `errorMessage`, so the activity line says what went wrong rather
            // than only that something did.
            if message.get("stopReason").and_then(Value::as_str) == Some("error") {
                *failed = true;
                let reason = string_field(message, "errorMessage")
                    .unwrap_or_else(|| "API request failed".to_owned());
                push_activity(activity, reason);
            }
        }
        "toolResult" => {
            if !text.is_empty() {
                push_activity(activity, format!("Tool result: {text}"));
            }
        }
        "bashExecution" => {
            if let Some(command) = string_field(message, "command") {
                push_activity(activity, format!("Shell: {command}"));
            }
        }
        "compactionSummary" | "branchSummary" => {
            if let Some(summary) = string_field(message, "summary") {
                push_activity(activity, format!("Summary: {summary}"));
            }
        }
        _ => {}
    }
}

/// pi names an assistant's tool calls in `toolCall` content blocks, where Codex and Claude
/// both use `tool_use`.
fn pi_tool_names(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("toolCall"))
        .filter_map(|item| item.get("name").and_then(Value::as_str))
        .map(clean_text)
        .collect()
}

fn bounded_jsonl(path: &Path) -> io::Result<(Vec<String>, String, u64)> {
    let metadata = fs::metadata(path)?;
    let length = metadata.len();
    let modified_at = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |value| value.as_secs());
    let fingerprint = format!("{modified_at}:{length}");
    let mut file = fs::File::open(path)?;

    if length <= (HEAD_BYTES + TAIL_BYTES) as u64 {
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        return Ok((
            text.lines().map(str::to_owned).collect(),
            fingerprint,
            modified_at,
        ));
    }

    let mut head = vec![0; HEAD_BYTES];
    let head_read = file.read(&mut head)?;
    head.truncate(head_read);
    file.seek(SeekFrom::End(-(TAIL_BYTES as i64)))?;
    let mut tail = Vec::with_capacity(TAIL_BYTES);
    file.read_to_end(&mut tail)?;

    let mut lines = String::from_utf8_lossy(&head)
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let tail_text = String::from_utf8_lossy(&tail);
    let tail_text = tail_text.split_once('\n').map_or("", |(_, rest)| rest);
    lines.extend(tail_text.lines().map(str::to_owned));
    Ok((lines, fingerprint, modified_at))
}

fn first_prompt_in_jsonl(
    path: &Path,
    extract: fn(&Value) -> Option<String>,
) -> io::Result<Option<String>> {
    let file = fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if !line.ends_with(b"\n") {
            break;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if let Some(prompt) = extract(&value) {
            return Ok(Some(prompt));
        }
    }
    Ok(None)
}

fn scan_claude_identity(path: &Path) -> io::Result<(Option<String>, bool)> {
    let file = fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut first_prompt = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if !line.ends_with(b"\n") {
            break;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            return Ok((None, true));
        }
        if first_prompt.is_none() {
            first_prompt = claude_prompt_from_record(&value);
        }
    }
    Ok((first_prompt, false))
}

fn codex_prompt_from_record(value: &Value) -> Option<String> {
    let record_type = value.get("type").and_then(Value::as_str).unwrap_or("");
    let payload = value.get("payload").unwrap_or(&Value::Null);
    let payload_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
    let text = match (record_type, payload_type) {
        ("event_msg", "user_message") => string_field(payload, "message"),
        ("response_item", "message")
            if payload.get("role").and_then(Value::as_str) == Some("user") =>
        {
            let text = content_text(payload.get("content"));
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }?;
    user_prompt_text(&text)
}

fn claude_prompt_from_record(value: &Value) -> Option<String> {
    if value.get("type").and_then(Value::as_str) != Some("user") {
        return None;
    }
    // Claude marks every turn it writes on the user's behalf -- a hook's directive, a
    // command's own echo -- with `isMeta`. None of it is anything a person typed, and a
    // session driven entirely through `/goal` ended up titled with a Stop hook's prose
    // because the injected records were the only ones left after the wrappers were dropped.
    if value.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let content = value.get("message")?.get("content");
    let has_tool_result = content
        .and_then(Value::as_array)
        .is_some_and(|items| item_type_present(items, "tool_result"));
    let text = content_text(content);
    (!has_tool_result)
        .then(|| user_prompt_text(&text))
        .flatten()
}

/// The first thing a person typed into a pi session. `custom_message` entries are
/// extension-injected context rather than the user's own words, so only `message` entries
/// with a `user` role can name the session.
fn pi_prompt_from_record(value: &Value) -> Option<String> {
    if value.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let message = value.get("message")?;
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return None;
    }
    user_prompt_text(&content_text(message.get("content")))
}

fn file_modified(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |value| value.as_secs())
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(clean_text)
        .filter(|value| !value.is_empty())
}

fn raw_string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|value| !value.is_empty())
}

fn content_text(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    if let Some(text) = value.as_str() {
        return clean_text(text);
    }
    let Some(items) = value.as_array() else {
        return String::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
            if matches!(kind, "text" | "input_text" | "output_text" | "tool_result") {
                item.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| item.get("content").and_then(Value::as_str))
                    .map(clean_text)
            } else {
                None
            }
        })
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn tool_names(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(|item| item.get("name").and_then(Value::as_str))
        .map(clean_text)
        .collect()
}

fn item_type_present(items: &[Value], expected: &str) -> bool {
    items
        .iter()
        .any(|item| item.get("type").and_then(Value::as_str) == Some(expected))
}

fn clean_text(value: &str) -> String {
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut output = collapsed.chars().take(800).collect::<String>();
    if collapsed.chars().count() > 800 {
        output.push('…');
    }
    output
}

fn user_prompt_text(value: &str) -> Option<String> {
    if let Some(objective) = codex_goal_objective(value) {
        return Some(objective);
    }
    if let Some(request) = slash_command_request(value) {
        return Some(request);
    }
    if is_internal_context(value) {
        return None;
    }
    let text = strip_image_attachments(value);
    (!text.is_empty() && !is_internal_context(&text)).then_some(text)
}

/// What a person typed after a slash command. Claude stores `/goal ship it` as
/// `<command-name>/goal</command-name>...<command-args>ship it</command-args>`, which
/// `is_internal_context` drops whole -- and with it the only words in a session someone drove
/// entirely through commands. The arguments alone name the work; the command's own name does
/// not, so `/clear` and `/model` still count as nothing said.
fn slash_command_request(value: &str) -> Option<String> {
    let value = value.trim_start();
    if !value.starts_with("<command-name>") {
        return None;
    }
    let args = value
        .split_once("<command-args>")?
        .1
        .split_once("</command-args>")?
        .0;
    let args = strip_image_attachments(args);
    (!args.is_empty() && !is_internal_context(&args)).then_some(args)
}

fn codex_goal_objective(value: &str) -> Option<String> {
    let value = value.trim_start();
    if !value.starts_with("<codex_internal_context") {
        return None;
    }
    let opening_end = value.find('>')?;
    let opening = &value[..=opening_end];
    if !opening.contains("source=\"goal\"") && !opening.contains("source='goal'") {
        return None;
    }
    let objective = value
        .split_once("<objective>")?
        .1
        .split("</objective>")
        .next()
        .unwrap_or_default();
    let objective = strip_image_attachments(objective);
    (!objective.is_empty()).then_some(objective)
}

fn strip_image_attachments(value: &str) -> String {
    let mut text = value.to_owned();
    while let Some(start) = text.find("<image") {
        let Some(open_end) = text[start..].find('>').map(|offset| start + offset) else {
            break;
        };
        let end = text[open_end + 1..]
            .find("</image>")
            .map_or(open_end + 1, |offset| {
                open_end + 1 + offset + "</image>".len()
            });
        text.replace_range(start..end, " ");
    }
    text = text.replace("</image>", " ");
    while let Some(start) = text.find("[Image #") {
        let Some(end) = text[start..].find(']').map(|offset| start + offset + 1) else {
            break;
        };
        text.replace_range(start..end, " ");
    }
    clean_text(&text)
}

pub(crate) fn is_internal_context(value: &str) -> bool {
    let value = value.trim_start();
    value
        .strip_prefix("# AGENTS.md instructions")
        .is_some_and(|rest| rest.trim_start().starts_with("<INSTRUCTIONS>"))
        || [
            "<environment_context>",
            "<codex_internal_context",
            "<permissions instructions>",
            "<collaboration_mode>",
            "<skills_instructions>",
            "<apps_instructions>",
            "<plugins_instructions>",
            "<subagent_notification>",
            "<turn_aborted>",
            "<user_shell_command>",
            "<observed_from_primary_session>",
            "<system-reminder>",
            "<command-name>",
            "<command-message>",
            "<command-args>",
            "<local-command-caveat>",
            "<local-command-stdout>",
            "<local-command-stderr>",
            "<bash-input>",
            "<bash-stdout>",
            "<bash-stderr>",
        ]
        .iter()
        .any(|prefix| value.starts_with(prefix))
}

fn push_activity(activity: &mut VecDeque<String>, value: String) {
    if value.trim().is_empty() {
        return;
    }
    if activity.back() == Some(&value) {
        return;
    }
    activity.push_back(value);
    while activity.len() > MAX_ACTIVITY {
        activity.pop_front();
    }
}

fn push_search_term(terms: &mut Vec<String>, value: String) {
    let value = value.trim();
    if !value.is_empty() && !terms.iter().any(|term| term == value) {
        terms.push(value.to_owned());
    }
}

fn scalar_string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(|value| match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    })
}

fn directory_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("session")
        .to_owned()
}

fn is_claude_mem_observer(cwd: &Path) -> bool {
    cwd.file_name().and_then(|name| name.to_str()) == Some("observer-sessions")
        && cwd
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            == Some(".claude-mem")
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::tempdir;
    use uuid::Uuid;

    use super::*;

    #[test]
    fn discovers_codex_and_claude_fixtures() {
        let root = tempdir().unwrap();
        let codex = root.path().join("codex/2026/07/15");
        let claude = root.path().join("claude/project");
        fs::create_dir_all(&codex).unwrap();
        fs::create_dir_all(&claude).unwrap();
        let cwd = root.path().join("repo");
        fs::create_dir(&cwd).unwrap();

        let mut codex_file = fs::File::create(codex.join("rollout-test.jsonl")).unwrap();
        writeln!(
            codex_file,
            "{}",
            serde_json::json!({"type":"session_meta","payload":{"id":"codex-1","cwd":cwd}})
        )
        .unwrap();
        writeln!(
            codex_file,
            "{}",
            serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":"Fix login"}})
        )
        .unwrap();

        let claude_id = Uuid::new_v4().to_string();
        let mut claude_file = fs::File::create(claude.join(format!("{claude_id}.jsonl"))).unwrap();
        writeln!(
            claude_file,
            "{}",
            serde_json::json!({"type":"user","sessionId":claude_id,"cwd":cwd,"gitBranch":"main","message":{"role":"user","content":"Add tests"}})
        )
        .unwrap();

        let sessions = discover(&DiscoveryPaths {
            codex_sessions: root.path().join("codex"),
            claude_projects: root.path().join("claude"),
            pi_sessions: PathBuf::new(),
        });
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().any(|session| {
            session.key == "codex:codex-1" && session.summary.task == "Fix login"
        }));
        assert!(sessions.iter().any(|session| {
            session.agent == AgentKind::Claude && session.summary.task == "Add tests"
        }));
    }

    #[test]
    fn first_prompts_are_recovered_from_between_the_bounded_windows() {
        let root = tempdir().unwrap();
        let codex_dir = root.path().join("codex");
        let claude_dir = root.path().join("claude/project");
        let cwd = root.path().join("repo");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::create_dir_all(&claude_dir).unwrap();
        fs::create_dir(&cwd).unwrap();
        let head_padding = "h".repeat(HEAD_BYTES + 1_024);
        let tail_padding = "t".repeat(TAIL_BYTES * 2);

        let codex_path = codex_dir.join("rollout-windowed.jsonl");
        let mut codex = fs::File::create(&codex_path).unwrap();
        for record in [
            serde_json::json!({"type":"session_meta","payload":{"id":"windowed","cwd":cwd}}),
            serde_json::json!({"type":"developer_context","payload":{"text":head_padding}}),
            serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":"Keep the first Codex prompt"}}),
            serde_json::json!({"type":"tool_output","payload":{"text":tail_padding}}),
            serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":"Do not replace the Codex title"}}),
            serde_json::json!({"type":"event_msg","payload":{"type":"task_complete"}}),
        ] {
            writeln!(codex, "{record}").unwrap();
        }

        let claude_id = "11111111-1111-4111-8111-111111111111";
        let claude_path = claude_dir.join(format!("{claude_id}.jsonl"));
        let mut claude = fs::File::create(&claude_path).unwrap();
        for record in [
            serde_json::json!({"type":"system","sessionId":claude_id,"cwd":cwd}),
            serde_json::json!({"type":"developer_context","padding":head_padding}),
            serde_json::json!({"type":"user","sessionId":claude_id,"cwd":cwd,"message":{"role":"user","content":"Keep the first Claude prompt"}}),
            serde_json::json!({"type":"assistant","sessionId":claude_id,"cwd":cwd,"message":{"role":"assistant","content":tail_padding}}),
            serde_json::json!({"type":"user","sessionId":claude_id,"cwd":cwd,"message":{"role":"user","content":"Do not replace the Claude title"}}),
            serde_json::json!({"type":"system","sessionId":claude_id,"cwd":cwd}),
        ] {
            writeln!(claude, "{record}").unwrap();
        }

        assert_eq!(
            parse_codex(&codex_path)
                .unwrap()
                .unwrap()
                .first_prompt
                .as_deref(),
            Some("Keep the first Codex prompt")
        );
        assert_eq!(
            parse_claude(&claude_path)
                .unwrap()
                .unwrap()
                .first_prompt
                .as_deref(),
            Some("Keep the first Claude prompt")
        );
    }

    #[test]
    fn first_prompt_fallback_scans_past_the_old_byte_budget() {
        const OLD_SCAN_BYTES: usize = 512 * 1024;
        let root = tempdir().unwrap();
        let path = root.path().join("rollout-bounded.jsonl");
        let mut transcript = fs::File::create(&path).unwrap();
        writeln!(
            transcript,
            "{}",
            serde_json::json!({
                "type": "developer_context",
                "payload": {"text": "x".repeat(OLD_SCAN_BYTES + 1_024)}
            })
        )
        .unwrap();
        writeln!(
            transcript,
            "{}",
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "past the old budget"}
            })
        )
        .unwrap();
        drop(transcript);

        assert_eq!(
            first_prompt_in_jsonl(&path, codex_prompt_from_record)
                .unwrap()
                .as_deref(),
            Some("past the old budget")
        );

        let malformed_path = root.path().join("rollout-malformed.jsonl");
        let mut malformed = fs::File::create(&malformed_path).unwrap();
        malformed.write_all(b"\xff\n").unwrap();
        writeln!(
            malformed,
            "{}",
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "valid after malformed"}
            })
        )
        .unwrap();
        drop(malformed);
        assert_eq!(
            first_prompt_in_jsonl(&malformed_path, codex_prompt_from_record)
                .unwrap()
                .as_deref(),
            Some("valid after malformed")
        );

        let split_path = root.path().join("rollout-split-utf8.jsonl");
        let mut split = fs::File::create(&split_path).unwrap();
        split.write_all(&vec![b'x'; OLD_SCAN_BYTES - 1]).unwrap();
        split.write_all(&[0xc3, 0xa9, b'\n']).unwrap();
        writeln!(
            split,
            "{}",
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "after split utf8"}
            })
        )
        .unwrap();
        drop(split);
        assert_eq!(
            first_prompt_in_jsonl(&split_path, codex_prompt_from_record)
                .unwrap()
                .as_deref(),
            Some("after split utf8")
        );
    }

    #[test]
    fn changed_transcript_reuses_a_cached_prompt_for_the_same_provider_session() {
        let root = tempdir().unwrap();
        let codex_dir = root.path().join("codex");
        let claude_dir = root.path().join("claude");
        let cwd = root.path().join("repo");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::create_dir_all(&claude_dir).unwrap();
        fs::create_dir(&cwd).unwrap();
        let transcript_path = codex_dir.join("rollout-cached-prompt.jsonl");
        let head_padding = "h".repeat(HEAD_BYTES + 1_024);
        let tail_padding = "t".repeat(TAIL_BYTES * 2);
        let mut transcript = fs::File::create(&transcript_path).unwrap();
        for record in [
            serde_json::json!({"type":"session_meta","payload":{"id":"cached-prompt","cwd":cwd}}),
            serde_json::json!({"type":"developer_context","payload":{"text":head_padding}}),
            serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":"Keep the cached prompt"}}),
            serde_json::json!({"type":"tool_output","payload":{"text":tail_padding}}),
        ] {
            writeln!(transcript, "{record}").unwrap();
        }
        drop(transcript);
        let paths = DiscoveryPaths {
            codex_sessions: codex_dir,
            claude_projects: claude_dir,
            pi_sessions: PathBuf::new(),
        };
        let mut cache = DiscoveryCache::default();
        let first = discover_cached(&paths, &mut cache);
        assert_eq!(
            first[0].first_prompt.as_deref(),
            Some("Keep the cached prompt")
        );

        let mut transcript = fs::File::create(&transcript_path).unwrap();
        for record in [
            serde_json::json!({"type":"session_meta","payload":{"id":"cached-prompt","cwd":cwd}}),
            serde_json::json!({"type":"developer_context","payload":{"text":"h".repeat(HEAD_BYTES + 2_048)}}),
            serde_json::json!({"type":"tool_output","payload":{"text":"t".repeat(TAIL_BYTES * 2)}}),
        ] {
            writeln!(transcript, "{record}").unwrap();
        }
        drop(transcript);

        let refreshed = discover_cached(&paths, &mut cache);
        assert_eq!(
            refreshed[0].first_prompt.as_deref(),
            Some("Keep the cached prompt")
        );
    }

    #[test]
    fn codex_title_uses_the_first_real_prompt_once() {
        let root = tempdir().unwrap();
        let cwd = root.path().join("repo");
        fs::create_dir(&cwd).unwrap();
        let path = root.path().join("rollout-agents-instructions.jsonl");
        let mut transcript = fs::File::create(&path).unwrap();
        for record in [
            serde_json::json!({
                "type": "session_meta",
                "payload": {"id": "agents-instructions", "cwd": cwd}
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{
                        "type": "input_text",
                        "text": "# AGENTS.md instructions\n\n<INSTRUCTIONS>\nUse repository rules.\n</INSTRUCTIONS><environment_context>\n<cwd>/tmp/repo</cwd>\n</environment_context>"
                    }]
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{
                        "type": "input_text",
                        "text": "Use the first real prompt as the session title"
                    }]
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{
                        "type": "input_text",
                        "text": "This later prompt updates the current task only"
                    }]
                }
            }),
        ] {
            writeln!(transcript, "{record}").unwrap();
        }

        let session = parse_codex(&path).unwrap().unwrap();
        assert_eq!(
            session.first_prompt.as_deref(),
            Some("Use the first real prompt as the session title")
        );
        assert_eq!(
            session.list_title(),
            "Use the first real prompt as the session title"
        );
        assert_eq!(
            session.summary.task,
            "This later prompt updates the current task only"
        );
        assert_eq!(
            user_prompt_text("# AGENTS.md instructions for end users"),
            Some("# AGENTS.md instructions for end users".into())
        );
    }

    #[test]
    fn provider_titles_use_the_first_real_text_after_internal_and_image_content() {
        let root = tempdir().unwrap();
        let cwd = root.path().join("repo");
        fs::create_dir(&cwd).unwrap();

        let goal_path = root.path().join("rollout-goal.jsonl");
        let mut goal = fs::File::create(&goal_path).unwrap();
        for record in [
            serde_json::json!({"type":"session_meta","payload":{"id":"goal","cwd":cwd}}),
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<codex_internal_context source=\"goal\">Continue toward the active goal. <objective>Build the browser extension</objective></codex_internal_context>"}]}}),
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<image name=[Image #1] path=\"/tmp/example.png\"> </image> [Image #1]"}]}}),
        ] {
            writeln!(goal, "{record}").unwrap();
        }

        let placeholder_path = root.path().join("rollout-placeholders.jsonl");
        let mut placeholder = fs::File::create(&placeholder_path).unwrap();
        for record in [
            serde_json::json!({"type":"session_meta","payload":{"id":"placeholders","cwd":cwd}}),
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<codex_internal_context source=\"runtime\">generated launch metadata</codex_internal_context>"}]}}),
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<image name=[Image #1] path=\"/tmp/example.png\"> </image> [Image #1]"}]}}),
            serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":"Analyze the actual request"}}),
        ] {
            writeln!(placeholder, "{record}").unwrap();
        }

        let caption_path = root.path().join("rollout-caption.jsonl");
        let mut caption = fs::File::create(&caption_path).unwrap();
        for record in [
            serde_json::json!({"type":"session_meta","payload":{"id":"caption","cwd":cwd}}),
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<image name=[Image #1] path=\"/tmp/example.png\"> </image> [Image #1] Diagnose this layout regression"}]}}),
        ] {
            writeln!(caption, "{record}").unwrap();
        }

        let claude_path = root.path().join("claude-image.jsonl");
        let mut claude = fs::File::create(&claude_path).unwrap();
        for record in [
            serde_json::json!({"type":"user","sessionId":"claude-image","cwd":cwd,"message":{"role":"user","content":[{"type":"text","text":"<image name=[Image #1] path=\"/tmp/example.png\"> </image> [Image #1]"}]}}),
            serde_json::json!({"type":"user","sessionId":"claude-image","cwd":cwd,"message":{"role":"user","content":[{"type":"text","text":"First Claude text request"}]}}),
        ] {
            writeln!(claude, "{record}").unwrap();
        }

        assert_eq!(
            parse_codex(&goal_path)
                .unwrap()
                .unwrap()
                .first_prompt
                .as_deref(),
            Some("Build the browser extension")
        );
        assert_eq!(
            parse_codex(&placeholder_path)
                .unwrap()
                .unwrap()
                .first_prompt
                .as_deref(),
            Some("Analyze the actual request")
        );
        assert_eq!(
            parse_codex(&caption_path)
                .unwrap()
                .unwrap()
                .first_prompt
                .as_deref(),
            Some("Diagnose this layout regression")
        );
        assert_eq!(
            parse_claude(&claude_path)
                .unwrap()
                .unwrap()
                .first_prompt
                .as_deref(),
            Some("First Claude text request")
        );
    }

    #[test]
    fn codex_state_metadata_enriches_resume_search_terms() {
        let root = tempdir().unwrap();
        let codex_home = root.path().join("codex");
        let codex_sessions = codex_home.join("sessions/2026/07/24");
        let claude_projects = root.path().join("claude");
        fs::create_dir_all(&codex_sessions).unwrap();
        fs::create_dir_all(&claude_projects).unwrap();
        let cwd = root.path().join("repo");
        fs::create_dir(&cwd).unwrap();

        let mut transcript =
            fs::File::create(codex_sessions.join("rollout-provider-name.jsonl")).unwrap();
        writeln!(
            transcript,
            "{}",
            serde_json::json!({"type":"session_meta","payload":{
                "id":"provider-name","cwd":cwd
            }})
        )
        .unwrap();
        writeln!(
            transcript,
            "{}",
            serde_json::json!({"type":"event_msg","payload":{
                "type":"user_message","message":"latest rollout prompt"
            }})
        )
        .unwrap();

        let connection = Connection::open(codex_home.join("state_5.sqlite")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    name TEXT,
                    title TEXT,
                    first_user_message TEXT,
                    preview TEXT
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO threads
                 (id, name, title, first_user_message, preview)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    "provider-name",
                    "release-triage",
                    "Fix checkout timeout",
                    "Investigate payment failures",
                    "Preview the production incident"
                ],
            )
            .unwrap();

        let sessions = discover(&DiscoveryPaths {
            codex_sessions: codex_home.join("sessions"),
            claude_projects,
            pi_sessions: PathBuf::new(),
        });
        let session = sessions
            .iter()
            .find(|session| session.provider_session_id == "provider-name")
            .unwrap();
        assert_eq!(session.name, "release-triage");
        for term in [
            "release-triage",
            "Fix checkout timeout",
            "Investigate payment failures",
            "Preview the production incident",
        ] {
            assert!(session.search_terms.iter().any(|value| value == term));
        }
    }

    #[test]
    fn codex_metadata_cache_keeps_last_good_result_when_sqlite_refresh_fails() {
        let root = tempdir().unwrap();
        let codex_home = root.path().join("codex");
        let codex_sessions = codex_home.join("sessions");
        let claude_projects = root.path().join("claude");
        fs::create_dir_all(&codex_sessions).unwrap();
        fs::create_dir_all(&claude_projects).unwrap();
        let cwd = root.path().join("repo");
        fs::create_dir(&cwd).unwrap();
        let transcript_path = codex_sessions.join("rollout-last-good.jsonl");
        let mut transcript = fs::File::create(transcript_path).unwrap();
        writeln!(
            transcript,
            "{}",
            serde_json::json!({"type":"session_meta","payload":{"id":"last-good","cwd":cwd}})
        )
        .unwrap();
        let database_path = codex_home.join("state_5.sqlite");
        let connection = Connection::open(&database_path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    name TEXT,
                    title TEXT,
                    first_user_message TEXT,
                    preview TEXT
                );
                INSERT INTO threads (id, name) VALUES ('last-good', 'cached-name');",
            )
            .unwrap();
        drop(connection);
        let paths = DiscoveryPaths {
            codex_sessions,
            claude_projects,
            pi_sessions: PathBuf::new(),
        };
        let mut cache = DiscoveryCache::default();
        let first = discover_cached(&paths, &mut cache);
        assert_eq!(first[0].name, "cached-name");

        fs::write(&database_path, b"not a sqlite database anymore").unwrap();
        let refreshed = discover_cached(&paths, &mut cache);
        assert_eq!(refreshed[0].name, "cached-name");
    }

    /// Writes one pi transcript under a session root shaped the way pi shapes it, and returns
    /// both the root to discover from and the file itself.
    fn pi_transcript(root: &Path, cwd: &Path, id: &str, records: &[Value]) -> (PathBuf, PathBuf) {
        let sessions = root.join("pi/sessions/--encoded-cwd--");
        fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join(format!("2026-08-30T00-04-14-153Z_{id}.jsonl"));
        let mut transcript = fs::File::create(&path).unwrap();
        writeln!(
            transcript,
            "{}",
            serde_json::json!({
                "type":"session","version":3,"id":id,
                "timestamp":"2026-08-30T00:04:14.153Z","cwd":cwd
            })
        )
        .unwrap();
        for record in records {
            writeln!(transcript, "{record}").unwrap();
        }
        (root.join("pi/sessions"), path)
    }

    #[test]
    fn a_pi_transcript_yields_its_name_prompt_activity_and_search_terms() {
        let root = tempdir().unwrap();
        let cwd = root.path().join("repo");
        fs::create_dir(&cwd).unwrap();
        let id = Uuid::new_v4().to_string();
        let (sessions_root, path) = pi_transcript(
            root.path(),
            &cwd,
            &id,
            &[
                serde_json::json!({
                    "type":"model_change","id":"aa","parentId":null,
                    "timestamp":"2026-08-30T00:04:14.188Z",
                    "provider":"deepseek","modelId":"deepseek-v4-flash"
                }),
                serde_json::json!({
                    "type":"session_info","id":"bb","parentId":"aa",
                    "timestamp":"2026-08-30T00:04:14.200Z","name":"Wire up pi"
                }),
                serde_json::json!({
                    "type":"message","id":"cc","parentId":"bb",
                    "timestamp":"2026-08-30T00:04:14.194Z",
                    "message":{"role":"user","content":[{"type":"text","text":"Add pi support"}]}
                }),
                serde_json::json!({
                    "type":"message","id":"dd","parentId":"cc",
                    "timestamp":"2026-08-30T00:04:16.039Z",
                    "message":{"role":"assistant","stopReason":"toolUse","content":[
                        {"type":"thinking","thinking":"planning"},
                        {"type":"text","text":"Reading the adapter table"},
                        {"type":"toolCall","id":"call-1","name":"read",
                         "arguments":{"path":"src/providers.rs"}}
                    ]}
                }),
                serde_json::json!({
                    "type":"message","id":"ee","parentId":"dd",
                    "timestamp":"2026-08-30T00:04:17.000Z",
                    "message":{"role":"toolResult","toolCallId":"call-1","toolName":"read",
                        "isError":false,"content":[{"type":"text","text":"const ADAPTERS"}]}
                }),
                serde_json::json!({
                    "type":"message","id":"ff","parentId":"ee",
                    "timestamp":"2026-08-30T00:04:18.000Z",
                    "message":{"role":"user","content":[{"type":"text","text":"Now run the tests"}]}
                }),
            ],
        );

        let sessions = discover(&DiscoveryPaths {
            codex_sessions: root.path().join("codex"),
            claude_projects: root.path().join("claude"),
            pi_sessions: sessions_root,
        });
        let session = sessions
            .iter()
            .find(|session| session.provider_session_id == id)
            .unwrap();
        assert_eq!(session.agent, AgentKind::Pi);
        assert_eq!(session.key, format!("pi:{id}"));
        assert_eq!(session.name, "Wire up pi");
        assert_eq!(session.cwd, cwd);
        assert_eq!(session.transcript_path.as_deref(), Some(path.as_path()));
        assert_eq!(session.first_prompt.as_deref(), Some("Add pi support"));
        assert_eq!(session.summary.task, "Now run the tests");
        assert_eq!(session.list_title(), "Add pi support");
        assert!(
            session
                .recent_activity
                .iter()
                .any(|line| line == "Tool: read"),
            "a pi tool call is named by its `toolCall` block, not by `tool_use`: {:?}",
            session.recent_activity
        );
        assert!(
            session
                .recent_activity
                .iter()
                .any(|line| line == "Tool result: const ADAPTERS"),
            "{:?}",
            session.recent_activity
        );
        for term in ["Wire up pi", "Add pi support", "deepseek-v4-flash"] {
            assert!(
                session
                    .search_terms
                    .iter()
                    .any(|value| value.eq_ignore_ascii_case(term)),
                "missing search term {term}: {:?}",
                session.search_terms
            );
        }
    }

    #[test]
    fn a_pi_turn_that_died_in_the_provider_reads_as_failed() {
        let root = tempdir().unwrap();
        let cwd = root.path().join("repo");
        fs::create_dir(&cwd).unwrap();
        let id = Uuid::new_v4().to_string();
        let (_, path) = pi_transcript(
            root.path(),
            &cwd,
            &id,
            &[
                serde_json::json!({
                    "type":"message","id":"cc","parentId":null,
                    "timestamp":"2026-08-30T00:04:14.194Z",
                    "message":{"role":"user","content":[{"type":"text","text":"Summarize"}]}
                }),
                serde_json::json!({
                    "type":"message","id":"dd","parentId":"cc",
                    "timestamp":"2026-08-30T00:04:16.039Z",
                    "message":{"role":"assistant","stopReason":"error",
                        "errorMessage":"deepseek returned 429","content":[]}
                }),
            ],
        );

        let session = parse_pi(&path).unwrap().unwrap();
        assert_eq!(session.status, SessionStatus::Failed);
        assert_eq!(
            session.recent_activity.last().map(String::as_str),
            Some("deepseek returned 429"),
            "the reason the turn died is what the dashboard has to show"
        );
    }

    #[test]
    fn a_pi_transcript_without_its_header_is_not_a_session() {
        let root = tempdir().unwrap();
        let sessions = root.path().join("pi/sessions/--encoded-cwd--");
        fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join(format!("2026-08-30T00-04-14-153Z_{}.jsonl", Uuid::new_v4()));
        // A file that never got its `session` line has no cwd, and a session with no working
        // directory cannot be resumed or shown.
        fs::write(&path, "{\"type\":\"message\",\"id\":\"cc\"}\n").unwrap();
        assert!(parse_pi(&path).unwrap().is_none());
    }

    #[test]
    fn claude_resume_metadata_includes_titles_prompts_summary_tag_and_pr() {
        let root = tempdir().unwrap();
        let codex = root.path().join("codex");
        let claude = root.path().join("claude/project");
        fs::create_dir_all(&codex).unwrap();
        fs::create_dir_all(&claude).unwrap();
        let cwd = root.path().join("repo");
        fs::create_dir(&cwd).unwrap();
        let claude_id = Uuid::new_v4().to_string();
        let mut transcript = fs::File::create(claude.join(format!("{claude_id}.jsonl"))).unwrap();
        for record in [
            serde_json::json!({
                "type":"ai-title","sessionId":claude_id,"aiTitle":"OIDC rollout"
            }),
            serde_json::json!({
                "type":"user","sessionId":claude_id,"cwd":cwd,"gitBranch":"feat/oidc",
                "message":{"role":"user","content":"Design the authentication flow"}
            }),
            serde_json::json!({
                "type":"summary","sessionId":claude_id,
                "summary":"Authentication middleware migrated to OIDC"
            }),
            serde_json::json!({
                "type":"tag","sessionId":claude_id,"tag":"security"
            }),
            serde_json::json!({
                "type":"pr-link","sessionId":claude_id,"prNumber":4869,
                "prUrl":"https://gitlab.example/deepmap/airflow/-/merge_requests/4869",
                "prRepository":"deepmap/airflow"
            }),
            serde_json::json!({
                "type":"user","sessionId":claude_id,"cwd":cwd,"gitBranch":"feat/oidc",
                "message":{"role":"user","content":"Ship the final migration"}
            }),
            serde_json::json!({
                "type":"last-prompt","sessionId":claude_id,
                "lastPrompt":"Verify the staging deployment"
            }),
        ] {
            writeln!(transcript, "{record}").unwrap();
        }

        let sessions = discover(&DiscoveryPaths {
            codex_sessions: codex,
            claude_projects: root.path().join("claude"),
            pi_sessions: PathBuf::new(),
        });
        let session = sessions
            .iter()
            .find(|session| session.provider_session_id == claude_id)
            .unwrap();
        assert_eq!(session.name, "OIDC rollout");
        assert_eq!(session.summary.task, "Ship the final migration");
        assert_eq!(
            session.first_prompt.as_deref(),
            Some("Design the authentication flow"),
            "the title must stay the first prompt while the task follows the latest one"
        );
        assert_eq!(session.list_title(), "Design the authentication flow");
        for term in [
            "OIDC rollout",
            "Design the authentication flow",
            "Verify the staging deployment",
            "Authentication middleware migrated to OIDC",
            "security",
            "https://gitlab.example/deepmap/airflow/-/merge_requests/4869",
            "deepmap/airflow",
            "4869",
            "pr #4869",
        ] {
            assert!(
                session.search_terms.iter().any(|value| value == term),
                "missing search term {term:?}"
            );
        }
    }

    #[test]
    fn codex_subagents_are_not_discovered() {
        let root = tempdir().unwrap();
        let codex = root.path().join("codex");
        fs::create_dir(&codex).unwrap();
        let cwd = root.path().join("repo");
        fs::create_dir(&cwd).unwrap();

        let mut parent = fs::File::create(codex.join("rollout-parent.jsonl")).unwrap();
        writeln!(
            parent,
            "{}",
            serde_json::json!({"type":"session_meta","payload":{
                "id":"parent","cwd":cwd,"thread_source":"user"
            }})
        )
        .unwrap();

        for (id, completed) in [("running-subagent", false), ("finished-subagent", true)] {
            let mut file = fs::File::create(codex.join(format!("rollout-{id}.jsonl"))).unwrap();
            writeln!(
                file,
                "{}",
                serde_json::json!({"type":"session_meta","payload":{
                    "id":id,
                    "cwd":cwd,
                    "forked_from_id":"parent",
                    "parent_thread_id":"parent",
                    "thread_source":"subagent",
                    "source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent"}}}
                }})
            )
            .unwrap();
            writeln!(
                file,
                "{}",
                serde_json::json!({"type":"session_meta","payload":{
                    "id":"parent","cwd":cwd,"thread_source":"user"
                }})
            )
            .unwrap();
            writeln!(
                file,
                "{}",
                serde_json::json!({"type":"event_msg","payload":{"type":"task_complete"}})
            )
            .unwrap();
            writeln!(
                file,
                "{}",
                serde_json::json!({"type":"event_msg","payload":{"type":"task_started"}})
            )
            .unwrap();
            if completed {
                writeln!(
                    file,
                    "{}",
                    serde_json::json!({"type":"event_msg","payload":{"type":"task_complete"}})
                )
                .unwrap();
            }
        }

        let sessions = discover_codex(&codex);
        assert_eq!(sessions.len(), 1);
        assert!(
            sessions
                .iter()
                .any(|session| session.provider_session_id == "parent")
        );
        assert!(
            !sessions
                .iter()
                .any(|session| session.provider_session_id == "running-subagent")
        );
        assert!(
            !sessions
                .iter()
                .any(|session| session.provider_session_id == "finished-subagent")
        );
    }

    #[test]
    fn claude_sidechain_markers_between_bounded_windows_are_not_discovered() {
        let root = tempdir().unwrap();
        let cwd = root.path().join("repo");
        fs::create_dir(&cwd).unwrap();
        let path = root
            .path()
            .join("11111111-1111-4111-8111-111111111111.jsonl");
        let mut file = fs::File::create(&path).unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({"type": "system", "sessionId": "parent-session", "cwd": cwd})
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "assistant",
                "message": {"role": "assistant", "content": "h".repeat(HEAD_BYTES + 1_024)}
            })
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "user",
                "isSidechain": true,
                "sessionId": "parent-session",
                "agentId": "a200da4e34f99330c",
                "cwd": cwd,
                "message": {"role": "user", "content": "Inspect the implementation"}
            })
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "assistant",
                "message": {"role": "assistant", "content": "t".repeat(TAIL_BYTES * 2)}
            })
        )
        .unwrap();

        assert!(parse_claude(&path).unwrap().is_none());
        let paths = DiscoveryPaths {
            codex_sessions: root.path().join("codex"),
            claude_projects: root.path().to_owned(),
            pi_sessions: PathBuf::new(),
        };
        let mut cache = DiscoveryCache::default();
        assert!(discover_cached(&paths, &mut cache).is_empty());
        assert!(
            cache
                .entries
                .get(&path)
                .is_some_and(|(_, session)| session.is_none()),
            "excluded transcripts should be cached until their metadata changes"
        );
    }

    #[test]
    fn malformed_lines_are_ignored() {
        let root = tempdir().unwrap();
        let codex = root.path().join("codex");
        fs::create_dir(&codex).unwrap();
        let cwd = root.path().join("repo");
        fs::create_dir(&cwd).unwrap();
        let mut file = fs::File::create(codex.join("rollout-test.jsonl")).unwrap();
        writeln!(file, "not json").unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({"type":"session_meta","payload":{"id":"ok","cwd":cwd}})
        )
        .unwrap();

        let sessions = discover_codex(&codex);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].provider_session_id, "ok");
    }

    #[test]
    fn internal_context_is_not_treated_as_a_user_task() {
        assert!(is_internal_context(
            "<environment_context> <cwd>/tmp</cwd> </environment_context>"
        ));
        assert!(!is_internal_context("Implement the login flow"));
    }

    #[test]
    fn provider_command_wrappers_are_not_treated_as_a_user_task() {
        for value in [
            "<command-name>/clear</command-name>",
            "<local-command-caveat>Caveat: the messages below were generated",
            "<local-command-stdout>Set model to Opus 5",
            "<bash-input>pwd</bash-input>",
            "<observed_from_primary_session>\n  <what_happened>Read",
            "<user_shell_command>\n<command>\ngit status\n</command>",
            "<subagent_notification>\n{\"agent_path\":\"019f\"}",
            "<turn_aborted>\nThe user interrupted the previous turn",
        ] {
            assert!(is_internal_context(value), "{value}");
        }
    }

    /// A session driven entirely through `/goal` was titled with the prose a Stop hook
    /// injected on the user's behalf -- "A session-scoped Stop hook is now active with
    /// condition: ..." -- because the only records that were not command wrappers were the
    /// injected ones. Claude marks those `isMeta`, and the words the user actually typed are
    /// the command's arguments.
    #[test]
    fn an_injected_hook_directive_cannot_title_a_session_its_slash_command_can() {
        let root = tempdir().unwrap();
        let codex = root.path().join("codex");
        let claude = root.path().join("claude/project");
        fs::create_dir_all(&codex).unwrap();
        fs::create_dir_all(&claude).unwrap();
        let cwd = root.path().join("repo");
        fs::create_dir(&cwd).unwrap();
        let claude_id = Uuid::new_v4().to_string();
        let mut transcript = fs::File::create(claude.join(format!("{claude_id}.jsonl"))).unwrap();
        for (content, is_meta) in [
            (
                "<local-command-caveat>Caveat: the messages below were generated",
                true,
            ),
            (
                "<command-name>/goal</command-name>\n<command-message>goal</command-message>\n<command-args>Open-source the lantunnel repo under Apache 2.0</command-args>",
                false,
            ),
            (
                "A session-scoped Stop hook is now active with condition: \"Open-source the lantunnel repo\". Briefly acknowledge the goal, then immediately start working toward it.",
                true,
            ),
        ] {
            let record = serde_json::json!({
                "type":"user","sessionId":claude_id,"cwd":cwd,"isMeta":is_meta,
                "message":{"role":"user","content":content}
            });
            writeln!(transcript, "{record}").unwrap();
        }

        let sessions = discover(&DiscoveryPaths {
            codex_sessions: codex,
            claude_projects: root.path().join("claude"),
            pi_sessions: PathBuf::new(),
        });
        let session = sessions
            .iter()
            .find(|session| session.provider_session_id == claude_id)
            .unwrap();
        assert_eq!(
            session.list_title(),
            "Open-source the lantunnel repo under Apache 2.0"
        );
    }

    #[test]
    fn claude_command_wrappers_keep_the_last_real_prompt_as_the_task() {
        let root = tempdir().unwrap();
        let codex = root.path().join("codex");
        let claude = root.path().join("claude/project");
        fs::create_dir_all(&codex).unwrap();
        fs::create_dir_all(&claude).unwrap();
        let cwd = root.path().join("repo");
        fs::create_dir(&cwd).unwrap();
        let claude_id = Uuid::new_v4().to_string();
        let mut transcript = fs::File::create(claude.join(format!("{claude_id}.jsonl"))).unwrap();
        for content in [
            "Check the keybinding conflicts",
            "<command-name>/model</command-name>\n<command-message>model</command-message>",
            "<local-command-stdout>Set model to Opus 5 (1M context)</local-command-stdout>",
        ] {
            let record = serde_json::json!({
                "type":"user","sessionId":claude_id,"cwd":cwd,
                "message":{"role":"user","content":content}
            });
            writeln!(transcript, "{record}").unwrap();
        }

        let sessions = discover(&DiscoveryPaths {
            codex_sessions: codex,
            claude_projects: root.path().join("claude"),
            pi_sessions: PathBuf::new(),
        });
        let session = sessions
            .iter()
            .find(|session| session.provider_session_id == claude_id)
            .unwrap();
        assert_eq!(session.summary.task, "Check the keybinding conflicts");
    }

    #[test]
    fn claude_mem_observer_sessions_are_not_discovered() {
        let root = tempdir().unwrap();
        let codex = root.path().join("codex");
        let claude = root.path().join("claude/project");
        fs::create_dir_all(&codex).unwrap();
        fs::create_dir_all(&claude).unwrap();
        let observer_cwd = root.path().join(".claude-mem/observer-sessions");
        fs::create_dir_all(&observer_cwd).unwrap();
        let claude_id = Uuid::new_v4().to_string();
        let mut file = fs::File::create(claude.join(format!("{claude_id}.jsonl"))).unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "user",
                "sessionId": claude_id,
                "cwd": observer_cwd,
                "message": {"role": "user", "content": "monitor another session"}
            })
        )
        .unwrap();

        let sessions = discover(&DiscoveryPaths {
            codex_sessions: codex,
            claude_projects: root.path().join("claude"),
            pi_sessions: PathBuf::new(),
        });

        assert!(sessions.is_empty());
    }
}
