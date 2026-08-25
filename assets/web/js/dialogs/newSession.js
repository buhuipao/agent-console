// New-session dialog with debounced working-directory completion, mirroring the
// path completion the TUI offers.

import { ApiError, completePath, createSession } from "../api.js";
import { byId, clear, el, toast } from "../dom.js";
import { refresh } from "../store.js";

const DEBOUNCE_MS = 160;

const nodes = {};
const state = {
  entries: [],
  active: -1,
  timer: null,
  requestId: 0,
  supported: true,
  onCreated: () => {},
};

export function initNewSessionDialog({ onCreated } = {}) {
  state.onCreated = onCreated || (() => {});
  nodes.overlay = byId("new-session-overlay");
  nodes.input = byId("new-session-cwd");
  nodes.list = byId("cwd-suggestions");
  nodes.error = byId("new-session-error");
  nodes.create = byId("new-session-create");
  nodes.cancel = byId("new-session-cancel");

  byId("new-session-btn").addEventListener("click", open);
  nodes.cancel.addEventListener("click", close);
  nodes.create.addEventListener("click", submit);
  nodes.input.addEventListener("input", onInput);
  nodes.input.addEventListener("keydown", onKeydown);
  nodes.overlay.addEventListener("mousedown", (event) => {
    if (event.target === nodes.overlay) close();
  });
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && !nodes.overlay.hidden) close();
  });
}

export function open() {
  nodes.error.hidden = true;
  nodes.overlay.hidden = false;
  hideSuggestions();
  nodes.input.focus();
  if (nodes.input.value) requestCompletions(nodes.input.value);
}

function close() {
  nodes.overlay.hidden = true;
  hideSuggestions();
  window.clearTimeout(state.timer);
}

function onInput() {
  window.clearTimeout(state.timer);
  const value = nodes.input.value;
  state.timer = window.setTimeout(() => requestCompletions(value), DEBOUNCE_MS);
}

async function requestCompletions(value) {
  if (!state.supported) return;
  const requestId = ++state.requestId;
  try {
    const payload = await completePath(value || "");
    if (requestId !== state.requestId) return;
    const entries = Array.isArray(payload?.entries)
      ? payload.entries
      : Array.isArray(payload)
        ? payload
        : [];
    showSuggestions(entries.filter((entry) => typeof entry === "string"));
  } catch (error) {
    if (error instanceof ApiError && error.code === "unavailable") {
      // Older server build: degrade to a plain text field.
      state.supported = false;
    }
    hideSuggestions();
  }
}

function showSuggestions(entries) {
  state.entries = entries;
  state.active = -1;
  clear(nodes.list);
  if (!entries.length) {
    hideSuggestions();
    return;
  }
  entries.forEach((entry, index) => {
    const item = el("li", {
      class: "suggestion",
      role: "option",
      id: `cwd-option-${index}`,
      text: entry,
    });
    item.addEventListener("mousedown", (event) => {
      event.preventDefault();
      accept(index);
    });
    nodes.list.append(item);
  });
  nodes.list.hidden = false;
  nodes.input.setAttribute("aria-expanded", "true");
}

function hideSuggestions() {
  nodes.list.hidden = true;
  nodes.input.setAttribute("aria-expanded", "false");
  nodes.input.removeAttribute("aria-activedescendant");
  state.entries = [];
  state.active = -1;
}

function setActive(index) {
  const items = [...nodes.list.children];
  items.forEach((item, position) => item.classList.toggle("active", position === index));
  state.active = index;
  if (index >= 0 && items[index]) {
    items[index].scrollIntoView({ block: "nearest" });
    nodes.input.setAttribute("aria-activedescendant", `cwd-option-${index}`);
  }
}

function accept(index) {
  const entry = state.entries[index];
  if (!entry) return;
  nodes.input.value = entry;
  nodes.input.focus();
  // Accepting a directory immediately offers its children, so drilling down works
  // the same way tab-completion does in a shell.
  requestCompletions(entry);
}

function onKeydown(event) {
  if (nodes.list.hidden) {
    if (event.key === "Enter") {
      event.preventDefault();
      submit();
    }
    return;
  }
  if (event.key === "ArrowDown") {
    event.preventDefault();
    setActive((state.active + 1) % state.entries.length);
  } else if (event.key === "ArrowUp") {
    event.preventDefault();
    setActive((state.active - 1 + state.entries.length) % state.entries.length);
  } else if (event.key === "Tab" && state.entries.length) {
    event.preventDefault();
    accept(state.active >= 0 ? state.active : 0);
  } else if (event.key === "Enter") {
    event.preventDefault();
    if (state.active >= 0) accept(state.active);
    else submit();
  } else if (event.key === "Escape") {
    event.preventDefault();
    hideSuggestions();
  }
}

async function submit() {
  const agentInput = document.querySelector('input[name="agent"]:checked');
  const agent = agentInput ? agentInput.value : "codex";
  const cwd = nodes.input.value.trim();
  if (!cwd) {
    showError("Enter a working directory.");
    return;
  }
  nodes.create.disabled = true;
  try {
    const session = await createSession(agent, cwd);
    close();
    await refresh();
    toast("Session created.");
    if (session && session.key) state.onCreated(session.key);
  } catch (error) {
    showError(error.message || "Could not create the session.");
  } finally {
    nodes.create.disabled = false;
  }
}

function showError(message) {
  nodes.error.textContent = message;
  nodes.error.hidden = false;
}
