//! Explicit, confirmed removal of old archived conversations and their provider records.

use std::{
    collections::BTreeSet,
    fs,
    io::{self, BufRead, IsTerminal, Read, Seek, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::Value;

use crate::{discovery::DiscoveryPaths, model::AgentKind, providers, pty, store};

const DAY: u64 = 24 * 60 * 60;
const HELP: &str = "Usage: agent-console prune-archived [--days N] [--dry-run]\n\n\
Delete archived sessions without activity for more than N days (default 7).\n\
Preview lists the sessions and provider files. --dry-run stops after the preview.\n\
Deletion requires an interactive terminal and typing DELETE <count> ARCHIVED SESSIONS.\n\
Close Agent Console dashboards/web servers first; open agent/shell sessions are skipped.\n\
Supported on macOS and Linux. There is no --yes or unattended deletion option.\n";

#[derive(Debug, Eq, PartialEq)]
struct Candidate {
    key: String,
    provider: AgentKind,
    id: String,
    title: String,
    last_activity: u64,
    paths: BTreeSet<PathBuf>,
    stamps: Vec<(PathBuf, u64, SystemTime)>,
}

pub(crate) fn run(mut args: impl Iterator<Item = String>) -> io::Result<()> {
    let mut days = 7_u64;
    let mut dry_run = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                print!("{HELP}");
                return Ok(());
            }
            "--dry-run" => dry_run = true,
            "--days" => {
                days = args
                    .next()
                    .and_then(|s| s.parse().ok())
                    .filter(|n: &u64| *n > 0 && n.checked_mul(DAY).is_some())
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "--days requires a positive number of days",
                        )
                    })?;
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown prune option: {arg}"),
                ));
            }
        }
    }
    let root =
        store::state_dir().ok_or_else(|| io::Error::other("cannot resolve state directory"))?;
    let paths = DiscoveryPaths::from_environment()
        .ok_or_else(|| io::Error::other("cannot resolve provider directories"))?;
    let now = crate::model::unix_timestamp();
    let candidates = plan(&root, &paths, days, now)?;
    for candidate in &candidates {
        println!(
            "{}  {} days  {:?}",
            candidate.key,
            now.saturating_sub(candidate.last_activity) / DAY,
            candidate.title
        );
        for path in &candidate.paths {
            println!("  {:?}", path);
        }
    }
    println!(
        "{} archived sessions older than {days} days.",
        candidates.len()
    );
    println!(
        "Matching entries in provider databases, history/index files and Agent Console state are included."
    );
    if dry_run || candidates.is_empty() {
        return Ok(());
    }
    if !cfg!(unix) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "prune-archived currently supports macOS and Linux",
        ));
    }
    if !io::stdin().is_terminal() {
        return Err(io::Error::other(
            "deletion requires typing the confirmation in an interactive terminal; use --dry-run to preview",
        ));
    }
    let lock = store::maintenance_lock(&root)?;
    lock.try_lock().map_err(|_| {
        io::Error::other("close Agent Console dashboards/web servers before pruning")
    })?;
    if !confirm(&mut io::stdin().lock(), &mut io::stdout(), candidates.len())? {
        println!("Cancelled. No sessions deleted.");
        return Ok(());
    }
    apply(&root, &paths, days, now, &candidates)?;
    println!("Deleted {} archived sessions.", candidates.len());
    Ok(())
}

fn confirmation(count: usize) -> String {
    format!("DELETE {count} ARCHIVED SESSIONS")
}

fn confirm(input: &mut impl BufRead, output: &mut impl Write, count: usize) -> io::Result<bool> {
    let phrase = confirmation(count);
    write!(output, "Permanently delete these sessions? Type {phrase}: ")?;
    output.flush()?;
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    Ok(answer.trim_end_matches(['\r', '\n']) == phrase)
}

fn read_database(path: &Path) -> io::Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(io::Error::other)
}

fn table_exists(db: &Connection, table: &str) -> io::Result<bool> {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )
    .map_err(io::Error::other)
}

