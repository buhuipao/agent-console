// Conversation view: message stream, banners, and the prompt composer.

import { ApiError, fetchMessages, interruptSession, retrySummary, sendPrompt } from "../api.js";
import { byId, clear, el, oneLine, toast } from "../dom.js";
import { isLocked, offerTakeover } from "../lease.js";
import {
  answerBlockingPrompt,
  armPromptWatch,
  getBlockingPrompt,
  pollPromptNow,
  stopWatchingPrompt,
  watchPrompt,
} from "../promptWatch.js";
import { getSession } from "../store.js";
import { decisionsBanner, summaryCard, unavailableBanner } from "./banners.js";
import { renderMessage } from "./message.js";

const POLL_MS = 2000;
const POLL_FAILURES_BEFORE_NOTICE = 3;
/* A backgrounded tab is the normal way this app is used -- the phone is in a pocket while
   the agent works -- so polling slows down when the page is hidden instead of stopping.
   Stopping is what made a conversation that opened empty stay empty forever: the initial
   load has no visibility check, so only a manual reload ever showed the reply. */
const HIDDEN_POLL_MS = 10000;
const BOTTOM_SLACK = 80;
const MAX_CHASE = 3;
/** How long a prompt may sit at "sending…" before the bubble stops claiming to be in flight. */
const SEND_TIMEOUT_MS = 45000;
/** How long a delivered prompt may go unrecorded by the transcript before the bubble says so. */
const CONFIRM_TIMEOUT_MS = 90000;

const nodes = {};

const state = {
  key: null,
  cursor: null,
  seen: new Set(),
  sequence: 0,
  pending: [],
  timer: null,
  polling: false,
  unavailable: false,
  promptUnavailable: false,
  atBottom: true,
  unseen: 0,
  chrome: "",
  startCursor: null,
  hasMoreBefore: false,
  loadingEarlier: false,
};

export function initConversation() {
  nodes.view = byId("chat-view");
  nodes.banners = byId("chat-banners");
  nodes.messages = byId("messages");
  nodes.jump = byId("jump-latest");
  nodes.form = byId("composer");
  nodes.input = byId("composer-input");
  nodes.send = byId("send-btn");
  nodes.stop = byId("stop-btn");
  nodes.hint = byId("composer-hint");
  nodes.pendingWrap = el("div", { class: "pending-wrap" });

  nodes.messages.addEventListener("scroll", onScroll, { passive: true });
  nodes.jump.addEventListener("click", () => scrollToBottom(true));
  nodes.form.addEventListener("submit", (event) => {
    event.preventDefault();
    submitPrompt();
  });
  nodes.input.addEventListener("input", autoResize);
  nodes.input.addEventListener("keydown", onComposerKeydown);
  nodes.stop.addEventListener("click", stopAgent);

  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible" && state.key) {
      pollNow();
      pollPromptNow();
    }
  });

  nodes.hint.textContent = usesEnterToSend() ? "Enter sends · Shift+Enter adds a newline" : "";
}

/** Switches the conversation to `key`, resetting stream state. */
export function openConversation(key) {
  if (state.key === key) {
    refreshChrome();
    return;
  }
  closeConversation();
  state.key = key;
  state.cursor = null;
  state.seen = new Set();
  state.sequence = 0;
  for (const entry of state.pending) window.clearTimeout(entry.timer);
  state.pending = [];
  state.unavailable = false;
  state.promptUnavailable = false;
  state.atBottom = true;
  state.unseen = 0;
  state.chrome = "";
  state.startCursor = null;
  state.hasMoreBefore = false;
  state.loadingEarlier = false;

  clear(nodes.messages);
  clear(nodes.banners);
  // Optimistic bubbles belong to the session that was just left.
  clear(nodes.pendingWrap);
  nodes.messages.append(el("p", { class: "empty-hint", id: "messages-placeholder", text: "Loading conversation…" }));
  nodes.messages.append(nodes.pendingWrap);
  nodes.jump.hidden = true;
  setComposerEnabled(true);
  // A dialog is not part of the session record, so its arrival has to redraw the chrome on
  // its own rather than waiting for the next `/api/sessions` poll to change something.
  watchPrompt(key, () => refreshChrome());
  refreshChrome();
  loadInitial();
}

