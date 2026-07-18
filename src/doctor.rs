use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use uuid::Uuid;

use crate::{
    config::AgentConsoleConfig,
    events::{self, EventIndex},
    model::AgentKind,
    pty,
    store::StateStore,
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(8);

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

pub fn version_support(provider: AgentKind, version: &str) -> VersionSupport {
    let Some((major, minor)) = numeric_version(version) else {
        return VersionSupport::Unknown;
    };
    let minimum = match provider {
        AgentKind::Codex => (0, 100),
        AgentKind::Claude => (2, 0),
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
    match executable_on_path("pbcopy") {
        Some(path) => ProviderStatus::Available(path.display().to_string()),
        None => ProviderStatus::Unavailable("pbcopy is not available on PATH".into()),
    }
}

pub fn check_daemon(state_root: &Path) -> ProviderStatus {
    let socket = state_root.join("pty-daemon.sock");
    match pty::daemon_health(&socket) {
        Ok(Some(())) => ProviderStatus::Available(format!("healthy at {}", socket.display())),
        Ok(None) => {
            ProviderStatus::Available("not running; will start on first agent entry".into())
        }
        Err(error) => ProviderStatus::Unavailable(format!("{}: {error}", socket.display())),
    }
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
