use std::{
    env, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;

use crate::{
    config::AgentConsoleConfig,
    model::{AgentKind, SessionSummary},
    store::{ensure_private_dir, write_private},
};

const MAX_PROMPT_BYTES: usize = 48 * 1024;
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SummaryBackend {
    SameProvider,
    Off,
}

impl SummaryBackend {
    pub fn from_environment() -> Self {
        match env::var("AGENT_CONSOLE_SUMMARIZER").as_deref() {
            Ok("off") => Self::Off,
            _ => Self::SameProvider,
        }
    }

    fn provider_for(self, session_provider: AgentKind) -> Option<AgentKind> {
        match self {
            Self::SameProvider => Some(session_provider),
            Self::Off => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SummaryJob {
    pub session_key: String,
    pub provider: AgentKind,
    pub fingerprint: String,
    pub previous: SessionSummary,
    pub records: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct SummaryResult {
    pub session_key: String,
    pub fingerprint: String,
    pub result: Result<SessionSummary, String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SummaryCommand {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
}

pub struct SummaryWorker {
    jobs: Option<Sender<SummaryJob>>,
    results: Receiver<SummaryResult>,
    pub backend: SummaryBackend,
    cancel: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl SummaryWorker {
    pub fn start(
        backend: SummaryBackend,
        state_dir: PathBuf,
        schema_path: PathBuf,
        config: AgentConsoleConfig,
    ) -> io::Result<Self> {
        ensure_private_dir(&state_dir)?;
        ensure_schema(&schema_path)?;
        let (job_tx, job_rx) = mpsc::channel::<SummaryJob>();
        let (result_tx, result_rx) = mpsc::channel::<SummaryResult>();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_thread = Arc::clone(&cancel);
        let handle = thread::Builder::new()
            .name("agent-console-summary".into())
            .spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    if cancel_for_thread.load(Ordering::Acquire) {
                        break;
                    }
                    let result = run_job(
                        backend,
                        &state_dir,
                        &schema_path,
                        &config,
                        &job,
                        &cancel_for_thread,
                    );
                    let _ = result_tx.send(SummaryResult {
                        session_key: job.session_key,
                        fingerprint: job.fingerprint,
                        result,
                    });
                }
            })
            .map_err(io::Error::other)?;
        Ok(Self {
            jobs: Some(job_tx),
            results: result_rx,
            backend,
            cancel,
            handle: Some(handle),
        })
    }

    pub fn enqueue(&self, job: SummaryJob) -> Result<(), String> {
        if self.backend == SummaryBackend::Off {
            return Err("summaries are disabled".into());
        }
        self.jobs
            .as_ref()
            .ok_or_else(|| "summary worker is stopped".to_owned())?
            .send(job)
            .map_err(|error| error.to_string())
    }

    pub fn try_result(&self) -> Option<SummaryResult> {
        self.results.try_recv().ok()
    }
}

impl Drop for SummaryWorker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.jobs.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

pub fn build_prompt(previous: &SessionSummary, records: &[String]) -> String {
    let previous = serde_json::to_string(previous).unwrap_or_else(|_| "{}".into());
    let mut records = records
        .iter()
        .map(|record| sanitize(record))
        .filter(|record| !record.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let prefix = format!(
        "You are a read-only sidecar summarizer for an AI coding session.\n\
         Return only JSON matching the supplied schema. Use concrete facts from the input.\n\
         Never invent completed work, blockers, or user decisions. Keep each string concise.\n\
         The application separately determines status and approval state, so those fields are advisory.\n\
         Previous summary:\n{previous}\n\
         New session records:\n"
    );
    let marker = "[older records truncated]\n";
    let available = MAX_PROMPT_BYTES.saturating_sub(prefix.len() + 1);
    if records.len() > available {
        let tail_bytes = available.saturating_sub(marker.len());
        let mut start = records.len() - tail_bytes;
        while !records.is_char_boundary(start) {
            start += 1;
        }
        records = format!("{marker}{}", &records[start..]);
    }
    format!("{prefix}{records}\n")
}

fn command_for(
    config: &AgentConsoleConfig,
    provider: AgentKind,
    neutral_cwd: &Path,
    schema_path: &Path,
) -> SummaryCommand {
    let args = match provider {
        AgentKind::Codex => vec![
            "exec".into(),
            "--ephemeral".into(),
            "--sandbox".into(),
            "read-only".into(),
            "--ignore-user-config".into(),
            "--skip-git-repo-check".into(),
            "--output-schema".into(),
            schema_path.display().to_string(),
            "-".into(),
        ],
        AgentKind::Claude => vec![
            "--safe-mode".into(),
            "--print".into(),
            "--tools".into(),
            String::new(),
            "--no-session-persistence".into(),
            "--output-format".into(),
            "json".into(),
            "--json-schema".into(),
            summary_schema().to_string(),
        ],
    };
    let command = config.provider_command(provider, args);
    SummaryCommand {
        program: command.program.to_string_lossy().into_owned(),
        args: command
            .args
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect(),
        cwd: neutral_cwd.to_owned(),
    }
}

fn run_job(
    backend: SummaryBackend,
    state_dir: &Path,
    schema_path: &Path,
    config: &AgentConsoleConfig,
    job: &SummaryJob,
    cancel: &AtomicBool,
) -> Result<SessionSummary, String> {
    let provider = backend
        .provider_for(job.provider)
        .ok_or_else(|| "summaries are disabled".to_owned())?;
    let command = command_for(config, provider, state_dir, schema_path);
    let prompt = build_prompt(&job.previous, &job.records);
    let output = run_with_timeout(&command, prompt.as_bytes(), SUMMARY_TIMEOUT, cancel)?;
    parse_output(provider, &output)
}

fn run_with_timeout(
    spec: &SummaryCommand,
    input: &[u8],
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, String> {
    let mut child = spawn_summary_process(spec)?;
    child
        .stdin
        .take()
        .ok_or_else(|| "summarizer stdin unavailable".to_owned())?
        .write_all(input)
        .map_err(|error| format!("cannot send summary input: {error}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "summarizer stdout unavailable".to_owned())?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "summarizer stderr unavailable".to_owned())?;
    let stdout_thread = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let stderr_thread = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    });

    let start = Instant::now();
    let status = loop {
        if cancel.load(Ordering::Acquire) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            return Err("summary cancelled".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() < timeout => thread::sleep(Duration::from_millis(100)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err("summary timed out".into());
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(format!("cannot wait for summarizer: {error}"));
            }
        }
    };
    let stdout = stdout_thread
        .join()
        .map_err(|_| "summary stdout reader panicked".to_owned())?;
    let stderr = stderr_thread
        .join()
        .map_err(|_| "summary stderr reader panicked".to_owned())?;
    if !status.success() {
        let detail = if stderr.is_empty() { &stdout } else { &stderr };
        let error = String::from_utf8_lossy(detail);
        return Err(format!(
            "summarizer exited with {status}: {}",
            sanitize(error.trim()).chars().take(240).collect::<String>()
        ));
    }
    Ok(stdout)
}

fn spawn_summary_process(spec: &SummaryCommand) -> Result<Child, String> {
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        // SAFETY: this callback runs in the child after fork and only invokes
        // the async-signal-safe setsid syscall before exec.
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
    command
        .spawn()
        .map_err(|error| format!("cannot start {}: {error}", spec.program))
}

fn parse_output(provider: AgentKind, bytes: &[u8]) -> Result<SessionSummary, String> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("summarizer returned invalid JSON: {error}"))?;
    let structured = match provider {
        AgentKind::Codex => value,
        AgentKind::Claude => {
            if let Some(value) = value.get("structured_output") {
                value.clone()
            } else if let Some(value) = value.get("result") {
                match value {
                    Value::String(text) => serde_json::from_str(text).map_err(|error| {
                        format!("Claude result is not structured JSON: {error}")
                    })?,
                    other => other.clone(),
                }
            } else {
                return Err("Claude output has no structured_output or result".into());
            }
        }
    };
    serde_json::from_value(structured)
        .map_err(|error| format!("summary does not match the schema: {error}"))
}

pub fn ensure_schema(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(&summary_schema()).map_err(io::Error::other)?;
    if fs::read(path).ok().as_deref() == Some(bytes.as_slice()) {
        return Ok(());
    }
    let temporary = path.with_extension("json.tmp");
    write_private(&temporary, &bytes)?;
    fs::rename(temporary, path)
}

fn summary_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "task": {"type": "string"},
            "status": {"type": "string", "enum": ["working", "waiting", "idle", "failed"]},
            "progress": {"type": "array", "items": {"type": "string"}},
            "current_action": {"type": "string"},
            "next_step": {"type": "string"},
            "needs_user": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "id": {"type": "string"},
                        "question": {"type": "string"}
                    },
                    "required": ["id", "question"]
                }
            },
            "blockers": {"type": "array", "items": {"type": "string"}}
        },
        "required": ["task", "status", "progress", "current_action", "next_step", "needs_user", "blockers"]
    })
}

