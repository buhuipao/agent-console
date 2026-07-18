# Prototype verdict

Question being tested: can a terminal-only console reproduce the useful part of
Cursor's layout—persistent session navigation, a live agent pane, and multiple
same-directory shell panes—without depending on tmux?

The interaction model is viable:

- A flat, activity-sorted list is easier to scan than inferred directory groups.
- Deterministic provider events own working/waiting/failed state; model output
  is used only for semantic task/progress summaries.
- A persistent session sidebar plus an agent pane and up to three visible shell
  panes matches the requested Cursor/VS Code workflow better than full-screen
  agent/shell toggling.
- Owning the PTY from process start is required for live attach/detach.
- Shell capture plus staged bracketed paste satisfies the copy-to-agent workflow
  without auto-submitting potentially dangerous text.

Before turning this into a production application, replace prototype JSON state
with SQLite, add search/filtering for large histories, and add a signed release
installation path so Codex hook trust remains stable across upgrades.
