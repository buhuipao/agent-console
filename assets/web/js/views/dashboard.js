// Dashboard: sessions grouped under collapsible workspace headers, plus the search box and
// the status counts that sit above them.
//
// The TUI reaches the same two things through modes: a search dialog that rewrites the one
// list it draws, and a header line you read. Here the search is a field whose value is a
// request parameter, so two clients can search at once without moving each other's list, and
// the counts are buttons -- tapping "waiting" narrows the list to what is waiting, which is
// the thing anyone reading that number was about to do by hand.

import { ApiError, archiveSession } from "../api.js";
import { byId, clear, el, inferHome, prettyPath, toast } from "../dom.js";
import { sessionHash } from "../router.js";
import {
  getState,
  refresh,
  setQuery,
  setShowArchived,
  setStatusFilter,
  subscribe,
  visibleWorkspaces,
} from "../store.js";

const COLLAPSE_KEY = "agent-console-collapsed-workspaces";
/* Long enough that a phone keyboard's word-at-a-time typing is one request, short enough that
   the list feels like it is following along. */
const SEARCH_DEBOUNCE_MS = 250;
const STATUSES = ["working", "waiting", "idle", "failed"];

const nodes = {};
let activeKey = null;
let collapsed = new Set();
let signature = null;

function loadCollapsed() {
  try {
    const raw = JSON.parse(window.localStorage.getItem(COLLAPSE_KEY) || "[]");
    return new Set(Array.isArray(raw) ? raw : []);
  } catch (error) {
    return new Set();
  }
}

function persistCollapsed() {
  window.localStorage.setItem(COLLAPSE_KEY, JSON.stringify([...collapsed]));
}

export function initDashboard() {
  nodes.list = byId("workspaces");
  nodes.empty = byId("sessions-empty");
  nodes.count = byId("session-count");
  nodes.showArchived = byId("show-archived");
  nodes.search = byId("session-search");
  nodes.searchClear = byId("search-clear");
  nodes.searchEcho = byId("search-echo");
  nodes.counts = byId("status-counts");
  collapsed = loadCollapsed();

  nodes.showArchived.checked = getState().showArchived;
  nodes.showArchived.addEventListener("change", () => setShowArchived(nodes.showArchived.checked));
  nodes.search.addEventListener("input", onSearchInput);
  // A `type="search"` field's native clear button fires `search`, not `input`, in Safari.
  nodes.search.addEventListener("search", onSearchInput);
  nodes.searchClear.addEventListener("click", clearSearch);
  nodes.counts.addEventListener("click", onCountClick);
  renderCounts(getState());

  subscribe(render);
  render(getState());
}

// -------------------------------------------------------------------- search

let searchTimer = null;

function onSearchInput() {
  const value = nodes.search.value;
  nodes.searchClear.hidden = value === "";
  window.clearTimeout(searchTimer);
  searchTimer = window.setTimeout(() => setQuery(value), SEARCH_DEBOUNCE_MS);
}

function clearSearch() {
  window.clearTimeout(searchTimer);
  nodes.search.value = "";
  nodes.searchClear.hidden = true;
  setQuery("");
  nodes.search.focus();
}

/** What the server actually matched on, so a stray space or capital cannot look like a miss. */
function renderSearchEcho(state) {
  const active = state.appliedQuery !== "";
  nodes.searchEcho.hidden = !active;
  if (active) nodes.searchEcho.textContent = `filtering on “${state.appliedQuery}”`;
}

// ------------------------------------------------------------ status counts

/**
 * The four tallies, as filters.
 *
 * They always describe the whole machine -- the server computes them before `?q=` narrows
 * anything -- so the strip is labelled "all sessions" and a count never moves when the list
 * does. A pressed chip is this client's view of that list, nothing more.
 */
let countsSignature = null;

function renderCounts(state) {
  // The list polls every few seconds. Rebuilding the chips each time would throw away the
  // focus ring of anyone tabbing through them, so they are only redrawn when they change.
  const next = JSON.stringify([state.counts, state.statusFilter]);
  if (next === countsSignature) return;
  countsSignature = next;
  clear(nodes.counts);
  for (const status of STATUSES) {
    const count = state.counts[status] || 0;
    const active = state.statusFilter === status;
    nodes.counts.append(
      el(
        "button",
        {
          type: "button",
          class: `status-chip ${status}${active ? " active" : ""}`,
          dataset: { status },
          "aria-pressed": String(active),
          title: active
            ? `Showing only ${status} sessions — tap to show all`
            : `${count} ${status} on this machine — tap to show only these`,
        },
        [
          el("span", { class: `dot ${status}`, "aria-hidden": "true" }),
          el("span", { class: "status-chip-count", text: String(count) }),
          el("span", { class: "status-chip-word", text: status }),
        ],
      ),
    );
  }
}

function onCountClick(event) {
  const chip = event.target.closest("[data-status]");
  if (!chip) return;
  setStatusFilter(chip.dataset.status);
}

export function setActiveSession(key) {
  activeKey = key;
  for (const row of nodes.list ? nodes.list.querySelectorAll(".session-row") : []) {
    row.classList.toggle("active", row.dataset.key === key);
  }
}

