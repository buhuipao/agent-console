# Prototype findings (historical)

This note records the design conclusions from the original prototype. All
follow-up items listed in the old verdict—SQLite state, live search, and a
packaged release path—are implemented in the current product.

The interaction model is viable:

- Workspace grouping with activity-sorted sessions is easier to scan than one
  ungrouped history.
- Deterministic provider events own working/waiting/failed state; model output
  is used only for semantic task/progress summaries.
- A persistent session sidebar plus an agent pane and up to three visible shell
  panes matches the requested Cursor/VS Code workflow better than full-screen
  agent/shell toggling.
- Owning the PTY from process start is required for live attach/detach.
- Shell capture plus staged bracketed paste satisfies the copy-to-agent workflow
  without auto-submitting potentially dangerous text.

The current implementation therefore uses SQLite state, live metadata search,
workspace groups, detached Unix PTYs, process-local Windows ConPTY, and native
release archives. This file is design history, not a current TODO list.
