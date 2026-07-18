# ADR-0001: continuous Runtime and daemon-owned PTYs

Status: accepted

## Context

The prototype enters Workspace through a blocking PTY loop. While that loop is
active, Dashboard discovery, event reduction, summary scheduling, and
cross-session attention state do not advance. PTYs are owned by the TUI
process, so a normal exit or crash also ends shells and live agents.

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
- The prototype's in-process PTY adapter remains useful for tests while the
  daemon adapter is introduced; two adapters make the seam real.