fn files_under(root: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            files_under(&entry.path(), files)?;
        } else if kind.is_file() || kind.is_symlink() {
            files.push(entry.path());
        }
    }
    Ok(())
}

fn plan(root: &Path, paths: &DiscoveryPaths, days: u64, now: u64) -> io::Result<Vec<Candidate>> {
    let database = root.join("state.db");
    if !database.exists() {
        return Ok(Vec::new());
    }
    let db = read_database(&database)?;
    let mut statement = db
        .prepare("SELECT session_key, data_json FROM session_cache ORDER BY session_key")
        .map_err(io::Error::other)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(io::Error::other)?;
    let mut files = Vec::new();
    for kind in [AgentKind::Codex, AgentKind::Claude, AgentKind::Pi] {
        files_under(paths.root(kind), &mut files)?;
    }
    let codex_home = paths
        .codex_sessions
        .parent()
        .ok_or_else(|| io::Error::other("invalid Codex session directory"))?;
    files_under(&codex_home.join("archived_sessions"), &mut files)?;
    let claude_home = paths
        .claude_projects
        .parent()
        .ok_or_else(|| io::Error::other("invalid Claude session directory"))?;
    let mut claude_metadata = Vec::new();
    files_under(&claude_home.join("sessions"), &mut claude_metadata)?;
    let cutoff = now.saturating_sub(days.saturating_mul(DAY));
    let mut candidates = Vec::new();
    // ponytail: scan filenames per archived ID; index by ID if large histories make pruning slow.
    'sessions: for row in rows {
        let (key, data) = row.map_err(io::Error::other)?;
        let cached: store::CachedSession = serde_json::from_str(&data).map_err(io::Error::other)?;
        if !cached.archived {
            continue;
        }
        let Some((provider, id)) = key.split_once(':') else {
            continue;
        };
        let provider = match provider {
            "codex" => AgentKind::Codex,
            "claude" => AgentKind::Claude,
            "pi" => AgentKind::Pi,
            _ => continue,
        };
        // IDs are used as single path components, never as paths supplied by a cache.
        if id.is_empty()
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            || matches!(id, "." | "..")
        {
            return Err(io::Error::other(format!(
                "invalid cached session key: {key:?}"
            )));
        }
        let mut candidate = Candidate {
            key: key.clone(),
            provider,
            id: id.into(),
            title: cached
                .alias
                .clone()
                .or(cached.first_prompt.clone())
                .unwrap_or_else(|| key.clone()),
            last_activity: 0,
            paths: BTreeSet::new(),
            stamps: Vec::new(),
        };
        let mut fallback_activity = cached.summary_updated_at.unwrap_or(0);
        for fingerprint in [
            cached.summary_fingerprint.as_str(),
            cached
                .managed_transcript_fingerprint
                .as_deref()
                .unwrap_or(""),
        ] {
            for part in fingerprint.split('|') {
                if let Some((timestamp, _)) = part.split_once(':') {
                    fallback_activity = fallback_activity.max(timestamp.parse().unwrap_or(0));
                }
            }
        }
        for path in &files {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let belongs = match provider {
                AgentKind::Codex => {
                    path.starts_with(&paths.codex_sessions)
                        || path.starts_with(codex_home.join("archived_sessions"))
                }
                AgentKind::Claude => path.starts_with(&paths.claude_projects),
                AgentKind::Pi => path.starts_with(&paths.pi_sessions),
            };
            let suffix = match provider {
                AgentKind::Codex => format!("-{id}.jsonl"),
                AgentKind::Claude => format!("{id}.jsonl"),
                AgentKind::Pi => format!("_{id}.jsonl"),
            };
            if belongs && name.ends_with(&suffix) && (providers::adapter(provider).accepts)(path) {
                let parsed = (providers::adapter(provider).parse)(path, None)?;
                if parsed.as_ref().map(|s| s.provider_session_id.as_str()) != Some(id) {
                    return Err(io::Error::other(format!(
                        "transcript identity does not match {key}: {path:?}"
                    )));
                }
                candidate.paths.insert(path.clone());
                if provider == AgentKind::Claude {
                    candidate.paths.insert(path.parent().unwrap().join(id));
                }
            }
        }
        if provider == AgentKind::Claude {
            for folder in ["file-history", "tasks", "session-env"] {
                candidate.paths.insert(claude_home.join(folder).join(id));
            }
            for path in &claude_metadata {
                if path.extension().is_some_and(|ext| ext == "json") {
                    // Claude can overwrite a registry entry without truncating its old tail.
                    let bytes = fs::read(path)?;
                    let value = serde_json::Deserializer::from_slice(&bytes)
                        .into_iter::<Value>()
                        .next()
                        .transpose()
                        .map_err(|error| {
                            io::Error::other(format!("cannot read {path:?}: {error}"))
                        })?;
                    let Some(value) = value else {
                        continue;
                    };
                    if value.get("sessionId").and_then(Value::as_str) == Some(id) {
                        if let Some(pid) = value
                            .get("pid")
                            .and_then(Value::as_u64)
                            .and_then(|pid| u32::try_from(pid).ok())
                            && pid > 0
                            && pty::process_is_alive(pid)
                        {
                            eprintln!(
                                "Skipped {key}: close the Claude process {pid} before pruning"
                            );
                            continue 'sessions;
                        }
                        candidate.paths.insert(path.clone());
                    }
                }
            }
        }
        for suffix in [".jsonl", ".jsonl.1"] {
            candidate
                .paths
                .insert(root.join("events").join(format!("{provider}-{id}{suffix}")));
        }
        candidate.paths = candidate
            .paths
            .into_iter()
            .filter_map(|path| match path.symlink_metadata() {
                Ok(_) => Some(Ok(path)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => Some(Err(error)),
            })
            .collect::<io::Result<_>>()?;
        for path in &candidate.paths {
            let owner = if path.starts_with(root) {
                root
            } else {
                paths.root(provider).parent().unwrap()
            };
            if !path.canonicalize()?.starts_with(owner.canonicalize()?) {
                return Err(io::Error::other(format!(
                    "session artifact escapes its storage directory: {path:?}"
                )));
            }
            snapshot(path, &mut candidate.stamps)?;
        }
        candidate.stamps.sort();
        for (_, _, modified) in &candidate.stamps {
            candidate.last_activity = candidate.last_activity.max(
                modified
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            );
        }
        if table_exists(&db, "normalized_event")? {
            let activity: Option<i64> = db.query_row("SELECT MAX(event_timestamp) FROM normalized_event WHERE provider=?1 AND session_id=?2", params![provider.label(), id], |row| row.get(0)).map_err(io::Error::other)?;
            candidate.last_activity = candidate
                .last_activity
                .max(activity.unwrap_or(0).max(0) as u64);
        }
        if provider == AgentKind::Codex {
            for path in codex_databases(codex_home)? {
                if path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("state_")
                {
                    let native = read_database(&path)?;
                    let activity: Option<i64> = native
                        .query_row("SELECT updated_at FROM threads WHERE id=?1", [id], |row| {
                            row.get(0)
                        })
                        .optional()
                        .map_err(io::Error::other)?;
                    candidate.last_activity = candidate
                        .last_activity
                        .max(activity.unwrap_or(0).max(0) as u64);
                }
            }
        }
        if candidate.last_activity == 0 {
            candidate.last_activity = fallback_activity;
        }
        if candidate.last_activity == 0 || candidate.last_activity >= cutoff {
            continue;
        }
        if let Err(error) = pty::prune_session_terminals(&root.join("pty-daemon.sock"), &key, false)
        {
            eprintln!("Skipped {key}: {error}");
            continue;
        }
        candidates.push(candidate);
    }
    Ok(candidates)
}

