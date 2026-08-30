//! Provider discovery contracts.
//!
//! Each supported agent CLI contributes one adapter describing where its
//! transcripts live, which files belong to it, how one file becomes a
//! `Session`, and any provider-specific enrichment. Discovery walks this table
//! instead of branching per provider, so a provider can be added, repointed, or
//! disabled without touching the discovery loop.
//!
//! Provider transcript formats are undocumented and change with provider
//! releases. `AGENT_CONSOLE_PROVIDERS` narrows the table at runtime, which is
//! also how a provider is retired once its own CLI ships an equivalent view.

use std::{env, io, path::Path};

use uuid::Uuid;

use crate::{
    diagnostics, discovery,
    model::{AgentKind, Session},
};

/// Environment variable holding a comma-separated provider allow list.
pub const PROVIDERS_ENV: &str = "AGENT_CONSOLE_PROVIDERS";

pub type ProviderParser =
    for<'a> fn(&Path, Option<(&'a str, &'a str)>) -> io::Result<Option<Session>>;

pub struct ProviderAdapter {
    pub kind: AgentKind,
    /// True when this file is one of the provider's session transcripts.
    pub accepts: fn(&Path) -> bool,
    /// Parse one accepted transcript, optionally reusing a known
    /// `(provider_session_id, first_prompt)`. `Ok(None)` means "not usable".
    pub parse: ProviderParser,
    /// Optional enrichment applied to the provider's parsed sessions, given its
    /// transcript root.
    pub enrich: Option<fn(&Path, &mut [Session], &mut discovery::DiscoveryCache)>,
}

const ADAPTERS: &[ProviderAdapter] = &[
    ProviderAdapter {
        kind: AgentKind::Codex,
        accepts: accepts_codex,
        parse: discovery::parse_codex_with_cached_prompt,
        enrich: Some(discovery::enrich_codex),
    },
    ProviderAdapter {
        kind: AgentKind::Claude,
        accepts: accepts_claude,
        parse: discovery::parse_claude_with_cached_prompt,
        enrich: None,
    },
    ProviderAdapter {
        kind: AgentKind::Pi,
        accepts: accepts_pi,
        parse: discovery::parse_pi_with_cached_prompt,
        enrich: None,
    },
];

/// Adapters enabled for this process, in table order.
pub fn enabled() -> Vec<&'static ProviderAdapter> {
    selected(env::var(PROVIDERS_ENV).ok().as_deref())
}

pub fn is_enabled(kind: AgentKind) -> bool {
    enabled().iter().any(|adapter| adapter.kind == kind)
}

#[cfg(test)]
pub fn adapter(kind: AgentKind) -> &'static ProviderAdapter {
    ADAPTERS
        .iter()
        .find(|adapter| adapter.kind == kind)
        .expect("every AgentKind has an adapter")
}

fn selected(selection: Option<&str>) -> Vec<&'static ProviderAdapter> {
    let Some(names) = allow_list(selection) else {
        return ADAPTERS.iter().collect();
    };
    ADAPTERS
        .iter()
        .filter(|adapter| names.iter().any(|name| name == adapter.kind.label()))
        .collect()
}

/// `None` means "no usable selection"; every provider stays enabled. A typo
/// must never produce a silently empty dashboard, so it is logged and ignored.
fn allow_list(selection: Option<&str>) -> Option<Vec<String>> {
    let selection = selection?.trim();
    if selection.is_empty() {
        return None;
    }
    let (known, unknown): (Vec<String>, Vec<String>) = selection
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_ascii_lowercase)
        .partition(|name| ADAPTERS.iter().any(|adapter| adapter.kind.label() == name));
    for name in &unknown {
        diagnostics::record(&format!(
            "{PROVIDERS_ENV}: unknown provider {name:?} ignored"
        ));
    }
    if known.is_empty() {
        diagnostics::record(&format!(
            "{PROVIDERS_ENV}={selection:?} matched no known provider; all providers stay enabled"
        ));
        return None;
    }
    Some(known)
}

fn accepts_codex(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
}

fn accepts_claude(path: &Path) -> bool {
    if path
        .ancestors()
        .skip(1)
        .any(|ancestor| ancestor.file_name().and_then(|name| name.to_str()) == Some("subagents"))
    {
        return false;
    }
    let Some(stem) = path.file_stem().and_then(|name| name.to_str()) else {
        return false;
    };
    path.extension().and_then(|value| value.to_str()) == Some("jsonl")
        && Uuid::parse_str(stem).is_ok()
}

/// pi names a transcript `<ISO stamp>_<session id>.jsonl`. The stamp is what tells it apart
/// from a Claude transcript, whose stem is the bare uuid and can therefore never contain `_`.
///
/// The id is not necessarily a uuid. `pi --session-id <id>` takes anything matching pi's own
/// rule, so requiring a uuid here made `pi --session-id review-pass` produce a session no
/// provider claimed and the dashboard never showed.
fn accepts_pi(path: &Path) -> bool {
    if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
        return false;
    }
    path.file_stem()
        .and_then(|name| name.to_str())
        .and_then(|stem| stem.rsplit_once('_'))
        .is_some_and(|(stamp, id)| !stamp.is_empty() && is_pi_session_id(id))
}

