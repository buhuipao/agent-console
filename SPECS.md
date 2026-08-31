# Agent Console implementation specification

Status: implementation contract for Agent Console 0.2.1.

Scope is the terminal application and the core it shares with the web server:
discovery, events and status, managed PTYs, summaries, and the dashboard. The
web/PWA server itself (`src/web/`) is not specified here; its user-facing
behavior is documented in [README.md](README.md), and the one contract the two
share is section 13.1. Domain terms are defined in [CONTEXT.md](CONTEXT.md),
and accepted architecture decisions live under `docs/adr/`.

This document is intentionally explicit so an implementation agent can execute
it without making product decisions. If code and this document disagree, this
document wins.

## 1. Goal

Build one local terminal application that can:

1. Discover recent persisted Codex, Claude Code, and pi sessions.
2. Show each session's provider, working directory, task summary, activity,
   status, and decisions that need the user.
3. Resume a persisted session in the provider's native terminal UI.
4. Keep sessions launched by Agent Console alive while the user moves between
   the dashboard, agent terminal, and a same-directory shell terminal.
5. Copy recent shell output, or stage it for insertion into the agent's next
   input without submitting that input automatically.
6. Continuously refresh a structured summary without modifying the coding
   conversation.

## 2. Hard boundary

Agent Console cannot attach to the PTY of an arbitrary process that was started
in another Terminal tab. Such a session is discoverable through its persisted
transcript, but entering it starts the provider's supported resume command.

A process is live-attachable only if Agent Console launched it and owns its PTY.

Never open the same provider session concurrently when Agent Console already
knows that its managed child is alive.

## 3. Supported environment

- Windows 10+, macOS, and Unix-like terminals.
- `codex`, `claude`, and/or `pi` available on `PATH`.
- Rust stable with edition 2024.
- No tmux dependency.
- Clipboard integration uses `pbcopy` on macOS, `clip.exe` on Windows, and
  `wl-copy`, `xclip`, or `xsel` on Linux. Clipboard failure is shown in the
  dashboard and never crashes the app.
- Unix uses the detached PTY daemon. Windows uses process-local ConPTY and does
  not promise PTY survival after Agent Console exits.
- `AGENT_CONSOLE_PTY_MODE=local` forces the Windows behavior on Unix: PTYs are
  owned by the running process and die with it. For debugging a daemon fault,
  not for daily use.

## 4. Commands

The binary supports these modes:

```text
agent-console                 Open the dashboard, serving the web UI too.
agent-console web             Serve the web UI alone, with no dashboard.
agent-console hook PROVIDER   Read one hook JSON object from stdin.
agent-console doctor          Check provider binaries and data paths.
```

The dashboard and `web` take `--host`, `--port`, and `--auth USER:PASS`; the
dashboard also takes `--no-web` to open without serving. Each reads
`AGENT_CONSOLE_WEB_HOST`, `_PORT`, `_AUTH`, and `_ENABLED`, then `[web]` in the
config file, in that order of precedence. `--help` and `--version` print and
exit.

The `hook` mode appends one normalized event to the application's event inbox.
It must finish quickly and must not invoke an LLM.

## 5. Data locations

Resolve the application state directory in this order:

1. `AGENT_CONSOLE_STATE_DIR`, when set.
2. `$XDG_STATE_HOME/agent-console`, when set.
3. `$HOME/.local/state/agent-console`.

Files:

```text
state.db                    SQLite session cache, metadata, event cursors/index.
state.json                  One-time legacy migration input only.
events/<provider>-<id>.jsonl[.1]  Two bounded hook/event generations.
summary-schema.json         Schema passed to non-interactive summarizers.
pi-hooks.mts                Generated pi extension bridging its events to `hook pi`.
pty-daemon.sock             Unix only; the detached PTY daemon's control socket.
agent-console.log           Current diagnostics; three rotated generations.
```

SQLite cache replacement is transactional and uses WAL mode. Import a valid
legacy JSON cache once into SQLite. A malformed legacy cache is ignored with a
visible warning and migration is not marked complete until a clean exit.