export function closeConversation() {
  window.clearTimeout(state.timer);
  state.timer = null;
  state.key = null;
  stopWatchingPrompt();
}

/** Re-renders banners / Stop button when the polled session record changes. */
export function refreshChrome() {
  if (!state.key) return;
  const session = getSession(state.key);
  if (!session) return;

  nodes.stop.hidden = session.status !== "working";
  // Two buttons plus a text field do not fit one phone-width row; the composer drops them
  // onto their own line instead of squeezing the prompt field down to a slot.
  nodes.form.classList.toggle("has-stop", !nodes.stop.hidden);
  nodes.input.placeholder = `Message ${session.agent}…`;

  const prompt = getBlockingPrompt();
  const signature = JSON.stringify({
    status: session.status,
    decisions: session.pending_decisions,
    prompt,
    summary: session.summary,
    unavailable: state.unavailable,
    promptUnavailable: state.promptUnavailable,
  });
  if (signature === state.chrome) return;
  state.chrome = signature;

  clear(nodes.banners);
  if (state.unavailable) nodes.banners.append(unavailableBanner(state.key, "messages"));
  else if (state.promptUnavailable) nodes.banners.append(unavailableBanner(state.key, "prompt"));
  if (session.pending_decisions.length || prompt) {
    nodes.banners.append(decisionsBanner(session, { prompt, onAnswer: answerOption }));
  }
  const summary = summaryCard(session, { onRetry: () => requestSummaryRetry(state.key) });
  if (summary) nodes.banners.append(summary);
}

/** Re-queues this session's summary. The work happens on the summary worker, so the answer
    only says it was queued -- the card fills in on a later poll. */
async function requestSummaryRetry(key) {
  if (!key) return;
  try {
    await retrySummary(key);
    toast("Summary queued — it will appear here once it is generated.");
  } catch (error) {
    const reason = error instanceof ApiError && error.code === "unavailable"
      ? "this server build has no summary retry yet."
      : error.message;
    toast(`Could not retry the summary: ${reason}`, "error");
  }
}

/**
 * Sends one option of the blocking dialog.
 *
 * A prompt that failed while the dialog was up is deliberately not re-sent here. The reader
 * was told it was not sent; silently sending it as a side effect of answering an unrelated
 * safety question is worse than asking for one more tap, so the failed bubble keeps its
 * Retry button and this only points at it.
 */
async function answerOption(option) {
  // The dialog is answered by the number it printed, never by the position of the button:
  // a provider is free to skip or reorder, and the digit is what reaches the pty.
  const result = await answerBlockingPrompt(option.number);
  if (result.ok) {
    const stuck = state.pending.some((entry) => entry.kind === "failed");
    toast(stuck ? "Answer sent — tap Retry to send your prompt." : "Answer sent.");
    window.setTimeout(pollNow, 800);
    return;
  }
  if (result.gone) {
    toast("That question is no longer on screen — the agent moved on.");
    return;
  }
  // A refused answer is not a broken one: another surface holds the input. Offer the takeover
  // and, once it is granted, press the same option again rather than making the user find it.
  if (result.locked) {
    const granted = await offerTakeover(state.key);
    if (granted) await answerOption(option);
    return;
  }
  toast(`Could not answer: ${oneLine(result.message, 160)}`, "error");
}

// ------------------------------------------------------------------ loading

async function loadInitial() {
  const key = state.key;
  try {
    const payload = await fetchMessages(key, {});
    if (state.key !== key) return;
    const { messages, cursor, startCursor, hasMoreBefore } = readPayload(payload);
    state.cursor = cursor;
    state.startCursor = startCursor;
    state.hasMoreBefore = hasMoreBefore;
    appendMessages(messages, { initial: true });
    renderEarlierControl();
    if (!messages.length) setPlaceholder("No messages in this session yet.");
    scrollToBottom(false);
    schedulePoll();
  } catch (error) {
    if (state.key !== key) return;
    handleStreamError(error);
  }
}