fn sanitize(value: &str) -> String {
    let without_ansi = strip_ansi(value);
    without_ansi
        .lines()
        .map(redact_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn redact_line(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    if let Some(index) = lower.find("bearer ") {
        let value_start = index + "bearer ".len();
        let end = line[value_start..]
            .find(char::is_whitespace)
            .map_or(line.len(), |relative| value_start + relative);
        return format!("{}[REDACTED]{}", &line[..value_start], &line[end..]);
    }
    for marker in ["api_key=", "apikey=", "token=", "password=", "secret="] {
        if let Some(index) = lower.find(marker) {
            let value_start = index + marker.len();
            let value_end = line[value_start..]
                .find(char::is_whitespace)
                .map_or(line.len(), |relative| value_start + relative);
            return format!("{}[REDACTED]{}", &line[..value_start], &line[value_end..]);
        }
    }
    line.to_owned()
}

fn strip_ansi(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut state = 0_u8;
    for character in value.chars() {
        match state {
            0 if character == '\x1b' => state = 1,
            0 => output.push(character),
            1 if character == '[' => state = 2,
            1 if character == ']' => state = 3,
            1 => state = 0,
            2 if ('@'..='~').contains(&character) => state = 0,
            2 => {}
            3 if character == '\u{7}' => state = 0,
            3 => {}
            _ => state = 0,
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::model::SessionStatus;

    #[test]
    fn rolling_prompt_redacts_secrets_and_is_bounded() {
        let previous = SessionSummary {
            task: "existing".into(),
            ..SessionSummary::default()
        };
        let records = vec![
            "x".repeat(MAX_PROMPT_BYTES * 2),
            "Authorization: Bearer secret-value next".into(),
            "API_KEY=abc123 cargo test".into(),
        ];
        let prompt = build_prompt(&previous, &records);
        assert!(!prompt.contains("secret-value"));
        assert!(!prompt.contains("abc123"));
        assert!(prompt.contains("[REDACTED]"));
        assert!(prompt.len() <= MAX_PROMPT_BYTES);
    }

    #[test]
    fn provider_commands_are_isolated_and_non_persistent() {
        let root = Path::new("/tmp/neutral");
        let schema = root.join("schema.json");
        let config = AgentConsoleConfig::default();
        let codex = command_for(&config, AgentKind::Codex, root, &schema);
        assert!(codex.args.contains(&"--ephemeral".into()));
        assert!(codex.args.contains(&"read-only".into()));
        let claude = command_for(&config, AgentKind::Claude, root, &schema);
        assert!(claude.args.contains(&"--safe-mode".into()));
        assert!(claude.args.contains(&"--no-session-persistence".into()));
    }

    #[test]
    fn summary_uses_the_same_configured_provider_command() {
        let config = AgentConsoleConfig::parse(
            "[providers]\nclaude = [\"env\", \"HTTPS_PROXY=http://127.0.0.1:7890\", \"claude\"]\n",
            Path::new("config.toml"),
        )
        .unwrap();
        let root = Path::new("/tmp/neutral");
        let command = command_for(&config, AgentKind::Claude, root, &root.join("schema.json"));

        assert_eq!(command.program, "env");
        assert_eq!(command.args[0], "HTTPS_PROXY=http://127.0.0.1:7890");
        assert_eq!(command.args[1], "claude");
        assert!(command.args.contains(&"--no-session-persistence".into()));
    }

    #[test]
    fn summary_can_use_a_shell_alias() {
        let alias = "agent_console_test_summary_alias_that_does_not_exist";
        let config = AgentConsoleConfig::parse(
            &format!("[providers]\nclaude = [\"{alias}\"]\n"),
            Path::new("config.toml"),
        )
        .unwrap();
        let root = Path::new("/tmp/neutral");
        let command = command_for(&config, AgentKind::Claude, root, &root.join("schema.json"));

        assert_eq!(command.args[0], "-ic");
        assert_eq!(command.args[1], format!(r#"{alias} "$@""#));
        assert_eq!(command.args[2], "agent-console-provider");
        assert!(command.args.contains(&"--no-session-persistence".into()));
    }

    #[cfg(unix)]
    #[test]
    fn summary_process_starts_in_its_own_session() {
        let command = SummaryCommand {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "sleep 2".into()],
            cwd: PathBuf::from("/tmp"),
        };
        let mut child = spawn_summary_process(&command).unwrap();
        let pid = child.id() as libc::pid_t;

        // SAFETY: pid is the live child returned by spawn_summary_process.
        let session = unsafe { libc::getsid(pid) };
        let _ = child.kill();
        let _ = child.wait();

        assert_eq!(session, pid, "summary child inherited the parent session");
    }

    #[test]
    fn summary_backend_never_crosses_session_provider() {
        assert_eq!(
            SummaryBackend::SameProvider.provider_for(AgentKind::Codex),
            Some(AgentKind::Codex)
        );
        assert_eq!(
            SummaryBackend::SameProvider.provider_for(AgentKind::Claude),
            Some(AgentKind::Claude)
        );
        assert_eq!(SummaryBackend::Off.provider_for(AgentKind::Codex), None);
    }

    #[test]
    fn parses_codex_and_claude_structured_output() {
        let summary = serde_json::json!({
            "task": "fix auth",
            "status": "working",
            "progress": ["read tests"],
            "current_action": "editing",
            "next_step": "test",
            "needs_user": [],
            "blockers": []
        });
        let codex = parse_output(AgentKind::Codex, summary.to_string().as_bytes()).unwrap();
        assert_eq!(codex.status, SessionStatus::Working);
        let claude_wrapper = serde_json::json!({"structured_output": summary});
        let claude =
            parse_output(AgentKind::Claude, claude_wrapper.to_string().as_bytes()).unwrap();
        assert_eq!(claude.task, "fix auth");
    }

    #[test]
    fn schema_is_written_atomically() {
        let root = tempdir().unwrap();
        let path = root.path().join("schema.json");
        ensure_schema(&path).unwrap();
        assert!(serde_json::from_slice::<Value>(&fs::read(path).unwrap()).is_ok());
    }
}