Hook ingestion appends complete JSONL records and rotates at 256 KiB, retaining
one prior generation. Store a byte offset and prefix fingerprint per source in
SQLite. Index only newly appended complete records; when the source shrinks or
its retained prefix changes, delete that source's indexed rows and rebuild it.
Status reduction and summaries read normalized events from this index.

The state directory and event directory use Unix mode `0700`; state, schema,
event, database, diagnostics, and daemon socket files use `0600`. Each event
inbox keeps two 256 KiB generations. Diagnostics rotate at 256 KiB
and retain three prior generations. A panic is recorded before the normal panic
handler runs, while a scope guard always restores raw mode and the alternate
screen during unwinding.

### 5.1 Provider command configuration

Read optional provider commands from `~/.config/agent-console/config.toml`, or
from the path in `AGENT_CONSOLE_CONFIG` when set:

```toml
[providers]
codex = ["proxychains4", "codex"]
claude = ["env", "HTTPS_PROXY=http://127.0.0.1:7890", "claude"]
pi = ["env", "DEEPSEEK_API_KEY=sk-...", "pi"]
```

There is one optional command per provider and no provider-profile layer.
Wrappers, environment launchers, proxies, aliases, and fixed arguments all
belong directly in that provider's argv array. Other runtime settings use
independent tables in the same file:

```toml

[summary]
min_interval_seconds = 30
failure_backoff_seconds = 30
circuit_failures = 3
circuit_cooldown_seconds = 300

[keys.dashboard]
help = ["?"]

[keys.workspace]
focus = ["ctrl-\\"]
dashboard = ["ctrl-q"]
```

For a multi-element value, treat it as an argv array without shell evaluation.
The first element is the executable; remaining elements precede Agent Console's
dynamic provider arguments. For a single safe command name, use it directly if
it resolves to an executable on `PATH`; otherwise invoke it through
`$SHELL -ic '<name> "$@"'` so an interactive-shell alias or function can receive the
dynamic arguments. An absent entry defaults to the provider executable on
`PATH`. An empty array or malformed TOML is a startup error that names the
invalid field. Use the same configured command for native sessions, isolated
same-provider summaries, and `doctor` checks.

