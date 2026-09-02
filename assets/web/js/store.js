// Session list state: fetch, normalise, poll, notify.
//
// `/api/sessions` returns `{ workspaces: [ { path, name, sessions: [...] } ] }`,
// already grouped and ordered by recency; this module only fills in defaults and
// keeps subscribers in sync.

import { fetchSessions } from "./api.js";
import { basename } from "./dom.js";

const POLL_MS = 4000;
/* A hidden page still has to notice that a session started working or is now blocked on a
   decision -- the conversation chrome is built from this list -- so it keeps polling at a
   slower rate rather than freezing until someone looks at the tab again. */
const HIDDEN_POLL_MS = 15000;

const listeners = new Set();

const state = {
  workspaces: [],
  byKey: new Map(),
  showArchived: false,
  loaded: false,
  error: null,
  /* What is in the search box. `appliedQuery` is what the server actually matched on, echoed
     back normalised, so the UI reports the filter that is in force rather than the keystrokes
     that have not reached the server yet. */
  query: "",
  appliedQuery: "",
  /* Whole-machine tallies, deliberately not recomputed from the filtered list: they answer
     "what is happening on this machine", which a narrowed view must not change. */
  counts: { working: 0, waiting: 0, idle: 0, failed: 0 },
  unreadNotifications: 0,
  /** One of the four statuses, or null. A client-side narrowing of the list, not a server filter. */
  statusFilter: null,
};

let timer = null;

export function getState() {
  return state;
}

export function getSession(key) {
  return state.byKey.get(key) || null;
}

export function setShowArchived(value) {
  state.showArchived = value;
  window.localStorage.setItem("agent-console-show-archived", value ? "1" : "0");
  emit();
}

/**
 * Sets the search text and refetches.
 *
 * The query is a request parameter rather than server state, so two clients searching at once
 * never see each other's filter. Debouncing is the caller's job -- this fires a request.
 */
export function setQuery(value) {
  const next = String(value || "");
  if (next === state.query) return Promise.resolve();
  state.query = next;
  emit();
  return refresh();
}

/** Narrows the list to one status, or clears the narrowing when given the active one again. */
export function setStatusFilter(status) {
  state.statusFilter = state.statusFilter === status ? null : status || null;
  emit();
}

export function restorePreferences() {
  state.showArchived = window.localStorage.getItem("agent-console-show-archived") === "1";
}

export function subscribe(handler) {
  listeners.add(handler);
  return () => listeners.delete(handler);
}

function emit() {
  for (const handler of listeners) handler(state);
}

export async function refresh() {
  const query = state.query;
  try {
    const payload = await fetchSessions(query);
    // A slower earlier request must not overwrite the list a later query already produced.
    if (state.query !== query) return;
    const next = normalise(payload);
    state.workspaces = next.workspaces;
    state.byKey = next.byKey;
    state.counts = next.counts;
    state.unreadNotifications = next.unreadNotifications;
    state.appliedQuery = next.query;
    state.error = null;
    state.loaded = true;
  } catch (error) {
    state.error = error;
    state.loaded = true;
  }
  emit();
}

/** Self-rescheduling rather than a fixed interval, so the cadence can follow visibility. */
export function startPolling() {
  stopPolling();
  const tick = async () => {
    await refresh();
    if (timer === null) return;
    timer = window.setTimeout(tick, delay());
  };
  timer = window.setTimeout(tick, delay());
}

function delay() {
  return document.visibilityState === "visible" ? POLL_MS : HIDDEN_POLL_MS;
}

function stopPolling() {
  if (timer !== null) window.clearTimeout(timer);
  timer = null;
}

function normalise(payload) {
  const byKey = new Map();
  const workspaces = (payload?.workspaces || []).map((workspace) => ({
    path: workspace.path || "",
    name: workspace.name || basename(workspace.path) || workspace.path || "workspace",
    sessions: (workspace.sessions || []).map(normaliseSession),
  }));
  for (const workspace of workspaces) {
    for (const session of workspace.sessions) byKey.set(session.key, session);
  }
  const counts = payload?.counts || {};
  return {
    workspaces,
    byKey,
    counts: {
      working: Number(counts.working) || 0,
      waiting: Number(counts.waiting) || 0,
      idle: Number(counts.idle) || 0,
      failed: Number(counts.failed) || 0,
    },
    unreadNotifications: Number(payload?.unread_notifications) || 0,
    // An older build answers without it; falling back to what was asked keeps the echo honest.
    query: typeof payload?.query === "string" ? payload.query : state.query.trim().toLowerCase(),
  };
}

function normaliseSession(raw) {
  return {
    key: raw.key,
    title: raw.title || raw.name || raw.key,
    agent: raw.agent || "codex",
    status: raw.status || "idle",
    cwd: raw.cwd || "",
    branch: raw.branch || null,
    /** The explicit rename, distinct from the derived `title` the rename dialog falls back to. */
    alias: raw.alias ?? null,
    archived: Boolean(raw.archived),
    managed_alive: raw.managed_alive !== false,
    activity_age: raw.activity_age || "",
    updated_at: raw.updated_at ?? null,
    summary: raw.summary || null,
    pending_decisions: Array.isArray(raw.pending_decisions) ? raw.pending_decisions : [],
  };
}

export function visibleWorkspaces() {
  return state.workspaces
    .map((workspace) => ({
      ...workspace,
      sessions: workspace.sessions.filter(isVisible),
    }))
    .filter((workspace) => workspace.sessions.length > 0);
}

function isVisible(session) {
  if (!state.showArchived && session.archived) return false;
  return !state.statusFilter || session.status === state.statusFilter;
}
