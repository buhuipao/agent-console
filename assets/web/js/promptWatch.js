// Watching the open session for a blocking dialog, and answering it.
//
// A provider stops on a numbered menu -- "trust this folder", a tool permission request --
// and waits for a keypress. Nothing about that reaches the transcript, and nothing about it
// reaches the hook-driven `pending_decisions` either (a session can sit blocked for minutes
// with `pending_decisions: []`), so `GET /prompt-status` is the only thing in the API that
// can see it. Without this watcher the conversation view offers a phone user no way out of a
// blocked session except the Agent TUI tab, which is the experience this UI exists to replace.

import { ApiError, answerPrompt, fetchPromptStatus } from "./api.js";
import { getSession } from "./store.js";

/* Faster than the session list (4s) and close to the message stream (2s), because this is
   what the reader is waiting on: a blocked agent emits no messages, so while it matters this
   is the only request in flight. The backend warns a dialog can come and go on its own, so
   the cadence is set by how long a dialog may be missed, not by how long it may be shown. */
const POLL_MS = 3000;
/* A pocketed phone still has to notice a dialog when it is picked up -- and `visibilitychange`
   forces a poll then anyway -- so the hidden cadence only has to keep the answer roughly
   fresh, at roughly the rate the session list uses. */
const HIDDEN_POLL_MS = 20000;
/* After answering, the agent needs a moment to repaint before the screen read stops showing
   the dialog. Re-reading instantly would flash the card back on for one cycle. */
const SETTLE_MS = 900;

const state = {
  key: null,
  prompt: null,
  timer: null,
  polling: false,
  /** Set once a prompt has been sent: the pty is certainly up, so the liveness gate is moot. */
  forced: false,
  unavailable: false,
  onChange: () => {},
};

/** Starts watching `key`; `onChange` fires whenever the blocking prompt appears or changes. */
export function watchPrompt(key, onChange) {
  stopWatchingPrompt();
  state.key = key;
  state.prompt = null;
  state.forced = false;
  state.unavailable = false;
  state.onChange = onChange || (() => {});
  schedule(0);
}

export function stopWatchingPrompt() {
  window.clearTimeout(state.timer);
  state.timer = null;
  state.key = null;
  state.prompt = null;
  state.forced = false;
  state.onChange = () => {};
}

/** The dialog the agent is sitting on, or `null`. */
export function getBlockingPrompt() {
  return state.prompt;
}

/**
 * Marks the session as certainly running.
 *
 * Sending a prompt starts the agent if it was not started, so from that moment the liveness
 * gate below would only delay the poll that matters most -- the one during a send that is
 * blocking precisely because a dialog is up.
 */
export function armPromptWatch() {
  state.forced = true;
}

export function pollPromptNow() {
  schedule(0);
}

/**
 * Answers the dialog by option number.
 *
 * Resolves to `{ ok, gone, locked, message }` rather than throwing: the dialog can vanish
 * between the poll that drew the buttons and the tap, and a 409 for that is a stale screen,
 * not a fault. `locked` is the 423 another surface holding the input answers with, which the
 * caller turns into a takeover rather than an error message.
 */
export async function answerBlockingPrompt(option) {
  const key = state.key;
  if (!key) return { ok: false, gone: true, locked: false, message: "no session open" };
  try {
    await answerPrompt(key, option);
    setPrompt(null);
    schedule(SETTLE_MS);
    return { ok: true, gone: false, locked: false, message: "" };
  } catch (error) {
    const gone = error instanceof ApiError && error.status === 409;
    const locked = error instanceof ApiError && error.status === 423;
    if (gone) setPrompt(null);
    schedule(SETTLE_MS);
    return { ok: false, gone, locked, message: error.message };
  }
}

// ------------------------------------------------------------------- polling

function schedule(delay) {
  window.clearTimeout(state.timer);
  if (!state.key || state.unavailable) return;
  state.timer = window.setTimeout(tick, delay);
}

function nextDelay() {
  return document.visibilityState === "visible" ? POLL_MS : HIDDEN_POLL_MS;
}

/**
 * Reading the screen *starts* the agent when it is not already running, so a conversation
 * opened only to be read must not be polled. `managed_alive` is exactly "this session has a
 * pty right now", which is also the only state in which a dialog can exist.
 *
 * A send starts the agent too, and the session list can be a poll behind, so a send arms a
 * temporary override. It is spent as soon as the list agrees: left standing, an agent that
 * later exits would be restarted by nothing more than a tab someone forgot to close.
 */
function isPollable(key) {
  const session = getSession(key);
  const alive = Boolean(session && session.managed_alive);
  if (alive) state.forced = false;
  return alive || state.forced;
}

async function tick() {
  if (!state.key || state.polling) return;
  const key = state.key;
  if (!isPollable(key)) {
    schedule(nextDelay());
    return;
  }
  state.polling = true;
  try {
    const payload = await fetchPromptStatus(key);
    if (state.key !== key) return;
    setPrompt(normalise(payload));
  } catch (error) {
    if (state.key !== key) return;
    if (error instanceof ApiError && error.code === "unavailable") {
      // An older server build has no such endpoint; the decision card falls back to whatever
      // `pending_decisions` reports and nothing here retries.
      state.unavailable = true;
      return;
    }
    // A transient failure leaves the last known answer on screen and waits for the next tick.
  } finally {
    state.polling = false;
    if (state.key === key) schedule(nextDelay());
  }
}

function setPrompt(prompt) {
  if (signature(prompt) === signature(state.prompt)) return;
  state.prompt = prompt;
  state.onChange(prompt);
}

function signature(prompt) {
  return prompt ? JSON.stringify(prompt) : "";
}

/** Keeps only a prompt this view can actually act on: a question plus numbered options. */
function normalise(payload) {
  const prompt = payload && payload.prompt;
  if (!prompt) return null;
  const options = (Array.isArray(prompt.options) ? prompt.options : [])
    .map((option) => ({
      number: Number(option && option.number),
      label: String((option && option.label) || "").trim(),
    }))
    .filter((option) => Number.isInteger(option.number) && option.label !== "");
  if (!options.length) return null;
  return { question: String(prompt.question || "").trim(), options };
}