Key maps replace defaults action by action. Reject unknown actions, unsupported
key labels, empty action arrays, and duplicate effective sequences before the
TUI starts. Workspace printable-key actions are active only in `FOCUS SESSIONS`;
the same bytes in agent/shell focus are forwarded. Focus, Dashboard, alert, and
new-Shell chords are global. Next-Shell and close-Shell chords are active only
in Shell focus and are forwarded in Agent focus. A chord that stays reachable
while a child owns the focus must avoid the keys Codex and Claude Code bind for
themselves; `ctrl-\`, `ctrl-^`, and `ctrl-q` are the only free ones, so `alert`
and `live_tail` ship unbound and rely on Session-list `a` and ordinary child
input. The input router and all
displayed control hints use the same resolved binding map. The `help` action
opens a three-column panel that lists all effective Dashboard and direct
Workspace bindings, fixed and configurable Session-list controls,
child-viewport controls, and mouse gestures using user-facing action names.
Context footers show the relevant high-frequency subset rather than repeating
the whole panel. Every modal shows its edit/commit/cancel behavior in place;
the configured Help key and `Esc` both close Help. `Esc` closes another modal
but is never a Workspace command while a child has focus.

## 6. Session identity

Use this stable key:

```text
<provider>:<provider-session-id>
```

`provider` is exactly `codex`, `claude`, or `pi`.

Each session record contains:

```text
key
provider
provider_session_id
name
cwd
branch (optional)
transcript_path
transcript_modified_at
status
summary
recent_activity
pending_decisions
pending_shell_injection (optional)
summary_through_fingerprint
summary_updated_at
managed_terminal_state
user_alias (optional)
archived
```

## 7. Discovery

Every provider contributes one adapter: its transcript root, the filter that
recognizes its transcripts, the parser that turns one transcript into a
session, and any provider-specific enrichment. Discovery iterates that table;
it must not branch on the provider anywhere else. Adding, repointing, or
retiring a provider is one table entry.

Provider transcript layouts are undocumented and change with provider releases,
so the table is narrowable at runtime. `AGENT_CONSOLE_PROVIDERS` holds a
comma-separated, case-insensitive allow list of provider labels:

```sh
AGENT_CONSOLE_PROVIDERS=codex agent-console
```

Omitted providers are not scanned, not summarized, and not reported by
`doctor`. An unset, empty, or unrecognized value enables every provider and
records the reason in diagnostics: a typo must never produce a silently empty
dashboard.

### 7.1 Codex

Scan `$CODEX_HOME/sessions`, or `~/.codex/sessions` when `CODEX_HOME` is unset,
for `rollout-*.jsonl` files. Sort by modification time descending and inspect at
most the newest 60 files. Reuse parsed results while file metadata is unchanged.

Read the first `session_meta.payload` for the transcript's own `id`; fork
transcripts may contain a later copy of the parent's metadata. Read `cwd` from
metadata and turn context. Identify a fork subagent from
`thread_source = "subagent"` or `source.subagent` and exclude it from Sessions,
status tracking, and summary generation. Stream complete records until the
first real user prompt is found, then use only a bounded tail for recent user,
agent, tool, and completion records. The transcript format is an observation
fallback, not a write interface.

When the newest `$CODEX_HOME/state_<version>.sqlite` is readable, enrich the
session with the provider's name, extracted title, first user message, and
preview from its `threads` row. Database enrichment is read-only and optional;
an absent database or incompatible schema must not hide transcript sessions.

Resume command:

```text
codex resume -C <cwd> <provider-session-id>
```

New command:

```text
codex -C <cwd>
```

### 7.2 Claude Code

Scan `~/.claude/projects` recursively for top-level UUID `.jsonl` transcript
files. Ignore transcripts below `subagents/`, `agent-*` files, and transcripts
whose records have `isSidechain = true`; exclude them from Sessions, status
tracking, and summary generation. Sort by modification time descending and
inspect at most the newest 60 files. Reuse parsed results while file metadata is
unchanged.

Ignore claude-mem's internal observer sessions whose cwd is
`~/.claude-mem/observer-sessions`; they summarize another primary session and
are not user workspaces.

Stream complete records to reject any sidechain marker and recover the first
real user prompt. Read `sessionId`, `cwd`, `gitBranch`, provider/AI/custom
titles, latest prompts, conversation summaries, tags, PR/MR links, user
messages, assistant messages, and tool results from the bounded transcript
head and tail.

Resume command:

```text
claude --resume <provider-session-id>
```

Run it with process cwd set to the session cwd.

New command:

```text
claude --session-id <new UUID> --name <directory name>
```

Run it with process cwd set to the selected cwd.

### 7.3 pi

Scan `$PI_CODING_AGENT_DIR/sessions`, or `~/.pi/agent/sessions` when the variable
is unset, for `<ISO stamp>_<uuid>.jsonl` transcripts. The stamp is what tells a pi
transcript apart from a Claude one, whose stem is the bare uuid. Sort by
modification time descending and inspect at most the newest 60 files. Reuse parsed
results while file metadata is unchanged.

Read the session id and `cwd` from the first `session` header line and the display
name from the newest `session_info` entry. pi stores a conversation tree, and
`/tree` can move the leaf back onto an earlier branch; read the file in write order
rather than walking the branch, as the Codex and Claude readers do with their own
transcripts. Take user prompts, assistant text, `toolCall` names, `toolResult`
text, `bashExecution` commands, and compaction and branch summaries as activity. An
assistant message with `stopReason = "error"` marks the session failed and its
`errorMessage` is the activity line.

Resume and new command are the same, because `--session-id` reopens the session
when the file exists and creates it with that id when it does not:

```text
pi --tui-mode regular --session-id <provider-session-id> -e <hook extension>
```

A new session additionally passes `--name <directory name>`. A resume must not, or
it overwrites a name the user set with `/name`. Run it with process cwd set to the
session cwd.

pi has no hook commands. The console writes a pi extension into its state
directory and passes it with `-e`; the extension forwards `session_start`,
`before_agent_start`, `tool_execution_start`, `tool_execution_end`, `user_bash`,
`agent_settled`, and `session_shutdown` to `agent-console hook pi` as the same JSON
hook payload the other providers send. Its child processes are detached:
`session_shutdown` fires while pi is exiting, and a child still in pi's process
group is killed before it can report the session ended. An extension that cannot be
written is recorded in diagnostics and the session starts without it.

### 7.4 Refresh

- Perform discovery on startup.
- Display at most the 50 most recently modified sessions from the last seven
  days. Managed sessions remain visible for the current process lifetime.
- Refresh transcript metadata every two seconds.
- Preserve selection by stable session key.
- Refresh is automatic; there is no manual refresh key.
- Never start an LLM summary merely because an old session was discovered for
  the first time. Summarize the selected session and sessions whose transcript
  changes after startup.

## 8. Normalized events and deterministic status

Normalized event kinds:

```text
session_started
user_message
agent_message
tool_started
tool_completed
approval_requested
user_input_requested
turn_completed
turn_failed
session_ended
```

Status precedence, highest first:

1. `waiting`: an unresolved approval or user-input request exists.
2. `working`: the managed child is alive and a turn/tool is active.
3. `failed`: the most recent turn failed.
4. `idle`: none of the above.

Transcript modification time alone may mark an unmanaged session as
`recently active`, but it must not claim that the process is attachable.

Approval text and identifiers come directly from provider events. An LLM may
shorten the wording but must not invent decisions or mark them resolved.

## 9. Managed PTYs

Use `portable-pty` to launch agent and shell children. Each selected session can
own one agent terminal and multiple shell terminals:

```text
agent PTY: codex, claude, or pi native TUI
shell PTYs: zero or more $SHELL -l children, each with cwd set to the session cwd
```

The output pump starts immediately and continues while the dashboard is shown.
It stores:

- a VT100 screen snapshot for restoring the visible terminal;
- a bounded plain-text tail for dashboard activity and shell capture;
- child exit status.

Entering a workspace:

1. Reuse the application's alternate screen. Session navigation must never
   reveal or redraw the Dashboard as an intermediate frame.
2. Keep the session list visible on the left. A two-line summary immediately
   below its heading continuously shows working, waiting, idle, and failed
   totals using the Dashboard status colors. Its `Cdx`, `Cla`, and `Pi` labels use
   the same provider colors as the Dashboard, padded to a common width so the
   columns beside them line up.
3. Render the agent PTY in the upper-right pane.
   Launch Codex with `--no-alt-screen` and pi with `--tui-mode regular` so their
   transcripts become retained pane history instead of an application-private
   alternate-screen page.
4. Render up to three shell PTYs side by side in the lower-right pane and all
   shell identities in a list at the far right.
5. Resize each visible child PTY to its pane.
6. Forward raw stdin only to the focused agent or shell. When the session list
   has focus, Up/Down or `j`/`k` changes the selected session and immediately
   rebinds the right side to its latest transcript preview and retained shells
   without starting a provider. That preview carries the title, workspace path,
   branch, the first user prompt, the rolling summary's task/current action/next
   step/first blocker, any stale-summary reason, and the recent transcript. The
   summary lives here rather than in the title, so periodic resummarizing changes
   what the session is doing now without changing what it is. The selected item owns the focus highlight;
   workspace group headings and the `SESSIONS` heading are never selectable.
   Enter explicitly starts/resumes and focuses its agent.
7. `Ctrl-\` cycles Agent -> Shell -> Sessions -> Agent, creating the first
   shell when needed. `Ctrl-Q` returns to the Dashboard. `Ctrl-^` adds a Shell
   directly from Agent
   or Shell focus. In Shell focus, `Ctrl-N` selects the next Shell and `Ctrl-X`
   closes it immediately. `Ctrl-N` and
   `Ctrl-X` are forwarded in Agent focus. `Ctrl-O`, `Ctrl-]`, `Shift-End`,
   `Ctrl-T`, `Ctrl-Enter`, `Esc`,
   printable input, unrelated Ctrl keys, Alt keys, and function keys remain
   child input. Modified-key reporting is enabled only while the Agent has
   focus, allowing nested providers to distinguish `Ctrl-Enter` from Enter
   without changing interactive Shell input.
8. Session-list focus is navigation, not a separate command mode. Up/Down or
   `j`/`k` selects sessions; Enter activates the selected Agent; `/` searches
   sessions live; `a` jumps to the next unread alert; `?` opens the effective
   Workspace key-binding panel; `n` opens a new-session dialog using the
   selected workspace; `s` creates a Shell;
   `m` maximizes and focuses the previously selected Shell; `h` starts/resumes,
   maximizes, and focuses the selected Agent. Returning to Session-list focus
   restores the regular Agent/Shell split; `+`/`_`
   resize the shell area; `y` copies its current command block; digits select a
   numbered shell; and `x`
   archives/restores the selected session.
9. Returning to Dashboard forces one redraw; switching sessions inside the
   Workspace does not leave the alternate screen.

Each agent and shell has an independent viewport. `Shift-PageUp` and
`Shift-PageDown` move it through retained history and the pane title displays
the current `SCROLL +N` offset. Ordinary
child input first returns the focused pane to the live tail. A full-screen
agent that enables mouse reporting receives its native clicks. Its native
wheel receives events only at the live tail when no retained outer history can
move; otherwise the wheel scrolls Agent Console's independent viewport. An
ordinary outer drag is reserved for Agent Console selection and copies on
release; no separate copy shortcut is required. The terminal's mouse-reporting
bypass modifier (for example, Option in iTerm2) remains available for
terminal-native text selection followed by the terminal's normal copy command.
A full-screen agent without mouse reporting receives alternate-screen cursor
scroll events, matching terminal-emulator behavior used by Claude Code.

Each outer viewport retains up to 2,000 rows independently of the 128 KiB raw
daemon replay tail. When a Codex-style partial scroll region has outgrown that
raw tail, the daemon checkpoint also carries its retained formatted rows so a
new TUI can reconnect without losing the available viewport history.

The detached PTY daemon owns managed agent and shell children. TUI exit or
crash detaches without terminating them; a later TUI reconnects by stable
session identity and restores retained output. If a client falls behind the
bounded raw-output tail, the daemon sends an authoritative VT screen
checkpoint instead of replaying bytes from the middle of an ANSI control
sequence. Crossing the raw-output bound never terminates or restarts a child;
only an explicit close action terminates a shell.

The persisted managed transcript fingerprint records the transcript version
last associated with a Console-owned Agent. If discovery observes a different
fingerprint, Session preview always shows the new transcript. A live
daemon-owned Agent is authoritative: Dashboard Enter, Workspace Enter, and
direct Agent focus reattach that PTY without restarting it or treating its own
new transcript output as an external conflict. If no live PTY remains,
activation resumes the provider from current history.

## 10. Shell capture and injection

The shell capture is the latest non-empty plain-text output, capped at 200
lines and 16 KiB.

- `s`: create another shell PTY and enter the session workspace.
- the configurable `copy` action sends the shell capture to the platform
  clipboard command.
- the configurable `stage` action stages this exact text for the selected
  agent terminal:

```text

