use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde::Serialize;
use uuid::Uuid;

use crate::{
    config::AgentConsoleConfig,
    diagnostics,
    events::{self, EventIndex},
    model::AgentKind,
    providers, pty, store,
    store::StateStore,
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(8);

/// Every capability `doctor` probes, in the order it probes them.
pub const CAPABILITIES: [ProviderCapability; 3] = [
    ProviderCapability::Resume,
    ProviderCapability::Hooks,
    ProviderCapability::Summary,
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderStatus {
    Available(String),
    Unavailable(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VersionSupport {
    Supported,
    TooOld,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderCapability {
    Resume,
    Hooks,
    Summary,
}

impl ProviderCapability {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Resume => "resume",
            Self::Hooks => "hooks",
            Self::Summary => "summary invocation",
        }
    }
}

pub fn check_configured_provider(
    config: &AgentConsoleConfig,
    provider: AgentKind,
) -> ProviderStatus {
    let invocation = config.provider_command(provider, ["--version"]);
    check_command(Command::new(invocation.program).args(invocation.args))
}

pub fn check_provider_capability(
    config: &AgentConsoleConfig,
    provider: AgentKind,
    capability: ProviderCapability,
) -> ProviderStatus {
    let (arguments, required): (&[&str], &[&str]) = match (provider, capability) {
        (AgentKind::Codex, ProviderCapability::Resume) => (&["--help"], &["resume"]),
        (AgentKind::Codex, ProviderCapability::Hooks) => (&["--help"], &["--config"]),
        (AgentKind::Codex, ProviderCapability::Summary) => {
            (&["exec", "--help"], &["--ephemeral", "--output-schema"])
        }
        (AgentKind::Claude, ProviderCapability::Resume) => (&["--help"], &["--resume"]),
        (AgentKind::Claude, ProviderCapability::Hooks) => (&["--help"], &["--settings"]),
        (AgentKind::Claude, ProviderCapability::Summary) => (
            &["--help"],
            &["--print", "--output-format", "--json-schema"],
        ),
        // pi resumes by id rather than by subcommand, reports progress through extensions
        // rather than hook commands, and summarizes through its non-interactive print mode.
        (AgentKind::Pi, ProviderCapability::Resume) => (&["--help"], &["--session-id"]),
        (AgentKind::Pi, ProviderCapability::Hooks) => (&["--help"], &["--extension"]),
        (AgentKind::Pi, ProviderCapability::Summary) => (
            &["--help"],
            &["--print", "--append-system-prompt", "--no-tools"],
        ),
    };
    let invocation = config.provider_command(provider, arguments);
    let output = match command_output(Command::new(invocation.program).args(invocation.args)) {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            return ProviderStatus::Unavailable(format!("help exited with {}", output.status));
        }
        Err(error) => return ProviderStatus::Unavailable(error),
    };
    let text = combined_output(&output).to_ascii_lowercase();
    let missing = required
        .iter()
        .filter(|token| !text.contains(*token))
        .copied()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        ProviderStatus::Available(format!("supports {}", required.join(", ")))
    } else {
        ProviderStatus::Unavailable(format!("help is missing {}", missing.join(", ")))
    }
}

impl VersionSupport {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::TooOld => "too_old",
            Self::Unknown => "unknown",
        }
    }
}

