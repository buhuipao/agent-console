// Shell tab: real login shells running in the session's working directory.
//
// A shell here is a `$SHELL -l` in the session's cwd, not the agent -- `ls -al` lists the
// directory instead of becoming a prompt. The terminals live in the PTY daemon under
// `shell|<session key>|<id>`, so the ones listed here are the same terminals the TUI shows
// in its shell panes: opening one in either place makes it visible in the other.
//
// A session can have several. Which one is showing is part of the route
// (`#/s/<key>/shell/<id>`), so a reload comes back to the same shell.

import {
  ApiError,
  createShell,
  deleteShell,
  fetchShellCapture,
  fetchShells,
  stageShellCapture,
} from "../api.js";
import { copyText, showCopyFallback } from "../clipboard.js";
import { byId, clear, el, toast } from "../dom.js";
import { isLocked, offerTakeover } from "../lease.js";
import { agentHash, navigate, shellHash } from "../router.js";
import { createTerminalView } from "./termview.js";

const nodes = {};
let view = null;

const state = {
  key: null,
  id: null,
  attached: false,
  shells: [],
  busy: false,
  // Guards against a slow list landing after the user has moved on to another session.
  generation: 0,
};

export function initShell() {
  nodes.view = byId("shell-view");
  nodes.tabs = byId("shell-tabs");
  nodes.add = byId("shell-add");
  nodes.container = byId("shell-container");
  nodes.toolbar = byId("shell-toolbar");
  nodes.copy = byId("shell-copy");
  nodes.stage = byId("shell-stage");
  view = createTerminalView({
    view: nodes.view,
    container: nodes.container,
    toolbar: nodes.toolbar,
    onLeaseDenied: () => offerTakeover(state.key),
  });
  nodes.add.addEventListener("click", onAdd);
  nodes.tabs.addEventListener("click", onTabsClick);
  nodes.copy.addEventListener("click", copyOutput);
  nodes.stage.addEventListener("click", sendToAgent);
}

// -------------------------------------------------------------- shell output

/**
 * Both capture actions, sharing their failure vocabulary.
 *
 * Selecting terminal text by hand is unpleasant on a desktop and close to impossible on a
 * phone, which is why these two buttons are worth more here than the key bindings they come
 * from: on a phone this is the only practical way to get a command's output anywhere else.
 */
async function withCapture(button, action) {
  const { key, id } = state;
  if (!key || !id || state.busy) return;
  state.busy = true;
  button.disabled = true;
  try {
    await action(key, id);
  } catch (error) {
    if (isLocked(error)) {
      const granted = await offerTakeover(key);
      state.busy = false;
      button.disabled = false;
      if (granted) await withCapture(button, action);
      return;
    }
    toast(captureFailure(error), "error");
  } finally {
    state.busy = false;
    button.disabled = false;
  }
}

function captureFailure(error) {
  if (error instanceof ApiError && error.status === 409) {
    return "This shell has not printed anything yet.";
  }
  if (error instanceof ApiError && error.code === "unavailable") {
    return "This server build cannot read shell output yet.";
  }
  return `Could not read the shell output: ${error.message}`;
}

function copyOutput() {
  return withCapture(nodes.copy, async (key, id) => {
    const payload = await fetchShellCapture(key, id);
    const text = (payload && payload.text) || "";
    if (!text.trim()) {
      toast("This shell has not printed anything yet.");
      return;
    }
    // `navigator.clipboard` does not exist on a plain-HTTP LAN address, which is how this
    // console is usually reached, so a refusal here is expected rather than exceptional.
    if (await copyText(text)) {
      toast(`Copied ${text.length} characters of shell output.`);
      return;
    }
    showCopyFallback(text, { title: "Shell output" });
  });
}

/**
 * Pastes the output into the agent's own composer and goes to where it is visible.
 *
 * That is the Agent TUI tab, not the conversation: the text lands in the provider's input
 * line inside its PTY, so the conversation view -- which renders the transcript and its own
 * separate textarea -- would show no sign of it and look like the button did nothing.
 */
