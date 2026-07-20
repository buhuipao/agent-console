# Provider compatibility

The supported floor and the versions exercised for this release are:

| Provider | Supported floor | Contract fixtures | Real local smoke |
| --- | ---: | --- | --- |
| Codex CLI | 0.100.0 | 0.100.0, 0.144.5 | 0.144.5 |
| Claude Code | 2.0.0 | 2.0.0, 2.1.214 | 2.1.214 |

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

## Packaged platforms

| OS | Architecture | Rust target | PTY lifetime |
| --- | --- | --- | --- |
| Windows | x86_64 | `x86_64-pc-windows-msvc` | process-local ConPTY |
| Linux | x86_64 | `x86_64-unknown-linux-gnu` | detached daemon |
| Linux | ARM64 | `aarch64-unknown-linux-gnu` | detached daemon |
| macOS | Intel | `x86_64-apple-darwin` | detached daemon |
| macOS | Apple Silicon | `aarch64-apple-darwin` | detached daemon |

Every pushed semantic version tag is built on a native GitHub-hosted runner.
Packages contain the platform binary and README; the release also contains one
`SHA256SUMS` file covering all archives. Starting with v0.0.6, both macOS
targets are Developer ID signed and notarized before their archives are
created; non-publishing packaging dry runs intentionally remain unsigned.
