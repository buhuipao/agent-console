use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
};

use serde::Deserialize;

use crate::model::AgentKind;

const CONFIG_ENV: &str = "AGENT_CONSOLE_CONFIG";

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConsoleConfig {
    #[serde(default)]
    providers: ProviderConfig,
    #[serde(default)]
    pub(crate) summary: SummaryConfig,
    #[serde(default)]
    pub(crate) web: WebConfig,
    #[serde(default)]
    keys: KeyConfig,
}

/// The `[web]` section. Every key is optional and every one of them is only a *default*:
/// the command line and the environment override it (see `web::settings`), so this file is
/// the lowest-priority source rather than the authoritative one.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebConfig {
    /// Whether the dashboard starts its embedded web server. Unset means enabled.
    pub(crate) enabled: Option<bool>,
    /// Bind address. A hostname (`localhost`) is as valid as a literal (`0.0.0.0`); it is
    /// resolved at bind time, not here.
    pub(crate) host: Option<String>,
    pub(crate) port: Option<u16>,
    /// HTTP Basic credentials as `user:password`. Everything after the first colon is the
    /// password, so a password may itself contain colons.
    pub(crate) auth: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct KeyConfig {
    #[serde(default)]
    dashboard: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    workspace: BTreeMap<String, Vec<String>>,
}

const DASHBOARD_ACTIONS: &[&str] = &[
    "quit",
    "next",
    "previous",
    "enter",
    "takeover",
    "shell",
    "copy",
    "stage",
    "new",
    "alert",
    "retry_summary",
    "search",
    "alias",
    "archive",
    "help",
];

const WORKSPACE_ACTIONS: &[&str] = &[
    "focus",
    "new_shell",
    "previous_shell",
    "next_shell",
    "close_shell",
    "dashboard",
    "alert",
    "search",
    "session_alert",
    "help",
    "previous_session",
    "next_session",
    "maximize",
    "hide_shells",
    "grow_shell",
    "shrink_shell",
    "copy_command",
    "scroll_up",
    "scroll_down",
    "live_tail",
    "select_shell_1",
    "select_shell_2",
    "select_shell_3",
    "select_shell_4",
    "select_shell_5",
    "select_shell_6",
    "select_shell_7",
    "select_shell_8",
    "select_shell_9",
];

#[derive(Clone, Debug, Default, Deserialize)]
struct ProviderConfig {
    codex: Option<Vec<String>>,
    claude: Option<Vec<String>>,
    pi: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct SummaryConfig {
    #[serde(default = "default_summary_min_interval")]
    pub(crate) min_interval_seconds: u64,
    #[serde(default = "default_summary_failure_backoff")]
    pub(crate) failure_backoff_seconds: u64,
    #[serde(default = "default_summary_circuit_failures")]
    pub(crate) circuit_failures: u32,
    #[serde(default = "default_summary_circuit_cooldown")]
    pub(crate) circuit_cooldown_seconds: u64,
}

impl Default for SummaryConfig {
    fn default() -> Self {
        Self {
            min_interval_seconds: default_summary_min_interval(),
            failure_backoff_seconds: default_summary_failure_backoff(),
            circuit_failures: default_summary_circuit_failures(),
            circuit_cooldown_seconds: default_summary_circuit_cooldown(),
        }
    }
}

const fn default_summary_min_interval() -> u64 {
    30
}

const fn default_summary_failure_backoff() -> u64 {
    30
}

const fn default_summary_circuit_failures() -> u32 {
    3
}

const fn default_summary_circuit_cooldown() -> u64 {
    300
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderCommand {
    pub program: OsString,
    pub args: Vec<OsString>,
}

impl AgentConsoleConfig {
    pub fn load() -> io::Result<Self> {
        let Some(path) = config_path() else {
            return Ok(Self::default());
        };
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error),
        };
        Self::parse(&text, &path)
    }