function sendToAgent() {
  return withCapture(nodes.stage, async (key, id) => {
    const payload = await stageShellCapture(key, id);
    const bytes = (payload && payload.bytes) || 0;
    navigate(agentHash(key));
    toast(`Pasted ${bytes} bytes into the agent's composer — review it, then press Enter to send.`);
  });
}

/**
 * Shows `requested` for `key`, or resolves one when the route names no shell: the session's
 * current shell if it still exists, otherwise the first, otherwise a newly spawned one so
 * the tab is usable the moment it is opened.
 */
export async function openShell(key, requested) {
  const generation = ++state.generation;
  if (state.key !== key) {
    view.close();
    state.key = key;
    state.id = null;
    state.attached = false;
    state.shells = [];
    renderTabs();
  }
  if (state.attached && requested && requested === state.id) {
    view.resize();
    return;
  }

  let shells;
  try {
    shells = await fetchShells(key);
    if (!shells.length) shells = [await createShell(key)];
  } catch (error) {
    toast(`Could not open a shell: ${error.message}`, "error");
    return;
  }
  if (generation !== state.generation) return;

  state.shells = shells;
  const preferred = requested || state.id;
  const picked = shells.some((shell) => shell.id === preferred) ? preferred : shells[0].id;
  if (!state.attached || picked !== state.id) attach(picked);
  else renderTabs();
  // Pin the resolved shell into the URL so a reload restores this shell, not just this tab.
  navigate(shellHash(key, picked), { replace: true });
}

export function closeShell() {
  state.attached = false;
  view.close();
}

function setCaptureEnabled(enabled) {
  if (!nodes.copy) return;
  nodes.copy.disabled = !enabled;
  nodes.stage.disabled = !enabled;
}

export function resizeShell() {
  view.resize();
}

function attach(id) {
  state.id = id;
  state.attached = true;
  renderTabs();
  setCaptureEnabled(true);
  view
    .open({
      id: `shell:${state.key}:${id}`,
      path: `/ws/sessions/${encodeURIComponent(state.key)}/shells/${encodeURIComponent(id)}`,
    })
    .catch(() => toast("Could not load the terminal library.", "error"));
}

function renderTabs() {
  clear(nodes.tabs);
  for (const shell of state.shells) {
    const active = shell.id === state.id;
    nodes.tabs.append(
      el("div", { class: `shell-tab${active ? " active" : ""}` }, [
        el("a", {
          class: "shell-tab-name",
          href: shellHash(state.key, shell.id),
          title: `Switch to ${shell.name}`,
          "aria-current": active ? "true" : null,
          text: shell.name,
        }),
        el("button", {
          type: "button",
          class: "shell-close",
          title: `Close ${shell.name}`,
          "aria-label": `Close ${shell.name}`,
          dataset: { close: shell.id },
          text: "×",
        }),
      ]),
    );
  }
}

async function onAdd() {
  const key = state.key;
  if (!key || state.busy) return;
  state.busy = true;
  try {
    const shell = await createShell(key);
    state.shells = [...state.shells, shell];
    navigate(shellHash(key, shell.id));
  } catch (error) {
    toast(`Could not open another shell: ${error.message}`, "error");
  } finally {
    state.busy = false;
  }
}

function onTabsClick(event) {
  const closer = event.target.closest("[data-close]");
  if (closer) {
    event.preventDefault();
    removeShell(closer.dataset.close);
  }
}

async function removeShell(id) {
  const key = state.key;
  if (!key || state.busy) return;
  state.busy = true;
  try {
    await deleteShell(key, id);
  } catch (error) {
    toast(`Could not close the shell: ${error.message}`, "error");
    return;
  } finally {
    state.busy = false;
  }
  state.shells = state.shells.filter((shell) => shell.id !== id);
  if (id !== state.id) {
    renderTabs();
    return;
  }
  // The shell being watched just went away. Fall back to another one, or to the bare tab
  // route, which spawns a fresh shell -- this tab is only useful with a shell in it.
  view.close();
  state.attached = false;
  state.id = null;
  setCaptureEnabled(false);
  navigate(state.shells.length ? shellHash(key, state.shells[0].id) : shellHash(key), {
    replace: true,
  });
}
