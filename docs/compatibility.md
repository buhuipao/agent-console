# Provider compatibility

The supported floor and the versions exercised for this release are:

| Provider | Supported floor | Contract fixtures | Real local smoke |
| --- | ---: | --- | --- |
| Codex CLI | 0.100.0 | 0.100.0, 0.144.4 | 0.144.4 |
| Claude Code | 2.0.0 | 2.0.0, 2.1.211 | 2.1.211 |

Compatibility means the provider exposes the resume and hook-configuration
flags used by the native session command and the non-persistent structured
output flags used by the same-provider summary command. `doctor` checks these
contracts from provider help output instead of invoking a model.

`tests/e2e/provider_compatibility.sh` covers every fixture pair. The fixtures
model the CLI contract rather than provider rendering, which remains owned by
the provider. `tests/e2e/real_provider_doctor.sh` runs the same no-model smoke
against locally installed/configured binaries.

`tests/e2e/real_provider_sessions.exp` is the release-only interactive smoke:
it starts one empty real session per provider, refreshes discovery, stops the
PTY daemon, and re-enters through the provider resume contract. It never sends
a prompt.