    pub fn provider_command<I, T>(&self, provider: AgentKind, dynamic_args: I) -> ProviderCommand
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString>,
    {
        let dynamic_args = dynamic_args
            .into_iter()
            .map(Into::into)
            .collect::<Vec<OsString>>();
        let configured = match provider {
            AgentKind::Codex => self.providers.codex.as_deref(),
            AgentKind::Claude => self.providers.claude.as_deref(),
            AgentKind::Pi => self.providers.pi.as_deref(),
        };
        if let Some([name]) = configured
            && is_shell_name(name)
            && !executable_on_path(name)
        {
            let shell = env::var_os("SHELL").unwrap_or_else(|| OsString::from("/bin/sh"));
            let args = [
                OsString::from("-ic"),
                OsString::from(format!(r#"{name} "$@""#)),
                OsString::from("agent-console-provider"),
            ]
            .into_iter()
            .chain(dynamic_args)
            .collect();
            return ProviderCommand {
                program: shell,
                args,
            };
        }
        let (program, static_args) = configured
            .and_then(|command| command.split_first())
            .map_or_else(
                || (OsString::from(provider.label()), &[][..]),
                |(program, args)| (OsString::from(program), args),
            );
        let args = static_args
            .iter()
            .map(OsString::from)
            .chain(dynamic_args)
            .collect();
        ProviderCommand { program, args }
    }

    pub fn dashboard_action(&self, key: &str) -> Option<&'static str> {
        DASHBOARD_ACTIONS.iter().copied().find(|action| {
            self.dashboard_keys(action)
                .iter()
                .any(|configured| configured.eq_ignore_ascii_case(key))
        })
    }

    pub fn dashboard_keys(&self, action: &str) -> Vec<String> {
        self.keys.dashboard.get(action).cloned().unwrap_or_else(|| {
            default_dashboard_keys(action)
                .iter()
                .map(ToString::to_string)
                .collect()
        })
    }

    pub fn workspace_keys(&self, action: &str) -> Vec<String> {
        self.keys.workspace.get(action).cloned().unwrap_or_else(|| {
            default_workspace_keys(action)
                .iter()
                .map(ToString::to_string)
                .collect()
        })
    }

    pub fn help_bindings(&self) -> Vec<String> {
        let mut lines = vec!["DASHBOARD".into()];
        for (action, label) in [
            ("previous", "previous session"),
            ("next", "next session"),
            ("enter", "open agent"),
            ("shell", "open shell"),
            ("new", "new session"),
            ("alert", "unread alert"),
            ("search", "search sessions"),
            ("alias", "rename session"),
            ("archive", "archive / restore"),
            ("copy", "copy shell output"),
            ("stage", "stage shell output"),
            ("retry_summary", "retry summary"),
            ("takeover", "force takeover"),
            ("help", "help"),
            ("quit", "quit"),
        ] {
            let keys = self.dashboard_keys(action);
            if !keys.is_empty() {
                lines.push(format!(
                    "{label:<22} {}",
                    keys.iter()
                        .map(|key| format_key_label(key))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        lines.push(format!("{:<22} {}", "sidebar selection", "Mouse wheel"));
        for (heading, actions) in [
            (
                "WORKSPACE · DIRECT",
                [
                    ("focus", "cycle focus"),
                    ("new_shell", "new shell"),
                    ("next_shell", "next shell"),
                    ("close_shell", "close shell"),
                    ("dashboard", "dashboard"),
                    ("alert", "unread alert"),
                    ("previous_session", "previous session"),
                    ("next_session", "next session"),
                ]
                .as_slice(),
            ),
            (
                "WORKSPACE · SESSION LIST",
                [
                    ("previous_shell", "previous shell"),
                    ("search", "search sessions"),
                    ("session_alert", "unread alert"),
                    ("help", "help"),
                    ("maximize", "focus last shell"),
                    ("hide_shells", "focus agent"),
                    ("grow_shell", "grow shell area"),
                    ("shrink_shell", "shrink shell area"),
                    ("copy_command", "copy command output"),
                ]
                .as_slice(),
            ),
            (
                "WORKSPACE · CHILD VIEWPORT",
                [
                    ("scroll_up", "scroll up"),
                    ("scroll_down", "scroll down"),
                    ("live_tail", "live tail"),
                ]
                .as_slice(),
            ),
        ] {
            lines.push(heading.into());
            if heading.contains("SESSION LIST") {
                lines.extend([
                    format!("{:<22} {}", "select session", "↑/↓, J/K"),
                    format!("{:<22} {}", "open agent", "Enter"),
                    format!("{:<22} {}", "new session", "N"),
                    format!("{:<22} {}", "open shell", "S"),
                    format!("{:<22} {}", "archive / restore", "X"),
                    format!("{:<22} {}", "rename session", "E"),
                ]);
            }
            for (action, label) in actions {
                let keys = self.workspace_keys(action);
                if !keys.is_empty() {
                    lines.push(format!(
                        "{label:<22} {}",
                        keys.iter()
                            .map(|key| format_key_label(key))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
            if heading.contains("CHILD VIEWPORT") {
                lines.extend([
                    format!("{:<22} {}", "scroll pointed pane", "Mouse wheel"),
                    format!(
                        "{:<22} {}",
                        "select / copy text", "Drag auto-copies; Option-Drag native in iTerm2"
                    ),
                ]);
            }
            if heading.contains("SESSION LIST") {
                let keys = (1..=9)
                    .flat_map(|index| self.workspace_keys(&format!("select_shell_{index}")))
                    .map(|key| format_key_label(&key))
                    .collect::<Vec<_>>();
                if !keys.is_empty() {
                    lines.push(format!("{:<22} {}", "select shell", keys.join(", ")));
                }
            }
        }
        lines
    }

    pub(crate) fn parse(text: &str, path: &Path) -> io::Result<Self> {
        let config: Self = toml::from_str(text).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("cannot parse {}: {error}", path.display()),
            )
        })?;
        for (name, command) in [
            ("codex", config.providers.codex.as_ref()),
            ("claude", config.providers.claude.as_ref()),
            ("pi", config.providers.pi.as_ref()),
        ] {
            if command.is_some_and(Vec::is_empty) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("providers.{name} must contain at least one command element"),
                ));
            }
        }
        validate_keys("keys.dashboard", &config.keys.dashboard, DASHBOARD_ACTIONS)?;
        validate_keys("keys.workspace", &config.keys.workspace, WORKSPACE_ACTIONS)?;
        if config.summary.circuit_failures == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "summary.circuit_failures must be greater than zero",
            ));
        }
        Ok(config)
    }
}