function handleStreamError(error) {
  if (error instanceof ApiError && error.code === "unavailable") {
    state.unavailable = true;
    state.promptUnavailable = true;
    setComposerEnabled(false);
    setPlaceholder("");
    state.chrome = "";
    refreshChrome();
    return;
  }
  setPlaceholder(`Could not load the conversation: ${error.message}`);
  schedulePoll();
}

/** Messages always arrive oldest -> newest, whichever direction was paged. */
function readPayload(payload) {
  return {
    messages: Array.isArray(payload?.messages) ? payload.messages : [],
    cursor: payload?.cursor ?? null,
    startCursor: payload?.start_cursor ?? null,
    hasMore: Boolean(payload?.has_more),
    hasMoreBefore: Boolean(payload?.has_more_before),
  };
}

/** Shows or hides the "load earlier" control above the oldest loaded message. */
function renderEarlierControl() {
  const existing = document.getElementById("load-earlier");
  if (!state.hasMoreBefore || state.startCursor === null) {
    if (existing) existing.remove();
    return;
  }
  if (existing) return;
  const button = el("button", {
    id: "load-earlier",
    class: "load-earlier",
    type: "button",
    text: "Load earlier messages",
    onclick: loadEarlier,
  });
  nodes.messages.prepend(button);
}

async function loadEarlier() {
  if (state.loadingEarlier || !state.startCursor) return;
  const key = state.key;
  const button = document.getElementById("load-earlier");
  state.loadingEarlier = true;
  if (button) {
    button.disabled = true;
    button.textContent = "Loading…";
  }
  try {
    const payload = await fetchMessages(key, { before: state.startCursor, limit: 100 });
    if (state.key !== key) return;
    const { messages, startCursor, hasMoreBefore } = readPayload(payload);
    state.startCursor = startCursor ?? state.startCursor;
    state.hasMoreBefore = messages.length > 0 && hasMoreBefore;
    prependMessages(messages);
  } catch (error) {
    toast(`Could not load earlier messages: ${error.message}`, "error");
  } finally {
    state.loadingEarlier = false;
    const node = document.getElementById("load-earlier");
    if (node) {
      node.disabled = false;
      node.textContent = "Load earlier messages";
    }
    renderEarlierControl();
  }
}

/** Inserts older messages above the current ones without moving the viewport. */
function prependMessages(messages) {
  const session = getSession(state.key);
  const agent = session ? session.agent : "";
  const anchor = nodes.messages.querySelector(".msg") || nodes.pendingWrap;
  const distanceFromBottom = nodes.messages.scrollHeight - nodes.messages.scrollTop;

  for (const message of messages) {
    const id = messageId(message);
    if (state.seen.has(id)) continue;
    state.seen.add(id);
    nodes.messages.insertBefore(renderMessage(message, { agent }), anchor);
  }
  nodes.messages.scrollTop = nodes.messages.scrollHeight - distanceFromBottom;
}

function schedulePoll() {
  window.clearTimeout(state.timer);
  if (state.unavailable || !state.key) return;
  const delay = document.visibilityState === "visible" ? POLL_MS : HIDDEN_POLL_MS;
  state.timer = window.setTimeout(poll, delay);
}

function pollNow() {
  window.clearTimeout(state.timer);
  if (!state.unavailable && state.key) poll();
}

