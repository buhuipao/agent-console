// The overview board: every session's state, read without opening any of them.
//
// The session list next to it answers "where do I go"; this answers "who needs me, and what
// is everything else doing". They are different questions, which is why the board is not the
// list with more lines in it -- it is ordered by *demand on the reader*, not by workspace:
//
//   Needs you   a blocking question, a waiting turn, or a failed run
//   Working     the agent is mid-turn; the card says what it is doing right now
//   Idle        nothing is happening; one dense line each, so twenty of them still fit
//
// That ordering is the whole point on a phone. Opening each session in turn to discover
// which one is blocked is exactly the thing a small screen makes expensive, and the fields
// that answer it -- summary.task, summary.current_action, summary.next_step,
// summary.blockers, pending_decisions -- are already in the list payload.

import { byId, clear, el } from "../dom.js";
import { sessionHash } from "../router.js";
import { getState, subscribe, visibleWorkspaces } from "../store.js";

/** Two blockers is the point where a card stops being scannable. */
const MAX_BLOCKERS = 2;
const MAX_DECISIONS = 2;

const nodes = {};
let signature = null;
let activeKey = null;

export function initOverview() {
  nodes.board = byId("overview-board");
  nodes.empty = byId("overview-empty");
  subscribe(render);
  render(getState());
}

/** Mirrors the list's active-row highlight, so the board and the list agree on where you are. */
export function setOverviewActive(key) {
  activeKey = key;
  if (!nodes.board) return;
  for (const card of nodes.board.querySelectorAll("[data-key]")) {
    card.classList.toggle("active", card.dataset.key === key);
  }
}

// ------------------------------------------------------------------ grouping

/** A session needs the reader when it is blocked on an answer, waiting, or has failed. */
function needsYou(session) {
  return (
    (session.pending_decisions || []).length > 0 ||
    session.status === "waiting" ||
    session.status === "failed"
  );
}

/**
 * Flattens the workspace grouping the list uses and regroups by demand.
 *
 * The workspace each session belongs to is not lost -- it rides along on the card as a
 * chip -- but it stops being the top-level axis, because "which repo is this in" is not
 * the question this surface exists to answer.
 */
function buckets(workspaces) {
  const flat = [];
  for (const workspace of workspaces) {
    for (const session of workspace.sessions) flat.push({ session, workspace: workspace.name });
  }
  return {
    attention: flat.filter((entry) => needsYou(entry.session)).sort(byUrgency),
    working: flat.filter((entry) => !needsYou(entry.session) && entry.session.status === "working"),
    idle: flat.filter((entry) => !needsYou(entry.session) && entry.session.status !== "working"),
  };
}

/** A question someone is standing at the keyboard for outranks a run that failed days ago. */
function byUrgency(left, right) {
  return rank(left.session) - rank(right.session);
}

function rank(session) {
  if ((session.pending_decisions || []).length > 0) return 0;
  if (session.status === "waiting") return 1;
  return 2;
}

// ----------------------------------------------------------------- rendering

function render(state) {
  if (!nodes.board) return;
  const grouped = buckets(visibleWorkspaces());
  const next = describe(grouped, state.error, state.loaded);
  // Polling runs every few seconds; rebuilding an unchanged board would reset its scroll
  // position and cancel a tap already in flight.
  if (next === signature) return;
  signature = next;

  clear(nodes.board);
  const total = grouped.attention.length + grouped.working.length + grouped.idle.length;

  if (state.error) {
    nodes.board.append(
      el("p", { class: "banner error", text: `Could not load sessions: ${state.error.message}` }),
    );
  }

  if (grouped.attention.length) {
    nodes.board.append(
      section("attention", "Needs you", grouped.attention.length, grouped.attention.map(attentionCard)),
    );
  }
  if (grouped.working.length) {
    nodes.board.append(
      section("working", "Working", grouped.working.length, grouped.working.map(workingCard)),
    );
  }
  if (grouped.idle.length) {
    nodes.board.append(section("idle", "Idle", grouped.idle.length, grouped.idle.map(idleRow)));
  }

  nodes.empty.hidden = total !== 0 || Boolean(state.error);
  nodes.empty.textContent = state.loaded
    ? "No session matches this filter."
    : "Loading sessions…";
  setOverviewActive(activeKey);
}