Shell output from <cwd>:
<shell-output>
...
</shell-output>

```

When the user next enters the agent terminal, send the staged text as bracketed
paste after the native UI is ready. Do not send Enter. Clear the staged value
only after a successful PTY write.

## 11. Continuous summary

### 11.1 Separation

Never ask the active coding session to summarize itself. Run an isolated,
non-persistent summarizer process. The coding process must continue if summary
generation fails.

The backend is always `same-provider`:

- Codex session uses `codex exec`.
- Claude session uses `claude --print`.

The environment variable `AGENT_CONSOLE_SUMMARIZER` may be `same-provider` or
`off`. Any other value falls back to `same-provider`. Cross-provider summaries
are forbidden.

### 11.2 Triggering

- Queue the selected session after startup if it has no cached summary.
- Queue a session two seconds after its transcript or normalized event inbox
  changes.
- Coalesce repeated changes for the same session.
- Do not run more than one summarizer process at a time.
- Use a FIFO queue and rotate temporarily ineligible entries so one session or
  provider cannot starve another.
- Do not refresh the same session more often than the configured
  `min_interval_seconds` while it is changing.
- Back off a failing session exponentially from `failure_backoff_seconds`,
  capped after six doublings.
- After `circuit_failures` consecutive failures, stop that provider for
  `circuit_cooldown_seconds`; the other provider remains eligible.
- Manual retry clears that session's throttle/backoff and its provider circuit,
  then inserts the session at the front of the queue.
- Always allow a final refresh after a turn completes.

### 11.3 Input

The prompt contains only:

1. The JSON output contract.
2. The previous structured summary, if any.
3. New normalized events/recent transcript records after the saved
   fingerprint.

Cap prompt input at 48 KiB. Remove terminal control sequences. Do not include
environment variables. Replace common credential assignments and bearer tokens
with `[REDACTED]`.

### 11.4 Output schema

```json
{
  "task": "short task description",
  "status": "working|waiting|idle|failed",
  "progress": ["completed fact"],
  "current_action": "what is happening now",
  "next_step": "likely next concrete action",
  "needs_user": [
    {"id": "provider event id", "question": "decision text"}
  ],
  "blockers": ["current blocker"]
}
```

Model-returned `status` and `needs_user` are advisory. The deterministic reducer
overwrites them before display.

### 11.5 Provider commands

Codex summary command uses an empty neutral working directory:

```text
codex exec --ephemeral --sandbox read-only --ignore-user-config
  --skip-git-repo-check --output-schema <schema-file> <prompt>
