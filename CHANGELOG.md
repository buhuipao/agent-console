# Changelog

All notable changes to this project will be documented in this file.

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