fn snapshot(path: &Path, stamps: &mut Vec<(PathBuf, u64, SystemTime)>) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || (!metadata.is_file() && !metadata.is_dir()) {
        return Err(io::Error::other(format!(
            "refusing to prune a symlink or special file: {path:?}"
        )));
    }
    stamps.push((path.to_owned(), metadata.len(), metadata.modified()?));
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            snapshot(&entry?.path(), stamps)?;
        }
    }
    Ok(())
}

fn codex_databases(home: &Path) -> io::Result<Vec<PathBuf>> {
    let mut databases = Vec::new();
    if !home.exists() {
        return Ok(databases);
    }
    for entry in fs::read_dir(home)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if ["state_", "thread_history_", "goals_", "queue_", "memories_"]
            .iter()
            .any(|prefix| {
                name.strip_prefix(prefix)
                    .and_then(|rest| rest.strip_suffix(".sqlite"))
                    .is_some_and(|version| version.parse::<u32>().is_ok())
            })
        {
            if !entry.file_type()?.is_file() {
                return Err(io::Error::other(format!(
                    "invalid provider database: {:?}",
                    entry.path()
                )));
            }
            databases.push(entry.path());
        }
    }
    databases.sort();
    Ok(databases)
}

fn apply(
    root: &Path,
    paths: &DiscoveryPaths,
    days: u64,
    now: u64,
    candidates: &[Candidate],
) -> io::Result<()> {
    if plan(root, paths, days, now)? != candidates {
        return Err(io::Error::other(
            "sessions changed after the preview; run prune-archived again",
        ));
    }
    #[cfg(unix)]
    {
        let processes = std::process::Command::new("ps")
            .args(["-axo", "args="])
            .output()?;
        if !processes.status.success() {
            return Err(io::Error::other("cannot check for open provider sessions"));
        }
        let processes = String::from_utf8_lossy(&processes.stdout);
        for candidate in candidates {
            if processes.lines().any(|line| {
                line.split_whitespace().any(|arg| {
                    arg == candidate.id
                        || candidate
                            .paths
                            .iter()
                            .any(|path| path.to_str() == Some(arg))
                })
            }) {
                return Err(io::Error::other(format!(
                    "close the provider process for {} before pruning",
                    candidate.key
                )));
            }
        }
    }
    for candidate in candidates {
        let mut stamps = Vec::new();
        for path in &candidate.paths {
            snapshot(path, &mut stamps)?;
        }
        stamps.sort();
        if stamps != candidate.stamps {
            return Err(io::Error::other(format!(
                "{} changed; cleanup stopped",
                candidate.key
            )));
        }
        pty::prune_session_terminals(&root.join("pty-daemon.sock"), &candidate.key, true)?;
        remove_provider_records(paths, candidate)?;
        for path in &candidate.paths {
            if path.is_dir() {
                fs::remove_dir_all(path)?;
            } else {
                fs::remove_file(path)?;
            }
        }
        remove_console_records(root, candidate)?;
        println!("Deleted {}", candidate.key);
    }
    Ok(())
}