```

Claude summary command uses an empty neutral working directory:

```text
claude --safe-mode --print --tools "" --no-session-persistence
  --output-format json --json-schema <schema-json> <prompt>
```

pi summary command uses an empty neutral working directory. It has no schema
flag, so the schema rides in the system prompt and the answer is recovered from
the last stdout line that parses as a JSON object:

```text
pi --print --no-session --no-tools --no-extensions --no-skills
  --no-prompt-templates --no-context-files --no-approve
  --append-system-prompt <schema instruction> <prompt>
```

Pass the prompt as the final argument and give the summarizer no stdin. A
configured Provider Command may wrap the provider in a terminal automation tool
that hands the child a PTY, and a PTY stdin never delivers a piped prompt, so a
stdin prompt fails or hangs for every such user. Drain stdout and stderr
concurrently while the child runs; a wrapper merges the provider's own chrome
into both, and either one can exceed its pipe buffer.

Recover the summary from the last line of the captured stream that is a JSON
object, after removing ANSI sequences. A wrapper also surfaces provider banners,
warnings, token counts, and cursor escapes, so the stream is not JSON on its own.
Save the previous summary on timeout, non-zero exit, missing JSON, or schema
mismatch. Display the error as `summary stale: <short reason>` and record it in
the rotating diagnostics log, because most sessions never reach a status that
shows the error on screen.

## 12. Dashboard interaction

```text
Up/Down or j/k   Select a session
Enter            Open workspace focused on the selected agent
s                Add shell and open workspace focused on it
n                Open new-session dialog
a                Jump to the next unread waiting/failed alert
r                Retry the selected session's summary, clearing backoff
/                Search sessions by metadata, provider, workspace, or status
x                Archive the selected session, or restore an archived one
?                 Open the effective key-binding panel
q or Esc         Quit (Esc closes a dialog first)
Ctrl-\ / Ctrl-Q  Focus cycle / Dashboard (all Workspace modes)
Ctrl-^            Add and focus a Shell (Agent or Shell focus)
Ctrl-N/X         Next / close Shell (Shell focus only; forwarded in Agent)
j/k or Up/Down   Select session (FOCUS SESSIONS only)
/                 Search sessions (FOCUS SESSIONS only)
a                Jump to next unread alert (FOCUS SESSIONS only)
?                 Open Workspace key-binding panel (FOCUS SESSIONS only)
n/s              New session / add Shell (FOCUS SESSIONS only)
1..9             Select shell (FOCUS SESSIONS only)
m                Maximize and focus last-selected Shell (FOCUS SESSIONS only)
h                Maximize and focus selected Agent (FOCUS SESSIONS only)
+/_              Resize shell area (FOCUS SESSIONS only)
y/x              Copy command block / archive session (FOCUS SESSIONS only)
```

There is no command menu. Press `t` only as the contextual response to a
session-lease conflict that offers force takeover. Discovery is automatic.
Manual refresh, pinning, per-session summary
enable/disable, and separate provider/status/workspace cycling filters are not
part of the interaction model. Live search covers those metadata dimensions.

Each shell has a user-visible name. Shells that exit remain as panes with their
exit status until explicitly closed. The Workspace records a capture boundary
whenever Enter submits input to a shell; copy/stage operations use output after
that boundary, with the bounded raw tail as the pre-command fallback.

A notification is created only when a background session transitions into
`waiting` or `failed`; repeated refreshes do not duplicate it. The alert shows
the session task and pending decision or failure reason.

The left list is grouped by exact workspace path. Each session row shows its
provider, status/age, and title. The title is the session's first user prompt,
excluding provider-injected setup records such as `# AGENTS.md instructions`,
and falls back to branch or a short session ID. It never follows the latest
prompt or the rolling summary, so a session keeps one stable identity for its
whole life, including discovery refreshes and application restarts; slash
commands or shell echoes cannot rename it. The right side begins with a
full-width selected-session focus
panel: full title, `NEEDS YOU`/`BLOCKER`/`NOW`/`LAST` priority, next step, and
full workspace context. Below it is an adaptive session-card grid with no more
than three cards per row. Card titles stay short (provider/status/age/shells),
while a bright body `TASK` row carries the identity. The priority row always
has deterministic fallback text, including failed sessions without a generated
blocker. Red/yellow/green borders retain failed/waiting/working meaning. The
selected card keeps a background and marker so selection remains visible when
the grid scrolls.

