# Changelog

All notable changes to this project will be documented in this file.

## [0.0.16] - 2026-08-07

### Added
- Add a `make install` target for atomic source-checkout installs.

### Fixed
- Exclude Codex and Claude Code subagents from session discovery and tracking.
- Skip injected `AGENTS.md` instructions when choosing a Codex session title.
- Persist the first real prompt so refreshes and restarts never retitle a session.
- Derive Codex goal-session titles from the embedded user objective instead of the internal context wrapper.
- Ignore image-only placeholders and use accompanying or subsequent text for Codex and Claude session titles.
- Keep Workspace input and rendering responsive while session discovery or Codex metadata reads are slow.
- Preserve terminal text selections while paging through Codex, Claude, and shell scrollback.

## [0.0.15] - 2026-08-07

### Fixed
- Keep Workspace shortcut hints visible on their own footer row while an alert is active.
- Clear alerts when their session recovers or the user opens the affected agent.
- Preserve first-prompt session titles after transcripts grow beyond the bounded head/tail discovery windows.

## [0.0.14] - 2026-08-04

### Fixed
- Fix `claude --resume` failing with "No conversation found" for sessions that entered a git worktree mid-run. Resume now walks up from `session.cwd` to find the ancestor directory matching the session's project root, instead of passing the worktree path to `claude --resume`.

## [0.0.13] - 2026-07-31

### Fixed
- Skip auto-spawning a new shell when toggling focus with no shells open; cycles back to sessions view instead

### CI
- Publish to crates.io from the release workflow

## [0.0.12] - 2026-07-29

### Features
- Select discovery providers with `AGENT_CONSOLE_PROVIDERS` environment variable

### Fixed
- Stop claiming provider keys and mistitling sessions

### Docs
- Lead the README with a recorded demo

### Chore
- Add dual license and crate metadata

## [0.0.11] - 2026-07-27

_(Release preparation — internal cleanup)_

## [0.0.9] - 2026-07-21

### Fixed
- Install signed macOS builds atomically

## [0.0.8] - 2026-07-21

### Fixed
- Align terminal selection with scrollback

## [0.0.7] - 2026-07-20

### CI
- Staple notarized macOS disk images

## [0.0.6] - 2026-07-20

### CI
- Sign macOS builds and harden session resume

## [0.0.5] - 2026-07-19

_(Release preparation)_

## [0.0.4] - 2026-07-19

_(Release preparation)_

## [0.0.3] - 2026-07-18

### CI
- Simplify provider commands

## [0.0.2] - 2026-07-18

### CI
- Add multi-platform release packaging
- Add non-publishing package dry runs

## [0.0.1] - 2026-07-18

Initial release — Agent Console dashboard for Codex and Claude Code sessions.
