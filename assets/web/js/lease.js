// Taking over a session another surface is driving.
//
// The PTY daemon hands one writer at a time an input lease. A TUI on the same machine, or a
// second browser, holds it and this server's writes are refused -- `423 Locked` on the REST
// routes, a `lease_denied` control frame on the websockets. Before this existed a phone user
// simply typed into the void.
//
// The TUI's answer is a takeover key inside its full-screen attach loop, which the web never
// enters. The web-native shape is this: name the conflict (which process, which pid), then
// offer one button that forces the lease and retries what the user was doing. It is a
// deliberate modal -- evicting another surface's input is not something to do on a stray tap.

import { ApiError, acquireLease } from "./api.js";
import { byId, el } from "./dom.js";

const nodes = {};
let resolveCurrent = null;

export function initTakeoverDialog() {
  nodes.overlay = byId("takeover-overlay");
  nodes.detail = byId("takeover-detail");
  nodes.holder = byId("takeover-holder");
  nodes.error = byId("takeover-error");
  nodes.confirm = byId("takeover-confirm");
  nodes.cancel = byId("takeover-cancel");

  nodes.cancel.addEventListener("click", () => finish(false));
  nodes.confirm.addEventListener("click", force);
  nodes.overlay.addEventListener("click", (event) => {
    if (event.target === nodes.overlay) finish(false);
  });
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && !nodes.overlay.hidden) finish(false);
  });
}

/** True for the one failure a takeover fixes. */
export function isLocked(error) {
  return error instanceof ApiError && error.status === 423;
}

/**
 * Opens the takeover dialog for `key`.
 *
 * Resolves `true` when this server ends up holding the lease -- either because the user
 * forced it, or because the holder had already let go by the time we asked -- which is the
 * caller's cue to retry whatever was refused. Resolves `false` if the user backed out.
 */
export function offerTakeover(key, { detail = "" } = {}) {
  if (!key) return Promise.resolve(false);
  if (resolveCurrent) finish(false);

  nodes.detail.textContent =
    detail || "Another surface has this session open and holds its input, so nothing you type here reaches the agent.";
  nodes.error.hidden = true;
  nodes.error.textContent = "";
  nodes.confirm.disabled = true;
  nodes.holder.replaceChildren(el("span", { class: "muted-line", text: "Checking who holds it…" }));
  nodes.overlay.hidden = false;
  nodes.confirm.focus();

  const promise = new Promise((resolve) => {
    resolveCurrent = resolve;
  });
  probe(key);
  return promise;
}

/**
 * Asks who holds the lease without disturbing them.
 *
 * `force: false` is a legitimate question with an informative answer, so a denial arrives as
 * a 200 carrying the holder rather than as an error. If it comes back granted the conflict
 * resolved itself while the user was reading, and there is nothing left to confirm.
 */
async function probe(key) {
  nodes.overlay.dataset.key = key;
  try {
    const payload = await acquireLease(key, false);
    if (nodes.overlay.dataset.key !== key) return;
    if (payload && payload.granted) {
      finish(true);
      return;
    }
    nodes.holder.replaceChildren(...holderLines(payload && payload.holder));
  } catch (error) {
    if (nodes.overlay.dataset.key !== key) return;
    nodes.holder.replaceChildren(
      el("span", { class: "muted-line", text: "Could not work out who holds it." }),
    );
    showError(error.message);
  } finally {
    if (nodes.overlay.dataset.key === key) nodes.confirm.disabled = false;
  }
}

function holderLines(holder) {
  if (!holder) return [el("span", { class: "muted-line", text: "The holder did not identify itself." })];
  return [
    el("span", { class: "holder-label", text: "Held by" }),
    el("span", { class: "holder-value", text: `pid ${holder.pid}` }),
    holder.instance_id
      ? el("span", { class: "holder-value muted-line", text: String(holder.instance_id).slice(0, 8) })
      : null,
  ].filter(Boolean);
}

async function force() {
  const key = nodes.overlay.dataset.key;
  if (!key) return;
  nodes.confirm.disabled = true;
  nodes.error.hidden = true;
  try {
    const payload = await acquireLease(key, true);
    if (payload && payload.granted) {
      finish(true);
      return;
    }
    showError("The lease was not granted. The other surface may have just reclaimed it.");
  } catch (error) {
    showError(error.message);
  } finally {
    nodes.confirm.disabled = false;
  }
}

function showError(message) {
  nodes.error.textContent = message;
  nodes.error.hidden = false;
}

function finish(granted) {
  nodes.overlay.hidden = true;
  delete nodes.overlay.dataset.key;
  const resolve = resolveCurrent;
  resolveCurrent = null;
  if (resolve) resolve(granted);
}