Search matches user alias, provider session name, generated title, first and
latest user prompts, conversation summary, Claude tag and PR/MR metadata,
workspace name, full cwd, branch, provider session ID, provider, status, and
active/archived state. Search is available from Dashboard and from the focused
Workspace Sessions list. It filters the session list after every typed
character or Backspace; the search UI explicitly labels live filtering, Enter
fixing the current query without remounting the Workspace, and Esc restoring
the query and selection from before search opened. The Dashboard session list
responds to the mouse wheel. In Workspace, the mouse wheel scrolls the
independent agent or shell viewport under the pointer using either SGR or
legacy X10 mouse input.
Aliases and archive state are persistent user metadata. Archive moves a
session into one dimmed `Archived` group after all active workspace groups.
Archived sessions remain selectable and `x` restores them to their workspace
group. The alias always takes display precedence over generated summary text.

Each card compactly shows task, priority detail, workspace/branch, next
step, status, activity age, and open-shell count. Selection changes update both
the focus panel and highlighted overview card in the same frame.

The footer uses readable textual keycaps such as `[Enter]`; never render
dark-on-dark filled rectangles.

## 13. New-session dialog

The dialog has two fields in focus order:

1. provider: cycle with Right or `l` and back with Left or `h`, values `codex`,
   `claude`, and `pi`;