/// pi's own validator for a session id, transcribed from its `session-manager`:
/// `^[A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?$`.
fn is_pi_session_id(value: &str) -> bool {
    let ends_are_alphanumeric = || {
        let mut characters = value.chars();
        let first = characters.next();
        let last = characters.next_back().or(first);
        first.is_some_and(|value| value.is_ascii_alphanumeric())
            && last.is_some_and(|value| value.is_ascii_alphanumeric())
    };
    !value.is_empty()
        && ends_are_alphanumeric()
        && value
            .chars()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, '.' | '_' | '-'))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn kinds(selection: Option<&str>) -> Vec<AgentKind> {
        selected(selection)
            .iter()
            .map(|adapter| adapter.kind)
            .collect()
    }

    #[test]
    fn every_agent_kind_has_exactly_one_adapter() {
        for kind in [AgentKind::Codex, AgentKind::Claude, AgentKind::Pi] {
            assert_eq!(
                ADAPTERS
                    .iter()
                    .filter(|adapter| adapter.kind == kind)
                    .count(),
                1
            );
            assert_eq!(adapter(kind).kind, kind);
        }
    }

    #[test]
    fn an_absent_selection_enables_every_provider() {
        assert_eq!(
            kinds(None),
            [AgentKind::Codex, AgentKind::Claude, AgentKind::Pi]
        );
        assert_eq!(
            kinds(Some("   ")),
            [AgentKind::Codex, AgentKind::Claude, AgentKind::Pi]
        );
    }

    #[test]
    fn a_selection_retires_the_providers_it_omits() {
        assert_eq!(kinds(Some("codex")), [AgentKind::Codex]);
        assert_eq!(kinds(Some(" CLAUDE ")), [AgentKind::Claude]);
        assert_eq!(kinds(Some("pi")), [AgentKind::Pi]);
        assert_eq!(
            kinds(Some("claude,codex")),
            [AgentKind::Codex, AgentKind::Claude]
        );
        assert_eq!(kinds(Some("pi,codex")), [AgentKind::Codex, AgentKind::Pi]);
    }

    #[test]
    fn an_unusable_selection_keeps_every_provider_instead_of_showing_nothing() {
        assert_eq!(
            kinds(Some("cladue")),
            [AgentKind::Codex, AgentKind::Claude, AgentKind::Pi]
        );
        assert_eq!(
            kinds(Some(",,")),
            [AgentKind::Codex, AgentKind::Claude, AgentKind::Pi]
        );
        assert_eq!(kinds(Some("codex,gemini")), [AgentKind::Codex]);
    }

    #[test]
    fn adapters_accept_only_their_own_transcripts() {
        let codex = PathBuf::from("/root/2026/07/27/rollout-2026-07-27T10-00-00-abc.jsonl");
        let claude = PathBuf::from("/root/project/0197e9a1-6f42-7c31-9d55-6f0f8b0a1234.jsonl");
        let claude_subagent = PathBuf::from(
            "/root/project/session/subagents/0197e9a1-6f42-7c31-9d55-6f0f8b0a1234.jsonl",
        );
        let nested_claude_subagent = PathBuf::from(
            "/root/project/session/subagents/nested/0197e9a1-6f42-7c31-9d55-6f0f8b0a1234.jsonl",
        );
        let pi = PathBuf::from(
            "/root/--Users-me-repo--/2026-08-30T00-04-14-153Z_0197e9a1-6f42-7c31-9d55-6f0f8b0a1234.jsonl",
        );
        let other = PathBuf::from("/root/project/notes.jsonl");

        assert!(accepts_codex(&codex));
        assert!(!accepts_codex(&claude));
        assert!(accepts_claude(&claude));
        // A pi transcript is a stamp and a uuid; a Claude one is a bare uuid. Neither adapter
        // may claim the other's file, or one provider's sessions land under the other's name.
        assert!(accepts_pi(&pi));
        assert!(!accepts_pi(&claude));
        assert!(!accepts_pi(&codex));
        assert!(!accepts_pi(&other));
        assert!(!accepts_claude(&pi));
        assert!(!accepts_codex(&pi));

        // `pi --session-id <id>` takes anything matching pi's own rule, not just a uuid.
        // Requiring one here left those sessions claimed by no provider at all.
        let named =
            PathBuf::from("/root/--Users-me-repo--/2026-08-30T00-04-14-153Z_review-pass.jsonl");
        let dotted = PathBuf::from("/root/--Users-me-repo--/2026-08-30T00-04-14-153Z_v2.1.jsonl");
        let single = PathBuf::from("/root/--Users-me-repo--/2026-08-30T00-04-14-153Z_a.jsonl");
        assert!(accepts_pi(&named));
        assert!(accepts_pi(&dotted));
        assert!(accepts_pi(&single));
        // A trailing separator is not a pi id, and a stem with no `_` is not a pi transcript.
        assert!(!accepts_pi(&PathBuf::from(
            "/root/x/2026-08-30T00-04-14-153Z_-bad.jsonl"
        )));
        assert!(!accepts_pi(&PathBuf::from(
            "/root/x/2026-08-30T00-04-14-153Z_bad-.jsonl"
        )));
        assert!(!accepts_pi(&PathBuf::from("/root/x/nounderscore.jsonl")));
        assert!(!accepts_claude(&claude_subagent));
        assert!(!accepts_claude(&nested_claude_subagent));
        assert!(!accepts_claude(&codex));
        assert!(!accepts_codex(&other));
        assert!(!accepts_claude(&other));
    }
}