/// Everything `agent-console doctor` checks, as data.
///
/// The CLI and the web endpoint both render this rather than each running their own
/// sequence of probes, so the two can never disagree about what "healthy" means.
#[derive(Clone, Debug, Serialize)]
pub struct DoctorReport {
    pub version: &'static str,
    pub providers_enabled: Vec<&'static str>,
    pub providers: Vec<ProviderReport>,
    /// Where each enabled provider's transcripts are looked for. Empty when the home
    /// directory cannot be resolved, which is the one case `doctor` prints nothing for.
    pub discovery: Vec<PathReport>,
    pub checks: Vec<CheckReport>,
    pub diagnostics_path: Option<String>,
    pub failures: usize,
    /// True under exactly the condition that makes `agent-console doctor` exit zero: at
    /// least one provider answered `--version`, and no required capability failed.
    pub ok: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProviderReport {
    pub name: &'static str,
    pub available: bool,
    /// The version line when the provider answered, otherwise why it did not.
    pub detail: String,
    /// Absent for a provider that never answered, since there is no version to judge.
    pub version_support: Option<&'static str>,
    pub capabilities: Vec<CheckReport>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PathReport {
    pub name: &'static str,
    pub path: String,
    pub exists: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct CheckReport {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

impl CheckReport {
    fn new(name: impl Into<String>, status: ProviderStatus) -> Self {
        let (ok, detail) = match status {
            ProviderStatus::Available(detail) => (true, detail),
            ProviderStatus::Unavailable(detail) => (false, detail),
        };
        Self {
            name: name.into(),
            ok,
            detail,
        }
    }
}

/// Runs every probe once and collects the results.
///
/// Blocking and slow by nature -- it spawns provider binaries with an 8s timeout each -- so
/// an async caller has to run it off the request thread.
pub fn report() -> io::Result<DoctorReport> {
    let config = AgentConsoleConfig::load()?;
    let enabled = providers::enabled();
    let mut provider_reports = Vec::new();
    let mut available = 0;
    let mut failures = 0;

    for provider in enabled.iter().map(|adapter| adapter.kind) {
        let name = provider.label();
        match check_configured_provider(&config, provider) {
            ProviderStatus::Available(version) => {
                available += 1;
                let support = version_support(provider, &version);
                if support == VersionSupport::TooOld {
                    failures += 1;
                }
                let capabilities = CAPABILITIES
                    .iter()
                    .map(|capability| {
                        CheckReport::new(
                            format!("{name} {}", capability.label()),
                            check_provider_capability(&config, provider, *capability),
                        )
                    })
                    .collect::<Vec<_>>();
                failures += capabilities.iter().filter(|check| !check.ok).count();
                provider_reports.push(ProviderReport {
                    name,
                    available: true,
                    detail: version,
                    version_support: Some(support.label()),
                    capabilities,
                });
            }
            // Not a failure: an uninstalled provider is a fact about this machine, not a
            // broken install. Only "no provider at all" is fatal.
            ProviderStatus::Unavailable(error) => provider_reports.push(ProviderReport {
                name,
                available: false,
                detail: error,
                version_support: None,
                capabilities: Vec::new(),
            }),
        }
    }

    let discovery = crate::discovery::DiscoveryPaths::from_environment()
        .map(|paths| {
            [
                (AgentKind::Codex, "Codex sessions", paths.codex_sessions),
                (AgentKind::Claude, "Claude projects", paths.claude_projects),
                (AgentKind::Pi, "pi sessions", paths.pi_sessions),
            ]
            .into_iter()
            .filter(|(kind, _, _)| providers::is_enabled(*kind))
            .map(|(_, name, path)| PathReport {
                name,
                exists: path.is_dir(),
                path: path.display().to_string(),
            })
            .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let state = store::state_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cannot resolve state directory"))?;
    let checks = vec![
        CheckReport::new("state permissions/SQLite", check_state(&state)),
        CheckReport::new("hook ingress/index", check_hook_ingress(&state)),
        CheckReport::new("clipboard", check_clipboard()),
        CheckReport::new("PTY daemon", check_daemon(&state)),
    ];
    failures += checks.iter().filter(|check| !check.ok).count();

    Ok(DoctorReport {
        version: env!("CARGO_PKG_VERSION"),
        providers_enabled: enabled.iter().map(|adapter| adapter.kind.label()).collect(),
        providers: provider_reports,
        discovery,
        checks,
        diagnostics_path: diagnostics::path().map(|path| path.display().to_string()),
        failures,
        ok: available > 0 && failures == 0,
    })
}

pub fn version_support(provider: AgentKind, version: &str) -> VersionSupport {
    let Some((major, minor)) = numeric_version(version) else {
        return VersionSupport::Unknown;
    };
    let minimum = match provider {
        AgentKind::Codex => (0, 100),
        AgentKind::Claude => (2, 0),
        // The oldest pi verified against this console. `--session-id`, `--tui-mode`, and the
        // extension events the hook bridge subscribes to all exist here.
        AgentKind::Pi => (0, 84),
    };
    if (major, minor) >= minimum {
        VersionSupport::Supported
    } else {
        VersionSupport::TooOld
    }
}

pub fn check_hook_ingress(state_root: &Path) -> ProviderStatus {
    let probe_root = state_root.join(format!("doctor-hook-{}", Uuid::new_v4()));
    let result = (|| -> io::Result<()> {
        let events_dir = probe_root.join("events");
        let hook = serde_json::json!({
            "session_id": "doctor-probe",
            "hook_event_name": "PermissionRequest",
            "tool_use_id": "doctor-event",
            "message": "doctor probe"
        });
        let path = events::ingest_hook(AgentKind::Claude, &hook, &events_dir)?;
        let mut index = EventIndex::open(&probe_root)?;
        let indexed = index.refresh_session(&path, AgentKind::Claude, "doctor-probe")?;
        if indexed.len() != 1 || indexed[0].event_id != "doctor-event" {
            return Err(io::Error::other("hook record was not indexed"));
        }
        Ok(())
    })();
    let _ = fs::remove_dir_all(&probe_root);
    match result {
        Ok(()) => ProviderStatus::Available("append, normalize, and index succeeded".into()),
        Err(error) => ProviderStatus::Unavailable(error.to_string()),
    }
}

pub fn check_state(state_root: &Path) -> ProviderStatus {
    let result = (|| -> io::Result<()> {
        let (_store, _) = StateStore::load(state_root.to_owned())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let directory_mode = fs::metadata(state_root)?.permissions().mode() & 0o777;
            let database = state_root.join("state.db");
            let database_mode = fs::metadata(&database)?.permissions().mode() & 0o777;
            if directory_mode != 0o700 || database_mode != 0o600 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "expected directory 0700/database 0600, got {directory_mode:04o}/{database_mode:04o}"
                    ),
                ));
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            ProviderStatus::Available(format!("private SQLite state at {}", state_root.display()))
        }
        Err(error) => ProviderStatus::Unavailable(error.to_string()),
    }
}

pub fn check_clipboard() -> ProviderStatus {
    for program in crate::clipboard::command_names() {
        if let Some(path) = executable_on_path(program) {
            return ProviderStatus::Available(path.display().to_string());
        }
    }
    ProviderStatus::Unavailable(format!(
        "no clipboard command is available on PATH (tried {})",
        crate::clipboard::command_names()
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

#[cfg(unix)]
pub fn check_daemon(state_root: &Path) -> ProviderStatus {
    let socket = state_root.join("pty-daemon.sock");
    match pty::daemon_health(&socket) {
        Ok(Some(())) => check_daemon_protocol(&socket),
        Ok(None) => {
            ProviderStatus::Available("not running; will start on first agent entry".into())
        }
        Err(error) => ProviderStatus::Unavailable(format!("{}: {error}", socket.display())),
    }
}

/// A daemon older than this build still serves terminals, so this is not "unhealthy" -- but
/// it answers polls without the rows above the screen, and a browser terminal opened against
/// it starts at the current screen with nothing to scroll back through. Restarting it is the
/// only cure and it ends every agent it is holding, so the report says that rather than
/// doing it.
#[cfg(unix)]
fn check_daemon_protocol(socket: &Path) -> ProviderStatus {
    match pty::daemon_protocol(socket) {
        Ok(Some(protocol)) if protocol < pty::DAEMON_PROTOCOL => {
            ProviderStatus::Unavailable(format!(
                "{} is running an older build (protocol {protocol}, this one speaks {}); \
                 browser and dashboard terminals open without their earlier output until it \
                 is restarted, which ends every agent terminal it currently holds",
                socket.display(),
                pty::DAEMON_PROTOCOL
            ))
        }
        Ok(_) => ProviderStatus::Available(format!("healthy at {}", socket.display())),
        Err(error) => ProviderStatus::Unavailable(format!("{}: {error}", socket.display())),
    }
}

#[cfg(not(unix))]
pub fn check_daemon(_state_root: &Path) -> ProviderStatus {
    ProviderStatus::Available(
        "process-local PTY mode (detached daemon is unavailable on this platform)".into(),
    )
}

#[cfg(test)]
pub fn check_provider(program: &Path) -> ProviderStatus {
    check_command(Command::new(program).arg("--version"))
}

fn check_command(command: &mut Command) -> ProviderStatus {
    match command_output(command) {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let lines = stdout
                .lines()
                .chain(stderr.lines())
                .filter(|line| !line.trim().is_empty())
                .collect::<Vec<_>>();
            let version = lines
                .iter()
                .copied()
                .find(|line| numeric_version(line).is_some())
                .or_else(|| lines.first().copied())
                .unwrap_or("version command returned no text")
                .trim()
                .to_owned();
            ProviderStatus::Available(version)
        }
        Ok(output) => ProviderStatus::Unavailable(format!("exited with {}", output.status)),
        Err(error) => ProviderStatus::Unavailable(error),
    }
}

fn command_output(command: &mut Command) -> Result<Output, String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let started = Instant::now();
    loop {
        match child.try_wait().map_err(|error| error.to_string())? {
            Some(_) => return child.wait_with_output().map_err(|error| error.to_string()),
            None if started.elapsed() < COMMAND_TIMEOUT => {
                thread::sleep(Duration::from_millis(20));
            }
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out after {}s", COMMAND_TIMEOUT.as_secs()));
            }
        }
    }
}

fn combined_output(output: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn numeric_version(value: &str) -> Option<(u64, u64)> {
    value.split_whitespace().find_map(|token| {
        let token =
            token.trim_matches(|character: char| !character.is_ascii_digit() && character != '.');
        let mut parts = token.split('.');
        Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
    })
}

fn executable_on_path(program: &str) -> Option<PathBuf> {
    env::var_os("PATH").and_then(|path| {
        env::split_paths(&path)
            .map(|directory| directory.join(program))
            .find(|candidate| is_executable(candidate))
    })
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    fn executable(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).unwrap();
        }
    }

    #[test]
    fn reports_fake_and_missing_provider_binaries() {
        let root = tempdir().unwrap();
        let fake = root.path().join("fake-agent");
        executable(&fake, "#!/bin/sh\nprintf 'fake 1.0\\n'\n");
        assert_eq!(
            check_provider(&fake),
            ProviderStatus::Available("fake 1.0".into())
        );
        assert!(matches!(
            check_provider(&root.path().join("missing")),
            ProviderStatus::Unavailable(_)
        ));
    }

    #[test]
    fn provider_help_contracts_detect_resume_hooks_and_summary_flags() {
        let root = tempdir().unwrap();
        let fake = root.path().join("fake-codex");
        executable(
            &fake,
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'codex-cli 0.144.0'; else echo 'resume --config --ephemeral --output-schema'; fi\n",
        );
        let config = AgentConsoleConfig::parse(
            &format!(
                "[providers]\ncodex = [{}]\n",
                toml::Value::String(fake.display().to_string())
            ),
            Path::new("config.toml"),
        )
        .unwrap();

        for capability in [
            ProviderCapability::Resume,
            ProviderCapability::Hooks,
            ProviderCapability::Summary,
        ] {
            assert!(matches!(
                check_provider_capability(&config, AgentKind::Codex, capability),
                ProviderStatus::Available(_)
            ));
        }
    }

    #[test]
    fn version_policy_and_local_state_probes_are_deterministic() {
        assert_eq!(
            version_support(AgentKind::Codex, "codex-cli 0.144.0"),
            VersionSupport::Supported
        );
        assert_eq!(
            version_support(AgentKind::Codex, "codex 0.99.0"),
            VersionSupport::TooOld
        );
        assert_eq!(
            version_support(AgentKind::Claude, "development build"),
            VersionSupport::Unknown
        );

        let root = tempdir().unwrap();
        assert!(matches!(
            check_state(root.path()),
            ProviderStatus::Available(_)
        ));
        assert!(matches!(
            check_hook_ingress(root.path()),
            ProviderStatus::Available(_)
        ));
        assert!(matches!(
            check_daemon(root.path()),
            ProviderStatus::Available(_)
        ));
    }
}