2. workspace: editable text, initialized to the dashboard startup directory.

Shift-Tab is the sole field-switching key and toggles between provider and
workspace. Focusing workspace selects the whole initial value. The first typed
or pasted character replaces it; Backspace/Delete clears it. Store a
Unicode-scalar caret position. Left/Right moves one character, Home/End jumps
to the start or end, typed input inserts at the caret, Backspace removes the
preceding character, and Delete removes the following character. Render a
visible caret and a horizontally cropped long path with ellipses while keeping
the caret in view.
While editing, enumerate matching filesystem directories only. Up/Down changes
the candidate. Tab accepts the selected candidate with a trailing separator
and keeps the workspace field active for child-directory completion. Tab has no
field-switching behavior. Right remains a caret key. Preserve a leading `~/` in
displayed completions. Enter validates that cwd is a directory, creates the
provider session, closes the dialog, selects the session, and immediately
enters its native terminal.
Validation errors stay in the dialog. Guidance on the last row changes with
the active field/completion state and always exposes field movement, editing or
completion, Enter, and Esc. The search and alias dialogs similarly expose live
filter/apply and cancel behavior.

### 13.1 Blocking provider dialogs

A dialog is never written to a transcript, so a session stopped on one is
indistinguishable from an idle one unless the visible screen is read.

