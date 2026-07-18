# Agent Console implementation specification

Status: implementation contract for the Rust prototype.

The ordered production requirements and final real-machine acceptance matrix
are tracked in [PLAN.md](PLAN.md). Domain terms used by that work are defined
in [CONTEXT.md](CONTEXT.md), and accepted architecture decisions live under
`docs/adr/`.

This document is intentionally explicit so an implementation agent can execute
it without making product decisions. If code and this document disagree, this
document wins.

## 1. Goal

Build one local terminal application that can:

1. Discover recent persisted Codex and Claude Code sessions.
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

- macOS and Unix-like terminals.
- `codex` and/or `claude` available on `PATH`.
- Rust stable with edition 2024.
- No tmux dependency.
- Clipboard integration uses `pbcopy` when present. Clipboard failure is shown
  in the dashboard and never crashes the app.

## 4. Commands

The binary supports these modes:

```text
agent-console-prototype                 Open the dashboard.
agent-console-prototype hook PROVIDER   Read one hook JSON object from stdin.
agent-console-prototype doctor          Check provider binaries and data paths.
```

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
codex = ["proxychains4", "codex", "--profile", "work"]
claude = ["env", "HTTPS_PROXY=http://127.0.0.1:7890", "claude"]

[profiles.direct]
codex = ["codex"]
claude = ["claude"]

[profiles.corporate_proxy]
codex = ["proxychains4", "codex"]
claude = ["env", "HTTPS_PROXY=http://127.0.0.1:7890", "claude"]

[summary]
min_interval_seconds = 30
failure_backoff_seconds = 30
circuit_failures = 3
circuit_cooldown_seconds = 300

[keys.dashboard]
help = ["?"]

[keys.workspace]
focus = ["ctrl-o"]
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

A named profile overrides only provider commands present in that profile;
missing provider entries fall back to `[providers]`. Store the selected
profile with session metadata and use it for native resume/new invocations and
the isolated same-provider summarizer. The new-session dialog exposes an
explicit profile field; cycling a persisted session profile is an optional,
unbound Dashboard action.

Key maps replace defaults action by action. Reject unknown actions, unsupported
key labels, empty action arrays, and duplicate effective sequences before the
TUI starts. Workspace printable-key actions are active only in `FOCUS SESSIONS`;
the same bytes in agent/shell focus are forwarded. Focus, Dashboard, alert, and
new-Shell chords are global. Next-Shell and close-Shell chords are active only
in Shell focus and are forwarded in Agent focus. The input router and all
displayed control hints use the same resolved binding map. The `help` action
groups direct, Session-list, and child-viewport bindings; `Esc` closes a modal
but is never a Workspace command while a child has focus.

## 6. Session identity

Use this stable key:

```text
<provider>:<provider-session-id>
```

`provider` is exactly `codex` or `claude`.

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

### 7.1 Codex

Scan `$CODEX_HOME/sessions`, or `~/.codex/sessions` when `CODEX_HOME` is unset,
for `rollout-*.jsonl` files. Sort by modification time descending and inspect at
most the newest 60 files. Reuse parsed results while file metadata is unchanged.

Read `session_meta.payload` for `id` and `cwd`. Read only a bounded tail for
recent user, agent, tool, and completion records. The transcript format is an
observation fallback, not a write interface.

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
files. Ignore `agent-*` subagent transcripts. Sort by modification time
descending and inspect at most the newest 60 files. Reuse parsed results while
file metadata is unchanged.

Ignore claude-mem's internal observer sessions whose cwd is
`~/.claude-mem/observer-sessions`; they summarize another primary session and
are not user workspaces.

Read `sessionId`, `cwd`, `gitBranch`, user messages, assistant messages, and
tool results from a bounded tail.

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

### 7.3 Refresh

- Perform discovery on startup.
- Display at most the 50 most recently modified sessions from the last seven
  days. Managed sessions remain visible for the current process lifetime.
- Refresh transcript metadata every two seconds.
- Preserve selection by stable session key.
- A manual `r` key triggers an immediate refresh.
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
agent PTY: codex or claude native TUI
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
2. Keep the session list visible on the left.
3. Render the agent PTY in the upper-right pane.
4. Render up to three shell PTYs side by side in the lower-right pane and all
   shell identities in a list at the far right.