async function poll(chase = 0) {
  if (!state.key || state.polling) return;
  state.polling = true;
  const key = state.key;
  try {
    const payload = await fetchMessages(key, { after: state.cursor });
    if (state.key !== key) return;
    const { messages, cursor, hasMore } = readPayload(payload);
    const added = appendMessages(messages, {});
    // Only the newer edge moves here, and only after the page has been rendered. An idle
    // `after=` poll answers with `start_cursor == cursor`, so copying it would repoint "load
    // earlier" at the bottom of the conversation; and advancing past messages a failed render
    // never showed would lose them for good, because `after=` does not offer them again.
    if (cursor !== null && cursor !== undefined) state.cursor = cursor;
    state.pollFailures = 0;
    if (added && hasMore && chase < MAX_CHASE) {
      state.polling = false;
      await poll(chase + 1);
      return;
    }
  } catch (error) {
    if (state.key !== key) return;
    if (error instanceof ApiError && error.code === "unavailable") {
      handleStreamError(error);
      return;
    }
    // A session terminated under the reader answers 404 forever. Retrying in silence left a
    // stale conversation on screen that looked live, so say what happened and stop polling;
    // anything else is treated as a blip and only surfaces once it stops recovering.
    if (error instanceof ApiError && error.status === 404) {
      state.pollFailures = 0;
      setPlaceholder("This session is gone -- it was terminated or removed.");
      setComposerEnabled(false);
      return;
    }
    state.pollFailures = (state.pollFailures || 0) + 1;
    if (state.pollFailures >= POLL_FAILURES_BEFORE_NOTICE) {
      setPlaceholder(`Lost contact with the console: ${error.message}. Still retrying...`);
    }
  } finally {
    if (state.polling) {
      state.polling = false;
      schedulePoll();
    }
  }
}

// ---------------------------------------------------------------- rendering

function setPlaceholder(text) {
  const existing = document.getElementById("messages-placeholder");
  if (!text) {
    if (existing) existing.remove();
    return;
  }
  if (existing) {
    existing.textContent = text;
    return;
  }
  nodes.messages.prepend(el("p", { class: "empty-hint", id: "messages-placeholder", text }));
}

function appendMessages(messages, { initial = false } = {}) {
  const session = getSession(state.key);
  const agent = session ? session.agent : "";
  let added = 0;

  for (const message of messages) {
    const id = messageId(message);
    if (state.seen.has(id)) continue;
    state.seen.add(id);
    nodes.messages.insertBefore(renderMessage(message, { agent }), nodes.pendingWrap);
    added += 1;
    dropMatchingPending(message);
  }

  if (added) {
    setPlaceholder("");
    if (state.atBottom) scrollToBottom(false);
    else if (!initial) {
      state.unseen += added;
      nodes.jump.hidden = false;
      nodes.jump.textContent = `↓ ${state.unseen} new message${state.unseen === 1 ? "" : "s"}`;
    }
  }
  return added;
}

function messageId(message) {
  if (message.id !== undefined && message.id !== null && message.id !== "") return String(message.id);
  state.sequence += 1;
  return `local-${message.role || "?"}-${message.ts || ""}-${state.sequence}`;
}

function messageText(message) {
  const blocks = Array.isArray(message.blocks) ? message.blocks : [];
  const text = blocks
    .filter((block) => block && block.type === "text")
    .map((block) => block.text || "")
    .join("\n");
  return (text || message.text || "").trim();
}

/** The transcript is the source of truth: once it carries the turn, the optimistic copy goes.
    A bubble the server rejected can never be this turn, so resending the same text after a
    failure resolves the resend and leaves the failure on screen where it belongs. */
function dropMatchingPending(message) {
  if (message.role !== "user" || !state.pending.length) return;
  const body = messageText(message);
  const entry = state.pending.find((item) => item.kind !== "failed" && item.text === body);
  if (entry) removePending(entry);
}

// ----------------------------------------------------------- pending bubbles

/**
 * An optimistic bubble for a prompt that has not reached the transcript yet.
 *
 * It is added before the request rather than after it: `POST /prompt` blocks until the agent
 * is actually ready to read input, which can be many seconds, and an unchanged screen during
 * that wait is indistinguishable from a prompt that was silently dropped.
 */