Report a blocking prompt only for a *contiguous block of option lines directly
above its own footer*: at most two blank lines may separate the block from the
footer, and the block ends at the first blank line above it. Evidence gathered
from anywhere else on the screen does not count. Anchoring is load-bearing --
without it a stale numbered list scrolled above a live dialog outranks the dialog,
and answering then confirms whatever the agent has highlighted.

A footer names the key to press: `to confirm`, `to select`, `to cancel`, or
`to continue` when the same line also says `press` or `enter`. `to continue`
alone is ordinary prose.

Two shapes, tried in this order:

- **Numbered** (Codex): each line carries its own digit. Answer by writing that
  digit and a carriage return.
- **Cursor** (Claude Code 2.1.251+, pi): no digits; exactly one line carries a
  selection marker and the siblings align at its label column. Number them by
  position and report which is marked. A line at any other position continues the
  option above it — pi wraps an option's path onto its own line at a *shallower*
  indent, so indentation cannot distinguish a wrap from a sibling.

Selection markers are `❯`, `›`, and `→` only. A numbered option may additionally
carry `>` or `*`, because there the digit does the work; treating those as cursor
markers turns a markdown bullet list into a menu.

Strip a box border from both ends of a line before parsing, so a framed dialog
parses like a bare one.

Answer a cursor menu in two steps: write the arrow keys, read the screen back,
and write the carriage return only once the marked label equals the requested one.
Give up after three seconds without confirming. A positional answer cannot be
verified at parse time — a label that wraps at the viewport width looks exactly
like a sibling option — so the read-back is what prevents confirming the wrong row.

A dialog that is reported also blocks `/prompt`, which reports the question rather
than typing into it.

pi emits no extension events at all while its trust dialog is up, so for pi the
screen is the only place a blocking dialog exists.

## 14. Failure behavior

- Missing one provider: show its doctor error; continue with the others.
- Missing transcript directory: treat as zero sessions.
- Missing cwd: show session as unavailable; Enter reports an error.
- Child spawn failure: return to dashboard with an error banner.
- Summarizer failure: keep old summary and mark it stale.
- Malformed transcript line: skip the line.
- All background thread failures are converted to visible messages; no panic in
  the terminal cleanup path.

On the first managed Codex launch, Codex may show its built-in hook trust page.
The user reviews the generated `agent-console hook codex` commands and
chooses `Trust all and continue`. This is a one-time official safety step, not a
manual configuration-file edit. Until trusted, transcript discovery continues
to work but live Codex approval events are unavailable.

## 15. Verification

Automated tests must cover:

1. Codex, Claude, and pi transcript discovery from fixtures.
2. Stable identity and refresh preserving selection.
3. Deterministic status precedence.
4. Summary rolling prompt, redaction, output validation, and status override.
5. Summary backend command construction.
6. PTY process output capture, input, detach-independent lifetime, and exit.
7. Shell capture bounds and staged bracketed paste.
8. State persistence round trip and malformed-state recovery.
9. Ratatui rendering at 80x24 and 160x50 without panic.
10. Doctor behavior with fake and missing provider binaries, supported-version
    policy, resume/hook/summary help contracts, clipboard, state permissions,
    hook ingress/index, and daemon health.
11. A provider contract matrix at every supported floor and current real local
    `--version`/`--help` smoke checks without model invocation.
12. A published macOS archive is extracted onto a fresh inode, signature
    verified, quarantined, launched, and exercised through the documented
    atomic install/upgrade path. Directly overwriting an already launched
    signed inode is unsupported because macOS can retain its old signature
    cache and kill the replacement.

Manual smoke test:

1. Start the dashboard in a real terminal.
2. Confirm real recent Codex/Claude/pi sessions appear.
3. Resume one session and return with `Ctrl-Q`.
4. Add multiple shells, switch focus, run `pwd`, close one, and return.
5. Copy and stage output from the selected shell.
6. Re-enter the agent and confirm staged text is present but not submitted.
7. Confirm summary refresh does not add a message to the coding conversation.

## 16. Completion criteria

The implementation is complete only when all automated tests pass, `cargo fmt
--check` and `cargo clippy -- -D warnings` pass, both installed providers pass
`doctor`, and the dashboard smoke test can start and exit without corrupting the
terminal.