5. Resize each visible child PTY to its pane.
6. Forward raw stdin only to the focused agent or shell. When the session list
   has focus, Up/Down or `j`/`k` changes the selected session and immediately
   rebinds the right side to its latest transcript preview and retained shells
   without starting a provider. The selected item owns the focus highlight;
   workspace group headings and the `SESSIONS` heading are never selectable.
   Enter explicitly starts/resumes and focuses its agent.
7. `Ctrl-O` cycles Agent -> Shell -> Sessions -> Agent, creating the first
   shell when needed. `Ctrl-Q` returns to the Dashboard and `Ctrl-]` jumps to a
   background waiting/failed alert. `Ctrl-\` adds a Shell directly from Agent
   or Shell focus. In Shell focus, `Ctrl-N` selects the next Shell and `Ctrl-X`
   closes it after a second confirmation when it is alive. `Ctrl-N` and
   `Ctrl-X` are forwarded in Agent focus. `Ctrl-T`, `Esc`, printable input,
   unrelated Ctrl keys, Alt keys, and function keys remain child input.
8. Session-list focus is navigation, not a separate command mode. Up/Down or
   `j`/`k` selects sessions; Enter activates the selected Agent; `n` opens a
   new-session dialog using the selected workspace; `s` creates a Shell;
   `r` renames it; `m` maximizes it; `+`/`-` resize the shell area; `y` copies
   its current command block; digits select a numbered shell; and `x`
   archives/restores the selected session.
9. Returning to Dashboard forces one redraw; switching sessions inside the
   Workspace does not leave the alternate screen.

Each agent and shell has an independent viewport. `Shift-PageUp` and
`Shift-PageDown` move it through retained history, `Shift-End` returns to the
live tail, and the pane title displays the current `SCROLL +N` offset. Ordinary
child input first returns the focused pane to the live tail. A full-screen
agent that enables mouse reporting receives its native mouse-wheel event. A
full-screen agent without mouse reporting receives alternate-screen cursor
scroll events, matching terminal-emulator behavior used by Claude Code.

The detached PTY daemon owns managed agent and shell children. TUI exit or
crash detaches without terminating them; a later TUI reconnects by stable
session identity and restores retained output. Only an explicit close action
terminates a shell.

The persisted managed transcript fingerprint records the transcript version
last associated with a Console-owned Agent. If discovery observes a different
fingerprint, Session preview always shows the new transcript. Explicitly
activating an idle/failed stale Agent terminates that stale PTY and resumes the
same provider session from current history. A working/waiting stale Agent is
never terminated automatically; keep preview focus and surface the conflict.

## 10. Shell capture and injection

The shell capture is the latest non-empty plain-text output, capped at 200
lines and 16 KiB.

- `s`: create another shell PTY and enter the session workspace.
- the configurable `copy` action pipes the shell capture to `pbcopy`.
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
- Persist a per-session summary enable/disable flag. A disabled session is
  removed from the queue.
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
  --skip-git-repo-check --output-schema <schema-file> -
```

Claude summary command uses an empty neutral working directory:

```text
claude --safe-mode --print --tools "" --no-session-persistence
  --output-format json --json-schema <schema-json>
```

Parse only the structured final result. Save the previous summary on timeout,
non-zero exit, malformed JSON, or schema mismatch. Display the error as
`summary stale: <short reason>`.

## 12. Dashboard interaction

