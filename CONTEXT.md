# Domain language

- **Runtime** — the continuously advancing application state machine. It owns
  discovery, normalized events, summaries, notifications, and view changes.
- **View** — either Dashboard or Workspace. Changing View never pauses Runtime.
- **Workspace** — one selected Agent Pane plus zero or more Shell Panes and the
  shared Session Sidebar.
- **Managed Session** — a provider conversation whose Agent PTY is owned by the
  PTY Daemon.
- **Pane** — one rendered Agent or Shell terminal, including its independent
  viewport and scroll position.
- **Live Tail** — viewport position zero, following the latest terminal frame.
- **Session Lease** — the exclusive cross-process right to run one provider
  session ID.
- **PTY Daemon** — the long-lived local process that owns Managed Session and
  Shell PTYs independently of any TUI process.
- **Notification** — a deduplicated state transition that needs user attention,
  principally waiting or failed.
- **Provider Command** — the single optional custom argv prefix for Codex or
  Claude, used consistently for launch, resume, and same-provider summary.
- **Command Block** — one shell command plus the output it produced and its exit
  status.
- **Event Index** — SQLite-backed normalized hook events plus a byte cursor and
  rotation fingerprint for each JSONL ingress generation.