pub(crate) fn format_key_label(key: &str) -> String {
    let lower = key.to_ascii_lowercase();
    for (prefix, display) in [("ctrl-", "Ctrl-"), ("alt-", "Alt-"), ("shift-", "Shift-")] {
        if let Some(value) = lower.strip_prefix(prefix) {
            let value = if prefix == "ctrl-" && value.chars().count() == 1 {
                value.to_ascii_uppercase()
            } else {
                capitalize_key_name(value)
            };
            return format!("{display}{value}");
        }
    }
    capitalize_key_name(&lower)
}

fn capitalize_key_name(key: &str) -> String {
    match key {
        "enter" => "Enter".into(),
        "esc" => "Esc".into(),
        "up" => "↑".into(),
        "down" => "↓".into(),
        "left" => "←".into(),
        "right" => "→".into(),
        "pageup" => "PageUp".into(),
        "pagedown" => "PageDown".into(),
        value if value.starts_with('f') => value.to_ascii_uppercase(),
        _ => key.to_owned(),
    }
}

fn validate_keys(
    path: &str,
    configured: &BTreeMap<String, Vec<String>>,
    actions: &[&str],
) -> io::Result<()> {
    for (action, keys) in configured {
        if !actions.contains(&action.as_str()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{path}.{action} is not a known action"),
            ));
        }
        if keys.is_empty() || keys.iter().any(|key| key.trim().is_empty()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{path}.{action} must contain at least one key"),
            ));
        }
        for key in keys {
            let valid = if path.ends_with("dashboard") {
                valid_dashboard_key(key)
            } else {
                valid_workspace_key(key)
            };
            if !valid {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{path}.{action} contains unsupported key {key:?}"),
                ));
            }
        }
    }
    let mut claimed = BTreeMap::new();
    for action in actions {
        let keys = if let Some(keys) = configured.get(*action) {
            keys.clone()
        } else if path.ends_with("dashboard") {
            default_dashboard_keys(action)
                .iter()
                .map(ToString::to_string)
                .collect()
        } else {
            default_workspace_keys(action)
                .iter()
                .map(ToString::to_string)
                .collect()
        };
        for key in keys {
            let normalized = key.to_ascii_lowercase();
            if let Some(previous) = claimed.insert(normalized.clone(), action) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{path} assigns {normalized} to both {previous} and {action}"),
                ));
            }
        }
    }
    Ok(())
}

