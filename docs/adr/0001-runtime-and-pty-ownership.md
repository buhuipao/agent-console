# ADR-0001: continuous Runtime and daemon-owned PTYs

Status: accepted

## Context

At the time of this decision, the prototype entered Workspace through a
blocking PTY loop. While that loop was active, Dashboard discovery, event
reduction, summary scheduling, and cross-session attention state did not
advance. PTYs were owned by the TUI process, so a normal exit or crash also
ended shells and live agents.

## Decision

Runtime is a single continuously advancing Module. Dashboard and Workspace are
Views selected within Runtime; neither owns a separate blocking event loop.

PTY ownership moves to a local PTY Daemon. The TUI communicates with it over a
versioned local Unix-socket interface. TUI exit detaches. Explicit stop or
force-takeover operations terminate a managed child.

The terminal Module retains VT state and scrollback. Runtime consumes terminal
snapshots and sends input through the daemon interface; rendering code never
owns child processes.

## Consequences

- Cross-session status and notifications remain live in every View.
- TUI crashes and upgrades do not destroy shell state.
- Session Lease enforcement has one authoritative owner.
- Daemon protocol compatibility and private socket/state permissions become
  release requirements.
- The process-local PTY adapter remains useful for tests and is the Windows
  ConPTY implementation; Unix uses the daemon adapter.