fn remove_provider_records(paths: &DiscoveryPaths, candidate: &Candidate) -> io::Result<()> {
    let id = candidate.id.as_str();
    let home = paths.root(candidate.provider).parent().unwrap();
    if candidate.provider == AgentKind::Codex {
        for path in codex_databases(home)? {
            let mut db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE)
                .map_err(io::Error::other)?;
            db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=2000;")
                .map_err(io::Error::other)?;
            let transaction = db.transaction().map_err(io::Error::other)?;
            for (table, column) in [
                ("thread_dynamic_tools", "thread_id"),
                ("thread_artifacts", "thread_id"),
                ("thread_spawn_edges", "parent_thread_id"),
                ("thread_spawn_edges", "child_thread_id"),
                ("thread_items", "thread_id"),
                ("thread_turns", "thread_id"),
                ("thread_history_projection_state", "thread_id"),
                ("thread_realtime_items", "thread_id"),
                ("thread_goal_continuation_deferrals", "thread_id"),
                ("thread_goals", "thread_id"),
                ("queued_items", "thread_id"),
                ("queued_thread_revisions", "thread_id"),
                ("stage1_outputs", "thread_id"),
                ("jobs", "job_key"),
                ("threads", "id"),
            ] {
                if table_exists(&transaction, table)? {
                    transaction
                        .execute(&format!("DELETE FROM {table} WHERE {column}=?1"), [id])
                        .map_err(io::Error::other)?;
                }
            }
            transaction.commit().map_err(io::Error::other)?;
        }
        prune_jsonl(&home.join("history.jsonl"), "session_id", id)?;
        prune_jsonl(&home.join("session_index.jsonl"), "id", id)?;
    } else if candidate.provider == AgentKind::Claude {
        prune_jsonl(&home.join("history.jsonl"), "sessionId", id)?;
        let parents = candidate
            .paths
            .iter()
            .filter(|path| {
                path.extension().is_some_and(|ext| ext == "jsonl")
                    && path.starts_with(&paths.claude_projects)
            })
            .filter_map(|path| path.parent())
            .collect::<BTreeSet<_>>();
        for parent in parents {
            rewrite(&parent.join("sessions-index.json"), |bytes| {
                let mut value: Value = serde_json::from_slice(bytes).map_err(io::Error::other)?;
                if let Some(entries) = value.get_mut("entries").and_then(Value::as_array_mut) {
                    entries
                        .retain(|entry| entry.get("sessionId").and_then(Value::as_str) != Some(id));
                }
                serde_json::to_vec_pretty(&value).map_err(io::Error::other)
            })?;
        }
    }
    Ok(())
}

