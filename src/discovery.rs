use std::{
    collections::{HashMap, HashSet, VecDeque},
    env, fs,
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use serde_json::Value;
use uuid::Uuid;

use crate::model::{AgentKind, Session, SessionStatus, SessionSummary};

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
}

#[derive(Default)]
pub struct DiscoveryCache {
    entries: HashMap<PathBuf, (String, Session)>,
}

impl DiscoveryPaths {
    pub fn from_environment() -> Option<Self> {
        let home = dirs::home_dir()?;
        let codex_home = env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"));
        Some(Self {
            codex_sessions: codex_home.join("sessions"),
            claude_projects: home.join(".claude/projects"),
        })
    }
}

#[cfg(test)]
pub fn discover(paths: &DiscoveryPaths) -> Vec<Session> {
    discover_cached(paths, &mut DiscoveryCache::default())
}

pub fn discover_cached(paths: &DiscoveryPaths, cache: &mut DiscoveryCache) -> Vec<Session> {
    let mut sessions = Vec::new();
    let codex_files = provider_files(&paths.codex_sessions, |path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
    });
    let claude_files = provider_files(&paths.claude_projects, |path| {
        let Some(stem) = path.file_stem().and_then(|name| name.to_str()) else {
            return false;
        };
        path.extension().and_then(|value| value.to_str()) == Some("jsonl")
            && Uuid::parse_str(stem).is_ok()
    });
    let seen = codex_files
        .iter()
        .chain(&claude_files)
        .cloned()
        .collect::<HashSet<_>>();
    sessions.extend(parse_cached_files(codex_files, AgentKind::Codex, cache));
    sessions.extend(parse_cached_files(claude_files, AgentKind::Claude, cache));
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

#[cfg(test)]
fn discover_codex(root: &Path) -> Vec<Session> {
    provider_files(root, |path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
    })
    .into_iter()
    .filter_map(|path| parse_codex(&path).ok().flatten())
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
    provider: AgentKind,
    cache: &mut DiscoveryCache,
) -> Vec<Session> {
    files
        .into_iter()
        .filter_map(|path| {
            let fingerprint = metadata_fingerprint(&path)?;
            if let Some((cached_fingerprint, session)) = cache.entries.get(&path)
                && cached_fingerprint == &fingerprint
            {
                return Some(session.clone());
            }
            let parsed = match provider {
                AgentKind::Codex => parse_codex(&path),
                AgentKind::Claude => parse_claude(&path),
            }
            .ok()
            .flatten()?;
            cache.entries.insert(path, (fingerprint, parsed.clone()));
            Some(parsed)
        })
        .collect()
}

fn metadata_fingerprint(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(format!("{modified}:{}", metadata.len()))
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

fn parse_codex(path: &Path) -> io::Result<Option<Session>> {
    let (lines, fingerprint, modified_at) = bounded_jsonl(path)?;
    let mut id = None;
    let mut cwd = None;
    let mut branch = None;
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
            id = raw_string_field(payload, "id").or(id);
            cwd = raw_string_field(payload, "cwd").map(PathBuf::from).or(cwd);
        }
        if record_type == "turn_context" {
            cwd = raw_string_field(payload, "cwd").map(PathBuf::from).or(cwd);
        }

        match (record_type, payload_type) {
            ("event_msg", "user_message") => {
                if let Some(text) = string_field(payload, "message")
                    && !is_internal_context(&text)
                {
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
                        if !is_internal_context(&text) {
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
    let name = directory_name(&cwd);
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

fn parse_claude(path: &Path) -> io::Result<Option<Session>> {
    let (lines, fingerprint, modified_at) = bounded_jsonl(path)?;
    let mut id = path
        .file_stem()
        .and_then(|value| value.to_str())
        .map(str::to_owned);
    let mut cwd = None;
    let mut branch = None;
    let mut title = None;
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

        if record_type == "ai-title" || record_type == "custom-title" {
            title = string_field(&value, "title")
                .or_else(|| string_field(&value, "customTitle"))
                .or(title);
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
                task.clone_from(&text);
                push_activity(&mut activity, format!("You: {text}"));
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
    let name = title.unwrap_or_else(|| directory_name(&cwd));
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

fn is_internal_context(value: &str) -> bool {
    [
        "<environment_context>",
        "<permissions instructions>",
        "<collaboration_mode>",
        "<skills_instructions>",
        "<apps_instructions>",
        "<plugins_instructions>",
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
        });

        assert!(sessions.is_empty());
    }
}