```text
Up/Down or j/k   Select a session
Enter            Open workspace focused on the selected agent
s                Add shell and open workspace focused on it
n                Open new-session dialog
a                Jump to the next unread waiting/failed alert
/                Search sessions by metadata, provider, workspace, or status
x                Archive the selected session, or restore an archived one
?                 Open the effective key-binding panel
q or Esc         Quit (Esc closes a dialog first)
Ctrl-O/Q/]       Focus cycle / Dashboard / unread alert (all Workspace modes)
Ctrl-\            Add and focus a Shell (Agent or Shell focus)
Ctrl-N/X         Next / close Shell (Shell focus only; forwarded in Agent)
j/k or Up/Down   Select session (FOCUS SESSIONS only)
n/s              New session / add Shell (FOCUS SESSIONS only)
1..9             Select shell (FOCUS SESSIONS only)
r                Rename shell (FOCUS SESSIONS only)
m or +/-         Maximize / resize shell area (FOCUS SESSIONS only)
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
provider, status/age, and summarized task (falling back to branch or a short
session ID). The right side begins with a full-width selected-session focus
panel: full task, `NEEDS YOU`/`BLOCKER`/`NOW`/`LAST` priority, next step, and
full workspace context. Below it is an adaptive session-card grid with no more
than three cards per row. Card titles stay short (provider/status/age/shells),
while a bright body `TASK` row carries the identity. The priority row always
has deterministic fallback text, including failed sessions without a generated
blocker. Red/yellow/green borders retain failed/waiting/working meaning. The
selected card keeps a background and marker so selection remains visible when
the grid scrolls.

Search matches user alias, summarized task, path, branch, provider session ID,
provider, workspace, and status. Search filters the session list after every typed character or
Backspace; Enter keeps the current query and Esc restores the query and
selection from before the search dialog opened. The Dashboard session list responds to the mouse
wheel. In Workspace, the mouse wheel scrolls the independent agent or shell
viewport under the pointer using either SGR or legacy X10 mouse input.
Aliases and archive state are persistent user metadata. Archive moves a
session into one dimmed `Archived` group after all active workspace groups.
Archived sessions remain selectable and `x` restores them to their workspace
group. The alias always takes display precedence over generated summary text.

Each card compactly shows task, priority detail, workspace/branch/profile, next
step, status, activity age, and open-shell count. Selection changes update both
the focus panel and highlighted overview card in the same frame.

The footer uses readable textual keycaps such as `[Enter]`; never render
dark-on-dark filled rectangles.

## 13. New-session dialog

The dialog has three fields in focus order:

1. provider: toggle with Left/Right or `h`/`l`, values `codex` and `claude`;
2. workspace: editable text, initialized to the dashboard startup directory;
3. profile: toggle with Left/Right or `h`/`l`, values `default` plus the configured named
   profiles for the selected provider.

Tab moves to the next field and Shift-Tab moves to the previous field.
Focusing workspace selects the whole initial value. The first typed or pasted
character replaces it; Backspace/Delete clears it. Store a Unicode-scalar
caret position. Left/Right moves one character, Home/End jumps to the start or
end, typed input inserts at the caret, Backspace removes the preceding
character, and Delete removes the following character. Render a visible caret
and a horizontally cropped long path with ellipses while keeping the caret in
view.
While editing, enumerate matching filesystem directories only. Up/Down changes
the candidate, Tab completes it with a trailing separator, and the field stays
active for child-directory completion. Right remains a caret key. The next Tab
advances to profile. Preserve a leading `~/` in displayed completions.
Changing provider resets an unavailable profile to
`default`. Enter validates that the selected profile exists and cwd is a
directory, creates the provider session with that profile, closes the dialog,
selects the session, and immediately enters its native terminal. Validation
errors stay in the dialog.

## 14. Failure behavior

- Missing one provider: show its doctor error; continue with the other.
- Missing transcript directory: treat as zero sessions.
- Missing cwd: show session as unavailable; Enter reports an error.
- Child spawn failure: return to dashboard with an error banner.
- Summarizer failure: keep old summary and mark it stale.
- Malformed transcript line: skip the line.
- All background thread failures are converted to visible messages; no panic in
  the terminal cleanup path.

On the first managed Codex launch, Codex may show its built-in hook trust page.
The user reviews the generated `agent-console-prototype hook codex` commands and
chooses `Trust all and continue`. This is a one-time official safety step, not a
manual configuration-file edit. Until trusted, transcript discovery continues
to work but live Codex approval events are unavailable.

## 15. Verification

Automated tests must cover:

1. Codex and Claude transcript discovery from fixtures.
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

Manual smoke test:

1. Start the dashboard in a real terminal.
2. Confirm real recent Codex/Claude sessions appear.
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
