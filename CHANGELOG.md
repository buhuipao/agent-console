# Changelog

All notable changes to this project will be documented in this file.

## [0.1.0] - 2026-08-25

### Added
- Add `agent-console web [--host H] [--port P]`, serving the sessions to a browser
  as a responsive, installable PWA with a token-guarded REST API and a websocket
  that streams the raw PTY to xterm.js.
- Serve the web UI from the dashboard itself: plain `agent-console` now starts the
  TUI and the web server in one process, on one shared `App`, so both surfaces see
  the same sessions, discovery worker and summary worker. The dashboard header and
  help panel carry the address, token included. A port already in use is reported
  there and the dashboard still opens; `--no-web` or `[web] enabled = false` turns
  the server off. `agent-console web` stays for a machine with no TUI attached.
- Authenticate the web UI with HTTP Basic when credentials are configured, via
  `--auth <user>:<password>`, `AGENT_CONSOLE_WEB_AUTH`, or `[web] auth` -- command
  line first, then environment, then config file. Credentials are compared without
  an early exit and never logged. With none configured the historical random URL
  token still applies, so the server is never unauthenticated.
- Configure the bind address and port from the config file and environment as well
  as the command line: `[web] host`/`port`, `AGENT_CONSOLE_WEB_HOST`/`_PORT`, and
  `--host`/`--port`, in that order of precedence. Hostnames resolve (`localhost`),
  and the exposure warning keys off the address actually bound, so `0.0.0.0`, `::`
  and a concrete LAN address all warn.
- Add a web `Shell` tab: login shells running in the session's working directory,
  several per session, switchable and closable, each addressable by URL. They are
  the same daemon terminals the TUI's shell panes use, so a shell opened in either
  place shows up in the other. The old `Terminal` tab is now `Agent TUI`, which is
  what it always was -- the agent's own PTY.
- Bring the TUI's dashboard-level capability to the web UI, re-cut for a browser:
  an alerts inbox in the header (with a real system notification, so an installed
  PWA can tell you a session needs you), a debounced session search, whole-machine
  status counts that double as filters, rename, summary retry, shell output copy
  and "send to agent", a `#/doctor` diagnostics page, and a full-screen toggle for
  the terminal panes with a draggable session-list split.
- Surface a refused write instead of swallowing it: when another surface holds a
  session's input lease, the composer, the decision buttons and both terminal
  sockets now explain who holds it and offer to take it over, then retry.

### Fixed
- Scroll back through a session's earlier output in the web terminal itself -- a wheel on a
  desktop, a drag on a phone. Attaching used to hand the browser a checkpoint, which is one
  screenful, so xterm.js started with an empty scrollback and everything printed before the
  tab opened was simply not there. The first poll of a socket now answers with the rows above
  the screen as well as the screen, taken from one parser at one instant so the two cannot
  overlap or leave a gap, and the browser writes them into its own scrollback ahead of the
  checkpoint. Both the Agent TUI and Shell tabs; output arriving while you have scrolled up
  still leaves you where you are, and "Jump to latest" returns you to the tail.
- Keep the web API answering while the TUI has a session open. A workspace frame used to
  hold the shared `App` lock for as long as the workspace was up, so `/api/*` timed out for
  the entire time anyone was actually using the console. Frames are now stepped, and the part
  of a frame that costs anything -- polling the PTY daemon, parsing output, painting the
  screen -- runs behind that session's own lock instead of the `App`'s. Under an agent
  flooding output, `/api/sessions` goes from mostly 503 to a sub-millisecond median.
- Refuse cross-origin websocket handshakes. `/ws/*` is a full PTY control channel and a
  browser replays cached HTTP Basic credentials on a handshake another page started, so the
  upgrade now requires `Origin` to match `Host`. Clients that send no `Origin` at all --
  curl, websockets libraries, native apps, none of which carry ambient credentials -- are
  unaffected and still have to pass the credential check.

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
