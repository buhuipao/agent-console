# Agent Console 0.0.1 hardening plan

This completed plan records the work that promoted the initial prototype into
the 0.0.1 local control plane for Codex and Claude Code. A checkbox was closed
only after its focused regression tests, the full Rust suite, Clippy, and the
relevant PTY E2E tests passed.

## P0 — required for the core workflow

- [x] Per-pane scrollback for every agent and shell; keyboard paging, mouse
  wheel, live-tail return, a visible offset, selection, and clipboard copy.
- [x] One non-blocking runtime that continues discovery, event reduction,
  summary collection, and rendering in both Dashboard and Workspace views.
- [x] Live cross-session waiting/failed notifications with direct navigation
  to the affected session.
- [x] A background PTY daemon. Closing or crashing the TUI detaches; it does
  not terminate managed agents or shells. A new TUI reconnects to them.
- [x] Cross-process session leases with owner information, safe refusal, and
  an explicit force-takeover operation.
- [x] Fair summary scheduling with exponential backoff, provider circuit
  breakers, configurable frequency, and optional manual retry. One failing
  selected session must not starve other sessions.
- [x] Automatic rotating diagnostics, panic-safe terminal restoration,
  private state permissions, and bounded retention for sensitive event data.

## P1 — required for sustained daily use

- [x] Live Session search across provider/status/workspace metadata.
- [x] Persistent archive and user alias metadata. A generated summary
  never overwrites a user alias.
- [x] Switch sessions from the Workspace sidebar without visiting Dashboard.
- [x] Named shells, direct numeric selection, maximize/restore, pane resizing,
  process exit status, and command-block capture instead of only a raw tail.
- [x] One configurable launch command per provider, shared by resume, new
  sessions, summaries, and doctor checks.
- [x] Configurable key bindings plus an in-product effective-bindings panel.
- [x] SQLite state and indexed event offsets/rotation.
- [x] `doctor` capability checks for resume, hooks, summary invocation,
  clipboard, state permissions, daemon health, and supported provider
  versions.
- [x] Compatibility fixtures and smoke tests for real supported Codex and
  Claude Code versions.
- [x] Three-way `Ctrl-\` focus cycle across session list, agent, and shells;
  list navigation previews the selected transcript and retained shells without
  leaving Workspace or starting a provider.
- [x] Persist managed transcript fingerprints; restart stale idle/failed
  provider clients on activation while preserving working/waiting clients.
- [x] Adaptive Dashboard session-card grid with linked selection styling and
  compact status, workspace, task, and shell information.
- [x] Provider-TUI mouse scrolling for both explicit mouse reporting (Codex)
  and alternate-screen cursor scrolling (Claude Code).

## Final acceptance

- [x] `cargo fmt --check`
- [x] `cargo clippy --all-targets -- -D warnings`
- [x] Full Rust test suite
- [x] Dashboard and Workspace PTY E2E suites
- [x] Real Codex resume/new-session test
- [x] Real Claude resume/new-session test
- [x] Multiple real shells, high-output redraw, scroll/copy, resize, and close
- [x] Kill and restart the TUI while children continue; reconnect succeeds
- [x] Competing second TUI cannot resume the same provider session
- [x] Waiting/failed notification appears while another session has focus
