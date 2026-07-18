# ADR-0002: SQLite state with indexed JSONL ingress

Status: accepted

## Context

Rewriting one JSON state object and rescanning every hook inbox on each refresh
makes cost grow with total history. Hooks still need a minimal, dependable
append-only interface that can run from Codex and Claude Code processes.

## Decision

`state.db` is the persistent-state Module. It owns session cache/metadata and a
normalized Event Index. SQLite runs in WAL mode; legacy `state.json` is an
import Adapter used once.

Provider hooks continue to append JSONL without opening the database. Each
inbox has a current and previous 256 KiB generation. Runtime records a byte
offset and retained-prefix fingerprint for every generation, indexing only new
complete records. A smaller file or changed prefix is a rotation Seam: indexed
rows for that source are discarded and rebuilt.

## Consequences

- Dashboard refresh cost follows new events rather than total JSONL size.
- Hook writers remain simple and isolated from SQLite locking.
- Provider status reduction and summaries share one normalized event source.
- The database, WAL, and JSONL generations are sensitive state and retain
  private filesystem permissions.