function addPendingBubble(text) {
  const node = renderMessage(
    { role: "user", ts: Date.now(), blocks: [{ type: "text", text }] },
    { pending: true },
  );
  const entry = { text, node, kind: "sending", timer: null };
  nodes.pendingWrap.append(node);
  state.pending.push(entry);
  // "sending…" is only honest while the request is in flight, so it is never the last word.
  entry.timer = window.setTimeout(
    () => setPendingState(entry, "stalled", "no answer from the server"),
    SEND_TIMEOUT_MS,
  );
  return entry;
}

const PENDING_LABELS = {
  sending: "sending…",
  sent: "sent",
  failed: "not sent",
  stalled: "unconfirmed",
};

function setPendingState(entry, kind, detail = "") {
  window.clearTimeout(entry.timer);
  entry.kind = kind;
  // The bubble may belong to a conversation the reader has already left.
  if (!entry.node.isConnected) return;
  // Anything that is no longer in flight takes its place in the transcript. Optimistic
  // bubbles live in a trailing wrapper so they stay below the newest message while they are
  // sending; a bubble left there after it stopped sending floats an 11:26 failure underneath
  // an 11:30 reply, which reads as if the failure happened last.
  if (kind !== "sending") pinToTranscript(entry);
  if (kind === "failed") attachRetry(entry);
  // "sent" only means the keystrokes reached the agent. A prompt that landed in the agent's
  // composer without being submitted looks exactly like a successful send from here, so the
  // bubble stops claiming success if the transcript never records the turn.
  if (kind === "sent") {
    entry.timer = window.setTimeout(
      () => setPendingState(entry, "stalled", "the agent has not recorded this turn"),
      CONFIRM_TIMEOUT_MS,
    );
  }
  const label = entry.node.querySelector(".msg-status");
  if (!label) return;
  const text = PENDING_LABELS[kind] || kind;
  label.textContent = detail ? `${text} · ${detail}` : text;
  label.className = `msg-status ${kind}`;
  entry.node.classList.toggle("unsent", kind === "failed");
  entry.node.classList.toggle("unconfirmed", kind === "stalled");
}

/** Moves a bubble out of the trailing wrapper and into the stream, at the point it was sent. */
function pinToTranscript(entry) {
  if (entry.node.parentNode === nodes.messages) return;
  nodes.messages.insertBefore(entry.node, nodes.pendingWrap);
}

/**
 * The way out of a refused send.
 *
 * A "not sent" bubble used to be the end of the road: the text was restored to the composer,
 * but nothing said so and nothing offered to send it again, so recovering meant noticing the
 * restored text and guessing when the agent might be ready. The button resends exactly the
 * text that failed, which is also the only text this bubble can honestly claim to be about.
 */
function attachRetry(entry) {
  if (entry.node.querySelector(".msg-retry")) return;
  entry.node.append(
    el("div", { class: "msg-actions" }, [
      el("button", {
        class: "btn msg-retry",
        type: "button",
        text: "Retry",
        title: "Send this prompt again",
        onclick: () => retryPending(entry),
      }),
    ]),
  );
}

function removePending(entry) {
  window.clearTimeout(entry.timer);
  entry.node.remove();
  const index = state.pending.indexOf(entry);
  if (index !== -1) state.pending.splice(index, 1);
}

// ------------------------------------------------------------------ scroll

function onScroll() {
  const distance = nodes.messages.scrollHeight - nodes.messages.scrollTop - nodes.messages.clientHeight;
  state.atBottom = distance < BOTTOM_SLACK;
  if (state.atBottom) {
    state.unseen = 0;
    nodes.jump.hidden = true;
  }
}

/** Keeps the newest message in view after a viewport change (keyboard open/close),
    but only when the reader was already at the bottom. */
export function keepPinned() {
  if (state.atBottom) scrollToBottom(false);
}

function scrollToBottom(smooth) {
  state.atBottom = true;
  state.unseen = 0;
  nodes.jump.hidden = true;
  nodes.messages.scrollTo({
    top: nodes.messages.scrollHeight,
    behavior: smooth ? "smooth" : "auto",
  });
}