fn valid_dashboard_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "enter"
            | "esc"
            | "up"
            | "down"
            | "left"
            | "right"
            | "tab"
            | "backtab"
            | "backspace"
            | "delete"
            | "home"
            | "end"
            | "pageup"
            | "pagedown"
    ) || lower
        .strip_prefix('f')
        .and_then(|number| number.parse::<u8>().ok())
        .is_some_and(|number| (1..=24).contains(&number))
        || valid_modified_character(&lower)
        || lower.chars().count() == 1
}

fn valid_workspace_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "ctrl-up" | "ctrl-down" | "shift-pageup" | "shift-pagedown" | "shift-end"
    ) || valid_modified_character(&lower)
        || lower.chars().count() == 1
}

fn valid_modified_character(key: &str) -> bool {
    ["ctrl-", "alt-"].into_iter().any(|prefix| {
        key.strip_prefix(prefix)
            .is_some_and(|value| value.chars().count() == 1)
    })
}

fn default_dashboard_keys(action: &str) -> &'static [&'static str] {
    match action {
        "quit" => &["q", "esc"],
        "next" => &["down", "j"],
        "previous" => &["up", "k"],
        "enter" => &["enter"],
        "takeover" => &["t"],
        "shell" => &["s"],
        "copy" | "stage" => &[],
        "new" => &["n"],
        "alert" => &["a"],
        "retry_summary" => &["r"],
        "search" => &["/"],
        "alias" => &["e"],
        "archive" => &["x"],
        "help" => &["?"],
        _ => &[],
    }
}

fn default_workspace_keys(action: &str) -> &'static [&'static str] {
    match action {
        // Global Workspace keys must stay clear of the bindings Codex and Claude
        // Code claim for themselves; ctrl-\, ctrl-^, and ctrl-q are the only free
        // ones left, so `alert` and `live_tail` rely on their Sessions-focus keys.
        "focus" => &["ctrl-\\"],
        "new_shell" => &["ctrl-^"],
        "previous_shell" => &[],
        "next_shell" => &["ctrl-n"],
        "close_shell" => &["ctrl-x"],
        "dashboard" => &["ctrl-q"],
        "alert" => &[],
        "search" => &["/"],
        "session_alert" => &["a"],
        "help" => &["?"],
        "previous_session" | "next_session" => &[],
        "maximize" => &["m"],
        "hide_shells" => &["h"],
        "grow_shell" => &["+"],
        "shrink_shell" => &["_"],
        "copy_command" => &["y"],
        "scroll_up" => &["shift-pageup"],
        "scroll_down" => &["shift-pagedown"],
        "live_tail" => &[],
        "select_shell_1" => &["1"],
        "select_shell_2" => &["2"],
        "select_shell_3" => &["3"],
        "select_shell_4" => &["4"],
        "select_shell_5" => &["5"],
        "select_shell_6" => &["6"],
        "select_shell_7" => &["7"],
        "select_shell_8" => &["8"],
        "select_shell_9" => &["9"],
        _ => &[],
    }
}