fn remove_console_records(root: &Path, candidate: &Candidate) -> io::Result<()> {
    let mut db =
        Connection::open_with_flags(root.join("state.db"), OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(io::Error::other)?;
    let transaction = db.transaction().map_err(io::Error::other)?;
    if table_exists(&transaction, "normalized_event")? {
        transaction
            .execute(
                "DELETE FROM normalized_event WHERE provider=?1 AND session_id=?2",
                params![candidate.provider.label(), candidate.id],
            )
            .map_err(io::Error::other)?;
    }
    if table_exists(&transaction, "event_cursor")? {
        for suffix in [".jsonl", ".jsonl.1"] {
            let path = root
                .join("events")
                .join(format!("{}-{}{suffix}", candidate.provider, candidate.id));
            transaction
                .execute(
                    "DELETE FROM event_cursor WHERE source_path=?1",
                    [path.to_string_lossy().as_ref()],
                )
                .map_err(io::Error::other)?;
        }
    }
    transaction
        .execute(
            "DELETE FROM session_cache WHERE session_key=?1",
            [&candidate.key],
        )
        .map_err(io::Error::other)?;
    transaction.commit().map_err(io::Error::other)?;
    rewrite(&root.join("state.json"), |bytes| {
        let mut value: Value = serde_json::from_slice(bytes).map_err(io::Error::other)?;
        if let Some(sessions) = value.get_mut("sessions").and_then(Value::as_object_mut) {
            sessions.remove(&candidate.key);
        }
        serde_json::to_vec_pretty(&value).map_err(io::Error::other)
    })
}

fn prune_jsonl(path: &Path, field: &str, id: &str) -> io::Result<()> {
    rewrite(path, |bytes| {
        Ok(bytes
            .split_inclusive(|b| *b == b'\n')
            .filter(|line| {
                serde_json::from_slice::<Value>(line)
                    .ok()
                    .and_then(|value| {
                        value
                            .get(field)
                            .and_then(Value::as_str)
                            .map(|value| value == id)
                    })
                    != Some(true)
            })
            .flatten()
            .copied()
            .collect())
    })
}

fn rewrite(path: &Path, transform: impl FnOnce(&[u8]) -> io::Result<Vec<u8>>) -> io::Result<()> {
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() {
        return Err(io::Error::other(format!(
            "refusing to rewrite a non-file: {path:?}"
        )));
    }
    let mut file = fs::OpenOptions::new().read(true).write(true).open(path)?;
    file.try_lock().map_err(io::Error::other)?;
    let mut original = Vec::new();
    file.read_to_end(&mut original)?;
    let replacement = transform(&original)?;
    if original == replacement {
        return Ok(());
    }
    let temporary = path.with_extension(format!("prune-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut output = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        output.set_permissions(metadata.permissions())?;
        output.write_all(&replacement)?;
        output.sync_all()?;
        let mut current = Vec::new();
        file.rewind()?;
        file.read_to_end(&mut current)?;
        if current != original || fs::read(path)? != original {
            return Err(io::Error::other(format!(
                "shared history changed during cleanup: {path:?}"
            )));
        }
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{process::Stdio, time::Duration};
    use tempfile::{TempDir, tempdir};

    const CODEX: &str = "11111111-1111-4111-8111-111111111111";
    const CLAUDE: &str = "22222222-2222-4222-8222-222222222222";
    const KEPT: &str = "44444444-4444-4444-8444-444444444444";

    fn write(path: &Path, value: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, value).unwrap();
    }

    fn fixture(codex_id: &str) -> (TempDir, PathBuf, DiscoveryPaths, u64) {
        let temp = tempdir().unwrap();
        let root = temp.path().join("console");
        let paths = DiscoveryPaths {
            codex_sessions: temp.path().join("codex/sessions"),
            claude_projects: temp.path().join("claude/projects"),
            pi_sessions: temp.path().join("pi/sessions"),
        };
        drop(store::StateStore::load(root.clone()).unwrap());
        let db = Connection::open(root.join("state.db")).unwrap();
        db.execute_batch("CREATE TABLE normalized_event(provider TEXT, session_id TEXT, event_timestamp INTEGER); CREATE TABLE event_cursor(source_path TEXT);").unwrap();
        let now = crate::model::unix_timestamp();
        let old = now - 8 * DAY;
        let mut legacy = serde_json::Map::new();
        for (provider, id, archived, timestamp) in [
            (AgentKind::Codex, codex_id, true, old),
            (AgentKind::Claude, CLAUDE, true, old),
            (AgentKind::Pi, "old-pi", true, old),
            (AgentKind::Codex, KEPT, false, old),
            (
                AgentKind::Codex,
                "55555555-5555-4555-8555-555555555555",
                true,
                now,
            ),
        ] {
            let key = format!("{provider}:{id}");
            let record = match provider {
                AgentKind::Codex => {
                    json!({"type":"session_meta","payload":{"id":id,"cwd":temp.path()}})
                }
                AgentKind::Claude => {
                    json!({"type":"user","sessionId":id,"cwd":temp.path(),"message":{"role":"user","content":"Old task"}})
                }
                AgentKind::Pi => json!({"type":"session","id":id,"cwd":temp.path()}),
            };
            let name = match provider {
                AgentKind::Codex => format!("rollout-{id}.jsonl"),
                AgentKind::Claude => format!("project/{id}.jsonl"),
                AgentKind::Pi => format!("project/2026-09-01_{id}.jsonl"),
            };
            let path = paths.root(provider).join(name);
            write(&path, format!("{record}\n").as_bytes());
            fs::File::open(path)
                .unwrap()
                .set_modified(UNIX_EPOCH + Duration::from_secs(timestamp))
                .unwrap();
            let cached = store::CachedSession {
                archived,
                first_prompt: Some(format!("Task {id}")),
                summary_updated_at: Some(now),
                summary_fingerprint: format!("{timestamp}:123"),
                ..store::CachedSession::default()
            };
            let value = serde_json::to_value(cached).unwrap();
            db.execute(
                "INSERT INTO session_cache VALUES (?1,?2)",
                params![key, value.to_string()],
            )
            .unwrap();
            legacy.insert(key, value);
        }
        write(
            &root.join("state.json"),
            json!({"version":1,"sessions":legacy})
                .to_string()
                .as_bytes(),
        );
        db.execute(
            "INSERT INTO normalized_event VALUES ('codex',?1,?2),('claude',?1,?2)",
            params![codex_id, old as i64],
        )
        .unwrap();
        let event_path = root.join("events").join(format!("codex-{codex_id}.jsonl"));
        write(&event_path, b"{}\n");
        fs::File::open(&event_path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(old))
            .unwrap();
        db.execute(
            "INSERT INTO event_cursor VALUES (?1)",
            [event_path.to_str().unwrap()],
        )
        .unwrap();
        let codex_home = paths.codex_sessions.parent().unwrap();
        let native = Connection::open(codex_home.join("state_5.sqlite")).unwrap();
        native.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE threads(id TEXT PRIMARY KEY, updated_at INTEGER); CREATE TABLE thread_dynamic_tools(thread_id TEXT REFERENCES threads(id));").unwrap();
        native
            .execute(
                "INSERT INTO threads VALUES (?1,?3),(?2,?3)",
                params![codex_id, KEPT, old as i64],
            )
            .unwrap();
        native
            .execute(
                "INSERT INTO thread_dynamic_tools VALUES (?1),(?2)",
                [codex_id, KEPT],
            )
            .unwrap();
        let goals = Connection::open(codex_home.join("goals_1.sqlite")).unwrap();
        goals
            .execute_batch("CREATE TABLE thread_goals(thread_id TEXT PRIMARY KEY);")
            .unwrap();
        goals
            .execute(
                "INSERT INTO thread_goals VALUES (?1),(?2)",
                [codex_id, KEPT],
            )
            .unwrap();
        for (name, field) in [
            ("history.jsonl", "session_id"),
            ("session_index.jsonl", "id"),
        ] {
            write(
                &codex_home.join(name),
                format!("{}\n{}\n", json!({field:codex_id}), json!({field:KEPT})).as_bytes(),
            );
        }
        let claude_home = paths.claude_projects.parent().unwrap();
        let registry = claude_home.join("sessions/0.json");
        write(
            &registry,
            format!(
                "{}stale trailing bytes",
                json!({"sessionId":CLAUDE,"pid":0})
            )
            .as_bytes(),
        );
        fs::File::open(registry)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(old))
            .unwrap();
        write(
            &claude_home.join("history.jsonl"),
            format!(
                "{}\n{}\n",
                json!({"sessionId":CLAUDE}),
                json!({"sessionId":KEPT})
            )
            .as_bytes(),
        );
        write(
            &paths.claude_projects.join("project/sessions-index.json"),
            json!({"version":1,"entries":[{"sessionId":CLAUDE},{"sessionId":KEPT}]})
                .to_string()
                .as_bytes(),
        );
        let artifacts = paths.claude_projects.join("project").join(CLAUDE);
        write(&artifacts.join("subagents/agent-test.jsonl"), b"{}\n");
        let mut stamps = Vec::new();
        snapshot(&artifacts, &mut stamps).unwrap();
        for (path, _, _) in stamps {
            fs::File::open(path)
                .unwrap()
                .set_modified(UNIX_EPOCH + Duration::from_secs(old))
                .unwrap();
        }
        (temp, root, paths, now)
    }

    #[test]
    fn confirmed_prune_removes_all_three_providers_and_preserves_other_sessions() {
        let (_temp, root, paths, now) = fixture(CODEX);
        let candidates = plan(&root, &paths, 7, now).unwrap();
        assert_eq!(candidates.len(), 3);
        assert!(plan(&root, &paths, 30, now).unwrap().is_empty());
        for answer in ["yes\n", "\n", "DELETE 2 ARCHIVED SESSIONS\n", ""] {
            assert!(!confirm(&mut answer.as_bytes(), &mut Vec::new(), 3).unwrap());
            assert_eq!(plan(&root, &paths, 7, now).unwrap(), candidates);
        }
        assert!(
            confirm(
                &mut b"DELETE 3 ARCHIVED SESSIONS\n".as_slice(),
                &mut Vec::new(),
                3
            )
            .unwrap()
        );
        apply(&root, &paths, 7, now, &candidates).unwrap();
        assert!(
            candidates
                .iter()
                .flat_map(|c| &c.paths)
                .all(|path| !path.exists())
        );
        let db = read_database(&root.join("state.db")).unwrap();
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM session_cache", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            db.query_row("SELECT provider FROM normalized_event", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "claude"
        );
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM event_cursor", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        let codex_home = paths.codex_sessions.parent().unwrap();
        for (name, table, column) in [
            ("state_5.sqlite", "threads", "id"),
            ("state_5.sqlite", "thread_dynamic_tools", "thread_id"),
            ("goals_1.sqlite", "thread_goals", "thread_id"),
        ] {
            let db = read_database(&codex_home.join(name)).unwrap();
            assert_eq!(
                db.query_row(&format!("SELECT {column} FROM {table}"), [], |r| r
                    .get::<_, String>(0))
                    .unwrap(),
                KEPT
            );
        }
        for path in [
            codex_home.join("history.jsonl"),
            codex_home.join("session_index.jsonl"),
            paths
                .claude_projects
                .parent()
                .unwrap()
                .join("history.jsonl"),
            paths.claude_projects.join("project/sessions-index.json"),
        ] {
            let text = fs::read_to_string(path).unwrap();
            assert!(!text.contains(CODEX) && !text.contains(CLAUDE));
            assert!(text.contains(KEPT));
        }
        let legacy: Value =
            serde_json::from_slice(&fs::read(root.join("state.json")).unwrap()).unwrap();
        assert_eq!(legacy["sessions"].as_object().unwrap().len(), 2);
        assert!(run(["--yes".into()].into_iter()).is_err());
    }

    #[test]
    fn activity_after_preview_and_open_provider_processes_prevent_deletion() {
        let id = uuid::Uuid::new_v4().to_string();
        let (_temp, root, paths, now) = fixture(&id);
        let candidates = plan(&root, &paths, 7, now).unwrap();
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "read line", &id])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let result = apply(&root, &paths, 7, now, &candidates);
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(result.unwrap_err().to_string().contains("provider process"));
        let transcript = paths.codex_sessions.join(format!("rollout-{id}.jsonl"));
        fs::OpenOptions::new()
            .append(true)
            .open(transcript)
            .unwrap()
            .write_all(b"{}\n")
            .unwrap();
        assert!(
            apply(&root, &paths, 7, now, &candidates)
                .unwrap_err()
                .to_string()
                .contains("changed")
        );
        assert!(
            candidates
                .iter()
                .flat_map(|c| &c.paths)
                .all(|path| path.exists())
        );
    }

    #[test]
    fn cleanup_lock_excludes_dashboards_and_symlink_artifacts_are_rejected() {
        let (_temp, root, paths, now) = fixture(CODEX);
        let (store, _) = store::StateStore::load(root.clone()).unwrap();
        let lock = store::maintenance_lock(&root).unwrap();
        assert!(lock.try_lock().is_err());
        drop(store);
        lock.try_lock().unwrap();
        assert!(store::StateStore::load(root.clone()).is_err());
        let outside = root.join("keep.txt");
        write(&outside, b"keep");
        let link = paths
            .claude_projects
            .join("project")
            .join(CLAUDE)
            .join("linked.txt");
        std::os::unix::fs::symlink(&outside, link).unwrap();
        assert!(plan(&root, &paths, 7, now).is_err());
        assert_eq!(fs::read(outside).unwrap(), b"keep");
    }

    #[test]
    fn concurrent_history_updates_are_preserved() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("history.jsonl");
        write(&path, b"original\n");
        assert!(
            rewrite(&path, |_| {
                fs::write(&path, b"new activity\n")?;
                Ok(Vec::new())
            })
            .is_err()
        );
        assert_eq!(fs::read(path).unwrap(), b"new activity\n");
    }
}
