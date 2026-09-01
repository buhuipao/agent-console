# Changelog

All notable changes to this project will be documented in this file.

## [0.2.2] - 2026-09-01

### Added
- Pasted images in the Conversation tab render as a clickable thumbnail with a
  full-size preview overlay, instead of a bare "image (not rendered in this
  view)" placeholder. Small images (up to ~3 MB decoded) are relayed inline as
  a data URI from Claude, Codex and pi transcripts alike; larger ones still
  fall back to a placeholder chip rather than repeating a multi-megabyte
  payload on every poll of the session.

### Fixed
- The web sidebar's workspace groups now carry a folder icon, and session
  rows sit visibly indented inside their group's rail instead of flush
  against the group header.
- The session detail header's title no longer wraps to multiple lines and
  overflows its row: a flex item's default `min-width: auto` was blocking the
  ellipsis rule from ever engaging.
- A Claude reply with no tool calls is now visibly a bubble (a distinct
  background), rather than reading as bare text indistinguishable from the
  pane behind it.
- A letterboxed terminal's spare room reads as a deliberate diagonal stripe
  again; the two stripe colors had drifted close enough to each other to look
  like a flat black dead zone instead.
- The Shell and Agent TUI terminals now render in the app's own monospace
  font stack instead of silently falling back to xterm.js's default, which
  diverged from every other monospace element in the UI.
- The topbar's "Agent Console" title is a link back to the dashboard, the
  Alerts button gained an icon, and on a narrow phone screen Alerts and New
  session collapse to icon-only instead of overlapping each other.

## [0.2.1] - 2026-08-30

### Fixed
- See and answer the blocking dialogs the web UI was blind to. A "trust this
  folder", a tool permission request, an update prompt or a spend-cap notice is
  never written to a transcript, so a session stopped on one reported `idle` with
  no pending decision and every prompt sent to it was swallowed and refused after
  a ten-second wait. Cursor menus -- Claude Code 2.1.251 and pi, which carry no
  digits at all -- are now recognised alongside Codex's numbered ones, and a
  dialog drawn inside a box is recognised too. pi needs this most: it emits no
  extension events whatsoever until its trust dialog is answered, so the screen is
  the only place that dialog exists.
- Answer a cursor menu in two steps: write the arrow keys, read the highlight
  back, and press Enter only once it sits on the option that was asked for. A
  positional answer cannot be checked at parse time -- a label that wraps at the
  window width is indistinguishable from a second option -- so without the
  read-back a miscount would confirm the neighbour of what the user tapped.
- Detection is anchored rather than searched: a menu is a contiguous block of
  option lines directly above its own footer, and evidence from elsewhere on the
  screen no longer counts. An earlier attempt matched any two numbered lines plus
  any line saying "to confirm", which made a stale plan scrolled above a live
  permission dialog outrank the dialog -- answering it then wrote an inert digit
  and a live carriage return, silently approving a tool call nobody chose. Ordinary
  agent output no longer trips it either: `to continue` now needs the key named
  beside it, and `>` and `*` are a blockquote and a bullet rather than selection
  markers.

## [0.2.0] - 2026-08-30

### Added
- Support [pi](https://github.com/earendil-works/pi) as a third provider, on the
  same footing as Codex and Claude Code: its sessions are discovered from
  `~/.pi/agent/sessions` (or `PI_CODING_AGENT_DIR`), listed and grouped by
  workspace, resumed in place, summarised by pi itself, and rendered in the web
  conversation view. `AGENT_CONSOLE_PROVIDERS=pi` and `[providers] pi = [...]`
  work as they do for the other two.
- Report pi progress through a generated pi extension. pi has no hook commands --
  extensions are TypeScript loaded with `-e` -- so the console writes one into its
  state directory and forwards session, prompt, tool, and turn events to
  `agent-console hook pi`. Its child processes are detached, because
  `session_shutdown` fires while pi is exiting and a hook still in pi's process
  group is killed before it can report the session ended. A bridge that cannot be
  written costs the session its live status, not its start.
- Launch pi with `--tui-mode regular`, for the same reason Codex gets
  `--no-alt-screen` and Claude Code gets `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN`:
  pi's experimental fullscreen mode takes the alternate screen, where neither the
  dashboard nor a browser has any history to scroll back through.
- Resume and create pi sessions through one flag. `pi --session-id <id>` reopens
  the session when the file exists and creates it with that id when it does not,
  so the console's own id stays authoritative and `--name` is only passed when the
  session is new -- a reattach must not overwrite a name set with `/name`.

### Fixed
- Walk the new-session provider field in the direction its arrow points. `< codex >`
  is drawn with an arrow on each side, and with two providers it made no difference
  which one was pressed; with three, Left has to go back.

### Known limitations
- A pi session never reports `waiting`. Codex and Claude Code raise an approval event
  the console turns into a pending decision; pi ships no permission prompts at all,
  so there is nothing to raise. A pi session blocked on its own "trust project
  folder?" dialog therefore reads as idle in the web UI -- answer it from the
  Agent TUI tab, which shows pi's own screen.
- pi status reporting needs a POSIX shell. The generated bridge spawns `sh -c`, so on
  Windows it silently reports nothing and the session runs without live status.
  Codex and Claude Code are unaffected: their own hook runners execute the command.

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
  checkpoint. Both the Agent TUI and Shell tabs.
- Let a web terminal actually be scrolled while its agent is printing. xterm renders into a
  real scrolling element and reassigns its scroll position on every write, so a wheel's
  smooth scroll and a finger's momentum were cancelled ten times a second: the Agent TUI tab
  crawled on a desktop and would not move at all on a phone, while the Shell tab -- usually
  sitting at a prompt, printing nothing -- looked fine. Scrolling off the tail now holds the
  arriving output instead of writing it, in order, and releases it when you come back to the
  bottom or press "Jump to latest".
- Keep Claude Code out of the alternate screen, which is what a session's earlier output was
  disappearing into. A program on the alternate screen has no scrollback at all, so the Agent
  TUI tab opened on the current screen with nothing above it and there was nothing to swipe
  back through on a phone; in the dashboard, Claude also turned mouse reporting on there, so
  every wheel notch was handed to the agent to answer instead of scrolling the buffer the
  console already keeps -- which is why scrolling a Claude session felt slow and a Codex one
  did not. Codex has been asked for this with `--no-alt-screen` all along; Claude Code's
  switch is an environment variable, so a spawn can now carry environment as well as
  arguments. An explicit `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN` of your own is left alone.
- Size a session's PTY to the smallest window looking at it, instead of to whichever one
  attached last. Opening the same session on a phone squashed the desktop it was already
  open on into a narrow column -- and, because resizing a PTY reflows its scrollback, mangled
  the history the desktop was in the middle of reading, which is why the two were reported
  together. Every attached window is now counted as a viewer -- each browser socket, and the
  dashboard's own workspace -- and the terminal is sized to the element-wise minimum of them;
  a window leaving gives the size back. A window with room to spare letterboxes the unused
  area rather than stretching or re-wrapping the agent's output into it, and names the size
  it is showing. A socket that dies without closing cleanly stops counting when its task
  drops, and a viewer whose whole process was killed is forgotten by pid.
- Report a PTY daemon left running by an older build. The daemon owns every running agent's
  terminal, so nothing restarts it behind your back, and its wire format tolerates a version
  gap field by field -- which meant an upgrade degraded in silence: an older daemon answers
  polls without the rows above the screen, so browser terminals opened with no history and
  nothing said why. `agent-console doctor` now asks it for its protocol version and says so.
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