fn is_shell_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn executable_on_path(program: &str) -> bool {
    let path = Path::new(program);
    if path.components().count() > 1 {
        return is_executable(path);
    }
    env::var_os("PATH").is_some_and(|paths| {
        env::split_paths(&paths).any(|directory| is_executable(&directory.join(program)))
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

fn config_path() -> Option<PathBuf> {
    if let Some(path) = env::var_os(CONFIG_ENV).filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path));
    }
    dirs::home_dir().map(|home| home.join(".config/agent-console/config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_command_prepends_configured_argv() {
        let config = AgentConsoleConfig::parse(
            r#"
                [providers]
                codex = ["proxychains4", "codex", "--profile", "work"]
                claude = ["env", "HTTPS_PROXY=http://127.0.0.1:7890", "claude"]
            "#,
            Path::new("config.toml"),
        )
        .unwrap();

        let codex = config.provider_command(AgentKind::Codex, ["resume", "session-id"]);
        assert_eq!(codex.program, "proxychains4");
        assert_eq!(
            codex.args,
            ["codex", "--profile", "work", "resume", "session-id"].map(OsString::from)
        );
        let claude = config.provider_command(AgentKind::Claude, ["--resume", "session-id"]);
        assert_eq!(claude.program, "env");
        assert_eq!(
            claude.args,
            [
                "HTTPS_PROXY=http://127.0.0.1:7890",
                "claude",
                "--resume",
                "session-id"
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn missing_provider_config_uses_normal_binary() {
        let command = AgentConsoleConfig::default()
            .provider_command(AgentKind::Codex, ["exec", "--ephemeral"]);
        assert_eq!(command.program, "codex");
        assert_eq!(command.args, ["exec", "--ephemeral"].map(OsString::from));
    }

    #[test]
    fn empty_provider_command_is_rejected() {
        let error =
            AgentConsoleConfig::parse("[providers]\ncodex = []\n", Path::new("config.toml"))
                .unwrap_err();
        assert!(error.to_string().contains("providers.codex"));
    }

    #[test]
    fn single_missing_executable_is_resolved_as_a_shell_alias() {
        let config = AgentConsoleConfig::parse(
            "[providers]\nclaude = [\"agent_console_test_alias_that_does_not_exist\"]\n",
            Path::new("config.toml"),
        )
        .unwrap();
        let command = config.provider_command(AgentKind::Claude, ["--resume", "session-id"]);
        let shell = env::var_os("SHELL").unwrap_or_else(|| OsString::from("/bin/sh"));

        assert_eq!(command.program, shell);
        assert_eq!(command.args[0], "-ic");
        assert_eq!(
            command.args[1],
            "agent_console_test_alias_that_does_not_exist \"$@\""
        );
        assert_eq!(command.args[2], "agent-console-provider");
        assert_eq!(command.args[3], "--resume");
        assert_eq!(command.args[4], "session-id");
    }

    #[test]
    fn single_existing_executable_remains_a_direct_command() {
        let config =
            AgentConsoleConfig::parse("[providers]\ncodex = [\"sh\"]\n", Path::new("config.toml"))
                .unwrap();
        let command = config.provider_command(AgentKind::Codex, ["--version"]);

        assert_eq!(command.program, "sh");
        assert_eq!(command.args, ["--version"].map(OsString::from));
    }

    #[test]
    fn summary_scheduler_policy_is_configurable() {
        let config = AgentConsoleConfig::parse(
            r#"
                [summary]
                min_interval_seconds = 12
                failure_backoff_seconds = 7
                circuit_failures = 4
                circuit_cooldown_seconds = 90
            "#,
            Path::new("config.toml"),
        )
        .unwrap();

        assert_eq!(config.summary.min_interval_seconds, 12);
        assert_eq!(config.summary.failure_backoff_seconds, 7);
        assert_eq!(config.summary.circuit_failures, 4);
        assert_eq!(config.summary.circuit_cooldown_seconds, 90);
    }

    #[test]
    fn named_profiles_are_rejected_in_favor_of_one_command_per_provider() {
        let error = AgentConsoleConfig::parse(
            "[profiles.work]\ncodex = [\"codex\"]\n",
            Path::new("config.toml"),
        )
        .unwrap_err();

        assert!(error.to_string().contains("unknown field `profiles`"));
    }

    #[test]
    fn configured_keys_replace_defaults_and_are_case_insensitive() {
        let config = AgentConsoleConfig::parse(
            r#"
                [keys.dashboard]
                search = ["Z"]

                [keys.workspace]
                focus = ["alt-f"]
            "#,
            Path::new("config.toml"),
        )
        .unwrap();

        assert_eq!(config.dashboard_action("z"), Some("search"));
        assert_eq!(config.dashboard_action("/"), None);
        assert_eq!(config.workspace_keys("focus"), vec!["alt-f"]);
    }

    #[test]
    fn removed_shell_rename_binding_is_rejected() {
        let error = AgentConsoleConfig::parse(
            "[keys.workspace]\nrename_shell = [\"r\"]\n",
            Path::new("config.toml"),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("keys.workspace.rename_shell is not a known action")
        );
    }

    #[test]
    fn workspace_defaults_use_direct_contextual_controls() {
        let config = AgentConsoleConfig::default();

        assert_eq!(config.workspace_keys("focus"), vec!["ctrl-\\"]);
        assert_eq!(config.workspace_keys("new_shell"), vec!["ctrl-^"]);
        assert!(config.workspace_keys("previous_shell").is_empty());
        assert_eq!(config.workspace_keys("next_shell"), vec!["ctrl-n"]);
        assert_eq!(config.workspace_keys("close_shell"), vec!["ctrl-x"]);
        assert_eq!(config.workspace_keys("dashboard"), vec!["ctrl-q"]);
        assert!(config.workspace_keys("alert").is_empty());
        assert_eq!(config.workspace_keys("search"), vec!["/"]);
        assert_eq!(config.workspace_keys("session_alert"), vec!["a"]);
        assert_eq!(config.workspace_keys("help"), vec!["?"]);
        assert!(config.workspace_keys("previous_session").is_empty());
        assert!(config.workspace_keys("next_session").is_empty());
        assert_eq!(config.workspace_keys("maximize"), vec!["m"]);
        assert_eq!(config.workspace_keys("hide_shells"), vec!["h"]);
        assert_eq!(config.workspace_keys("grow_shell"), vec!["+"]);
        assert_eq!(config.workspace_keys("shrink_shell"), vec!["_"]);
        assert!(config.workspace_keys("rename_shell").is_empty());
    }

    #[test]
    fn workspace_keys_reachable_from_a_child_avoid_provider_bindings() {
        let config = AgentConsoleConfig::default();
        // Keys Codex and Claude Code claim for themselves. Workspace actions that
        // stay active while a provider owns the focus must leave these alone, or
        // the provider never sees them.
        const PROVIDER_KEYS: &[&str] = &[
            "ctrl-a",
            "ctrl-b",
            "ctrl-c",
            "ctrl-d",
            "ctrl-e",
            "ctrl-f",
            "ctrl-g",
            "ctrl-j",
            "ctrl-k",
            "ctrl-l",
            "ctrl-n",
            "ctrl-o",
            "ctrl-p",
            "ctrl-r",
            "ctrl-s",
            "ctrl-t",
            "ctrl-u",
            "ctrl-v",
            "ctrl-w",
            "ctrl-x",
            "ctrl-y",
            "ctrl-z",
            "ctrl-]",
            "shift-end",
        ];
        for action in [
            "focus",
            "new_shell",
            "dashboard",
            "alert",
            "live_tail",
            "scroll_up",
            "scroll_down",
        ] {
            for key in config.workspace_keys(action) {
                assert!(
                    !PROVIDER_KEYS.contains(&key.as_str()),
                    "{action} claims {key} from the focused provider"
                );
            }
        }
    }

    #[test]
    fn dashboard_defaults_expose_archive_and_contextual_takeover_without_a_menu() {
        let config = AgentConsoleConfig::default();

        assert_eq!(config.dashboard_action("x"), Some("archive"));
        assert_eq!(config.dashboard_action("t"), Some("takeover"));
        assert_eq!(config.dashboard_action("r"), Some("retry_summary"));
        assert_eq!(config.dashboard_action("e"), Some("alias"));
        assert_eq!(config.dashboard_action(":"), None);
        for key in ["y", "i", "u", "m", "f", "g", "w", "c", "v", "p", "o"] {
            assert_eq!(
                config.dashboard_action(key),
                None,
                "{key} must not be global"
            );
        }
        assert!(
            config
                .help_bindings()
                .iter()
                .all(|line| !line.ends_with(' '))
        );
    }

    #[test]
    fn help_lists_fixed_and_configured_actions_with_user_facing_names() {
        let config = AgentConsoleConfig::parse(
            r#"
                [keys.dashboard]
                alias = ["e"]
                copy = ["y"]
                stage = ["i"]
                retry_summary = ["r"]

                [keys.workspace]
                previous_session = ["ctrl-up"]
                next_session = ["ctrl-down"]
                previous_shell = ["alt-p"]
                select_shell_1 = ["alt-1"]
            "#,
            Path::new("config.toml"),
        )
        .unwrap();
        let lines = config.help_bindings();
        let help = lines.join("\n");

        for (label, keys) in [
            ("rename session", "e"),
            ("copy shell output", "y"),
            ("stage shell output", "i"),
            ("retry summary", "r"),
            ("previous session", "Ctrl-↑"),
            ("next session", "Ctrl-↓"),
            ("previous shell", "Alt-p"),
            ("select session", "↑/↓, J/K"),
            ("open agent", "Enter"),
            ("new session", "N"),
            ("open shell", "S"),
            ("archive / restore", "X"),
            ("rename session", "E"),
            ("focus last shell", "m"),
            ("focus agent", "h"),
            ("select shell", "Alt-1, 2, 3, 4, 5, 6, 7, 8, 9"),
        ] {
            assert!(
                lines
                    .iter()
                    .any(|line| line.contains(label) && line.ends_with(keys)),
                "missing help line: {label} -> {keys}"
            );
        }
        assert!(!help.contains("hide_shells"));
        assert!(!help.contains("copy_command"));
        assert!(help.contains("sidebar selection"));
        assert!(help.contains("scroll pointed pane"));
        assert!(help.contains("select / copy text"));
        for action in DASHBOARD_ACTIONS {
            for key in config.dashboard_keys(action) {
                assert!(
                    help.contains(&format_key_label(&key)),
                    "missing {action} key"
                );
            }
        }
        for action in WORKSPACE_ACTIONS {
            for key in config.workspace_keys(action) {
                assert!(
                    help.contains(&format_key_label(&key)),
                    "missing {action} key"
                );
            }
        }
    }

    #[test]
    fn invalid_or_duplicate_keys_are_rejected_at_startup() {
        let duplicate = AgentConsoleConfig::parse(
            "[keys.dashboard]\nsearch = [\"q\"]\n",
            Path::new("config.toml"),
        )
        .unwrap_err();
        assert!(duplicate.to_string().contains("both quit and search"));

        let invalid = AgentConsoleConfig::parse(
            "[keys.workspace]\nfocus = [\"space\"]\n",
            Path::new("config.toml"),
        )
        .unwrap_err();
        assert!(invalid.to_string().contains("unsupported key"));
    }
}
