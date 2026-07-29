use std::{
    fmt,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Claude,
    Codex,
}

impl AgentKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Self::Claude => "Cla",
            Self::Codex => "Cdx",
        }
    }
}

impl fmt::Display for AgentKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Working,
    Waiting,
    Failed,
    #[default]
    Idle,
}

impl SessionStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Waiting => "waiting",
            Self::Failed => "failed",
            Self::Idle => "idle",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Decision {
    pub id: String,
    pub question: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionSummary {
    pub task: String,
    pub status: SessionStatus,
    pub progress: Vec<String>,
    pub current_action: String,
    pub next_step: String,
    pub needs_user: Vec<Decision>,
    pub blockers: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Session {
    pub key: String,
    pub provider_session_id: String,
    pub name: String,
    pub search_terms: Vec<String>,
    pub first_prompt: Option<String>,
    pub agent: AgentKind,
    pub status: SessionStatus,
    pub cwd: PathBuf,
    pub branch: Option<String>,
    pub transcript_path: Option<PathBuf>,
    pub transcript_modified_at: u64,
    pub transcript_fingerprint: String,
    pub summary_fingerprint: String,
    pub summary_updated_at: Option<u64>,
    pub summary_error: Option<String>,
    pub summary: SessionSummary,
    pub recent_activity: Vec<String>,
    pub pending_decisions: Vec<Decision>,
    pub pending_shell_injection: Option<String>,
    pub managed_alive: bool,
    pub unavailable_reason: Option<String>,
    pub discovered_after_startup: bool,
}

impl Session {
    pub fn stable_key(agent: AgentKind, provider_session_id: &str) -> String {
        format!("{}:{provider_session_id}", agent.label())
    }

    pub fn apply_deterministic_status(&mut self, active: bool, turn_failed: bool) {
        self.status = if !self.pending_decisions.is_empty() {
            SessionStatus::Waiting
        } else if self.managed_alive && active {
            SessionStatus::Working
        } else if turn_failed {
            SessionStatus::Failed
        } else {
            SessionStatus::Idle
        };

        self.summary.status = self.status;
        self.summary.needs_user.clone_from(&self.pending_decisions);
    }

    pub fn activity_age(&self, now: u64) -> String {
        let age = now.saturating_sub(self.transcript_modified_at);
        if age < 60 {
            format!("{age}s")
        } else if age < 3_600 {
            format!("{}m", age / 60)
        } else if age < 86_400 {
            format!("{}h", age / 3_600)
        } else {
            format!("{}d", age / 86_400)
        }
    }

    /// The first thing the user asked for. It identifies the session for its
    /// whole life, so it never follows the latest prompt or the rolling summary.
    pub fn list_title(&self) -> String {
        if let Some(prompt) = self
            .first_prompt
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return prompt.to_owned();
        }
        if let Some(branch) = self.branch.as_deref().filter(|value| !value.is_empty()) {
            return branch.to_owned();
        }
        format!(
            "session {}",
            self.provider_session_id.chars().take(8).collect::<String>()
        )
    }
}

pub fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        Session {
            key: "codex:one".into(),
            provider_session_id: "one".into(),
            name: "demo".into(),
            search_terms: Vec::new(),
            first_prompt: None,
            agent: AgentKind::Codex,
            status: SessionStatus::Idle,
            cwd: "/tmp".into(),
            branch: None,
            transcript_path: None,
            transcript_modified_at: 0,
            transcript_fingerprint: String::new(),
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

    #[test]
    fn the_title_is_the_first_prompt_and_ignores_later_work() {
        let mut value = session();
        value.branch = Some("feat/oidc".into());
        value.first_prompt = Some("  Add signed releases  ".into());
        value.summary.task = "Rewrote the notarization step".into();
        assert_eq!(value.list_title(), "Add signed releases");

        value.first_prompt = None;
        assert_eq!(value.list_title(), "feat/oidc");

        value.branch = None;
        assert_eq!(value.list_title(), "session one");
    }

    #[test]
    fn deterministic_status_precedence_is_waiting_working_failed_idle() {
        let mut value = session();
        value.pending_decisions.push(Decision {
            id: "approval-1".into(),
            question: "Run migration?".into(),
        });
        value.managed_alive = true;
        value.apply_deterministic_status(true, true);
        assert_eq!(value.status, SessionStatus::Waiting);

        value.pending_decisions.clear();
        value.apply_deterministic_status(true, true);
        assert_eq!(value.status, SessionStatus::Working);

        value.managed_alive = false;
        value.apply_deterministic_status(false, true);
        assert_eq!(value.status, SessionStatus::Failed);

        value.apply_deterministic_status(false, false);
        assert_eq!(value.status, SessionStatus::Idle);
    }
}