function render(state) {
  if (!nodes.list) return;
  const workspaces = visibleWorkspaces();
  const total = workspaces.reduce((sum, workspace) => sum + workspace.sessions.length, 0);
  const home = inferHome(workspaces.map((workspace) => workspace.path));

  renderCounts(state);
  renderSearchEcho(state);

  // Polling runs every few seconds; rebuilding an unchanged list would reset
  // scroll position and fight with a tap that is already in flight.
  const next = describe(workspaces, state.error);
  if (next === signature) return;
  signature = next;

  clear(nodes.list);

  if (state.error) {
    nodes.list.append(
      el("p", { class: "banner error", text: `Could not load sessions: ${state.error.message}` }),
    );
  }

  for (const workspace of workspaces) nodes.list.append(workspaceSection(workspace, home));

  nodes.empty.hidden = total !== 0 || Boolean(state.error);
  nodes.empty.textContent = narrowed(state)
    ? "No session matches this search."
    : "No sessions yet. Use “New session” to start one.";
  nodes.count.textContent = total ? `${total} session${total === 1 ? "" : "s"}` : "";
  setActiveSession(activeKey);
}

function narrowed(state) {
  return state.appliedQuery !== "" || state.statusFilter !== null;
}

/** Cheap change detector for the rendered list. */
function describe(workspaces, error) {
  return JSON.stringify([
    error ? error.message : null,
    workspaces.map((workspace) => [
      workspace.path,
      workspace.name,
      workspace.sessions.map((session) => [
        session.key,
        session.title,
        session.status,
        session.agent,
        session.activity_age,
        session.branch,
        session.archived,
        session.pending_decisions.length,
      ]),
    ]),
  ]);
}

function workspaceSection(workspace, home) {
  const isCollapsed = collapsed.has(workspace.path);
  const working = workspace.sessions.filter((session) => session.status === "working").length;
  const waiting = workspace.sessions.filter(
    (session) => session.status === "waiting" || session.pending_decisions.length > 0,
  ).length;

  const tally = el("span", { class: "workspace-tally" }, [
    waiting ? el("span", { class: "dot waiting", title: `${waiting} waiting on you` }) : null,
    working ? el("span", { class: "dot working", title: `${working} working` }) : null,
    el("span", { text: String(workspace.sessions.length) }),
  ]);

  const header = el(
    "button",
    {
      class: "workspace-header",
      type: "button",
      "aria-expanded": String(!isCollapsed),
      title: workspace.path || "workspace",
    },
    [
      el("span", { class: "workspace-caret", "aria-hidden": "true", text: "▾" }),
      el("span", { class: "workspace-icon", "aria-hidden": "true", text: "📁" }),
      el("span", { class: "workspace-labels" }, [
        el("span", { class: "workspace-name", text: workspace.name }),
        el("span", { class: "workspace-path", text: prettyPath(workspace.path, home) }),
      ]),
      tally,
    ],
  );

  const list = el("ul", { class: "workspace-sessions" });
  for (const session of workspace.sessions) {
    list.append(el("li", { class: "session-row-wrap" }, [sessionRow(session, home), archiveButton(session)]));
  }

  const section = el("section", {
    class: "workspace",
    dataset: { collapsed: String(isCollapsed), path: workspace.path },
  }, [header, list]);

  header.addEventListener("click", () => {
    const next = section.dataset.collapsed !== "true";
    section.dataset.collapsed = String(next);
    header.setAttribute("aria-expanded", String(!next));
    if (next) collapsed.add(workspace.path);
    else collapsed.delete(workspace.path);
    persistCollapsed();
  });

  return section;
}

function sessionRow(session, home) {
  const pending = session.pending_decisions.length;
  const sub = el("span", { class: "session-row-sub" }, [
    el("span", { class: `status-word ${session.status}`, text: session.status }),
    el("span", { class: "sep", text: "·" }),
    el("span", { class: `agent-badge ${session.agent}`, text: session.agent }),
    session.activity_age ? el("span", { class: "sep", text: "·" }) : null,
    session.activity_age
      ? el("span", { text: session.activity_age, title: "time since last activity" })
      : null,
    session.branch ? el("span", { class: "sep", text: "·" }) : null,
    session.branch ? el("span", { text: session.branch, title: "git branch" }) : null,
    pending
      ? el("span", {
          class: "decision-flag",
          title: session.pending_decisions.map((decision) => decision.question).join("\n"),
          text: pending === 1 ? "needs you" : `needs you ×${pending}`,
        })
      : null,
    session.archived ? el("span", { class: "archived-flag", text: "archived" }) : null,
  ]);

  const row = el(
    "a",
    {
      class: "session-row" + (session.key === activeKey ? " active" : ""),
      href: sessionHash(session.key),
      dataset: { key: session.key },
      title: prettyPath(session.cwd, home),
    },
    [
      el("span", { class: `dot ${session.status}`, "aria-hidden": "true" }),
      el("span", { class: "session-row-body" }, [
        el("span", { class: "session-row-title", text: session.title }),
        sub,
      ]),
    ],
  );
  return row;
}

/** Per-row archive, so tidying the list does not mean opening every session first. It is a
    sibling of the row link rather than a child: a button inside an anchor is not valid HTML
    and swallows the tap on touch. */
function archiveButton(session) {
  const label = session.archived ? "Restore session" : "Archive session";
  return el("button", {
    class: "row-archive",
    type: "button",
    title: label,
    "aria-label": `${label}: ${session.title}`,
    text: session.archived ? "Restore" : "Archive",
    onclick: (event) => {
      event.preventDefault();
      toggleArchive(session, event.currentTarget);
    },
  });
}

async function toggleArchive(session, button) {
  button.disabled = true;
  try {
    await archiveSession(session.key);
    await refresh();
    toast(session.archived ? "Session restored." : "Session archived.");
  } catch (error) {
    button.disabled = false;
    const reason = error instanceof ApiError && error.code === "unavailable"
      ? "this server build does not support it yet."
      : error.message;
    toast(`Could not archive the session: ${reason}`, "error");
  }
}