// ---------------------------------------------------------------- composer

function usesEnterToSend() {
  return window.matchMedia("(hover: hover) and (pointer: fine)").matches;
}

function onComposerKeydown(event) {
  // Never hijack Return on a touch keyboard: there is a visible Send button there.
  if (event.key !== "Enter" || event.shiftKey || event.isComposing) return;
  if (!usesEnterToSend()) return;
  event.preventDefault();
  submitPrompt();
}

function autoResize() {
  nodes.input.style.height = "auto";
  const max = Math.round(window.innerHeight * 0.4);
  nodes.input.style.height = `${Math.min(nodes.input.scrollHeight, max)}px`;
}

function setComposerEnabled(enabled) {
  nodes.input.disabled = !enabled;
  nodes.send.disabled = !enabled;
}

function submitPrompt() {
  const text = nodes.input.value.trim();
  if (!state.key || !text) return;
  nodes.input.value = "";
  autoResize();
  send(text);
}

/** Resends the exact text a bubble failed with, replacing that bubble rather than stacking. */
function retryPending(entry) {
  const text = entry.text;
  removePending(entry);
  // The failure put this same text back in the composer. Clearing it there keeps the box from
  // holding a copy of something now in flight -- but only while it is untouched, since edits
  // made since then are the reader's, not ours to discard.
  if (nodes.input.value.trim() === text) {
    nodes.input.value = "";
    autoResize();
  }
  send(text);
}

async function send(text) {
  const key = state.key;
  if (!key || !text) return;

  const entry = addPendingBubble(text);
  scrollToBottom(true);
  setComposerEnabled(false);
  // `POST /prompt` blocks while the agent is not reading input, which is exactly what a
  // blocking dialog causes. Watching from here means the dialog and its answer buttons come
  // up *during* the wait, so the reader can clear the block before the send gives up.
  armPromptWatch();
  pollPromptNow();

  let failed = false;
  try {
    await sendPrompt(key, text);
    setPendingState(entry, "sent");
    window.setTimeout(pollNow, 400);
  } catch (error) {
    failed = true;
    setPendingState(
      entry,
      "failed",
      isLocked(error) ? "another surface holds this session" : oneLine(error.message, 120),
    );
    // The prompt never reached the agent, so it goes back in the box ready to send again.
    nodes.input.value = text;
    autoResize();
    if (isLocked(error)) {
      // Deliberately after the bubble is marked failed: if the takeover is declined, the
      // screen still explains why nothing was sent and still offers Retry.
      offerTakeover(key).then((granted) => {
        if (granted) retryPending(entry);
      });
    } else if (error instanceof ApiError && error.code === "unavailable") {
      state.promptUnavailable = true;
      state.chrome = "";
      refreshChrome();
      toast("This server build cannot accept prompts yet — use the Agent TUI tab.", "error");
    } else {
      toast(`Could not send: ${oneLine(error.message, 160)}`, "error");
    }
  } finally {
    // Whatever stopped the send is most often a dialog; read for one straight away.
    pollPromptNow();
    setComposerEnabled(!state.unavailable);
    // Focusing after a failure would raise the on-screen keyboard over the explanation and
    // over the answer buttons, which is the one moment the reader needs to see them.
    if (!state.unavailable && !failed) nodes.input.focus();
  }
}

async function stopAgent() {
  if (!state.key) return;
  const key = state.key;
  nodes.stop.disabled = true;
  try {
    await interruptSession(key);
    toast("Interrupt sent.");
  } catch (error) {
    if (isLocked(error)) {
      const granted = await offerTakeover(key);
      if (granted) {
        nodes.stop.disabled = false;
        await stopAgent();
        return;
      }
    } else {
      const message = error instanceof ApiError && error.code === "unavailable"
        ? "This server build has no interrupt endpoint yet."
        : `Could not interrupt: ${error.message}`;
      toast(message, "error");
    }
  } finally {
    nodes.stop.disabled = false;
  }
}