/** Cheap change detector: every field the board actually draws. */
function describe(grouped, error, loaded) {
  const shape = (entry) => [
    entry.session.key,
    entry.session.status,
    entry.session.activity_age,
    entry.session.title,
    entry.workspace,
    (entry.session.pending_decisions || []).map((decision) => decision.question),
    entry.session.summary ? entry.session.summary.task : "",
    entry.session.summary ? entry.session.summary.current_action : "",
    entry.session.summary ? entry.session.summary.next_step : "",
    entry.session.summary ? entry.session.summary.blockers : [],
  ];
  return JSON.stringify([
    error ? error.message : null,
    loaded,
    grouped.attention.map(shape),
    grouped.working.map(shape),
    grouped.idle.map(shape),
  ]);
}

function section(kind, label, count, children) {
  return el("section", { class: `board-section ${kind}` }, [
    el("h2", { class: "board-section-head" }, [
      el("span", { class: "board-section-title", text: label }),
      el("span", { class: "board-section-count tabular", text: String(count) }),
    ]),
    el("div", { class: `board-items ${kind}` }, children),
  ]);
}

/** The headline of any card: what this session is for, falling back to its own title. */
function headline(session) {
  const task = session.summary && session.summary.task ? session.summary.task.trim() : "";
  return task || session.title;
}

function metaRow(session, workspace) {
  return el("div", { class: "card-meta" }, [
    el("span", { class: `dot ${session.status}`, "aria-hidden": "true" }),
    el("span", { class: `status-word ${session.status}`, text: session.status }),
    el("span", { class: `agent-badge ${session.agent}`, text: session.agent }),
    workspace ? el("span", { class: "card-workspace", text: workspace, title: workspace }) : null,
    session.activity_age
      ? el("span", {
          class: "card-age tabular",
          text: session.activity_age,
          title: "time since last activity",
        })
      : null,
    session.archived ? el("span", { class: "archived-flag", text: "archived" }) : null,
  ]);
}

/** A line of the shape `Label  value`, the board's one repeated primitive. */
function field(label, value, kind = "") {
  return el("p", { class: `card-field ${kind}` }, [
    el("span", { class: "card-field-label", text: label }),
    el("span", { class: "card-field-value", text: value }),
  ]);
}

function attentionCard({ session, workspace }) {
  const decisions = (session.pending_decisions || []).slice(0, MAX_DECISIONS);
  const extraDecisions = (session.pending_decisions || []).length - decisions.length;
  const blockers = (session.summary && session.summary.blockers) || [];

  return card(session, workspace, [
    ...decisions.map((decision) =>
      field("Asks", decision.question || "The agent is waiting for an answer.", "ask"),
    ),
    extraDecisions > 0
      ? el("p", { class: "card-more", text: `+${extraDecisions} more question${extraDecisions === 1 ? "" : "s"}` })
      : null,
    // A failed session has no question to show; what it does have is why it stopped.
    !decisions.length && session.summary && session.summary.current_action
      ? field("Now", session.summary.current_action, "now")
      : null,
    ...blockerFields(blockers),
    session.summary && session.summary.next_step && !decisions.length
      ? field("Next", session.summary.next_step, "next")
      : null,
  ]);
}

function workingCard({ session, workspace }) {
  const summary = session.summary || {};
  const blockers = summary.blockers || [];
  return card(session, workspace, [
    summary.current_action ? field("Now", summary.current_action, "now") : null,
    summary.next_step ? field("Next", summary.next_step, "next") : null,
    ...blockerFields(blockers),
  ]);
}

function blockerFields(blockers) {
  return blockers
    .slice(0, MAX_BLOCKERS)
    .map((blocker) => field("Blocked", blocker, "blocked"));
}

function card(session, workspace, fields) {
  return el(
    "a",
    {
      class: `board-card ${session.status}${session.key === activeKey ? " active" : ""}`,
      href: sessionHash(session.key),
      dataset: { key: session.key },
    },
    [
      metaRow(session, workspace),
      el("p", { class: "card-task", text: headline(session) }),
      ...fields.filter(Boolean),
    ],
  );
}

/** Idle sessions are the long tail: one line each, or twenty of them bury the two that matter. */
function idleRow({ session, workspace }) {
  return el(
    "a",
    {
      class: `board-row${session.key === activeKey ? " active" : ""}`,
      href: sessionHash(session.key),
      dataset: { key: session.key },
      title: headline(session),
    },
    [
      el("span", { class: `dot ${session.status}`, "aria-hidden": "true" }),
      el("span", { class: "board-row-body" }, [
        el("span", { class: "board-row-task", text: headline(session) }),
        el("span", { class: "board-row-meta" }, [
          el("span", { class: `status-word ${session.status}`, text: session.status }),
          workspace ? el("span", { class: "card-workspace", text: workspace }) : null,
          session.activity_age ? el("span", { class: "card-age tabular", text: session.activity_age }) : null,
        ]),
      ]),
    ],
  );
}
