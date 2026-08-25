// Renaming a session.
//
// The field is prefilled with the session's `alias` -- the explicit name -- and never with
// `title`, which is usually the derived one (the first prompt, or the directory). Prefilling
// the derived title would turn every rename dialog someone opened and confirmed into a
// permanent alias pinning whatever the title happened to be that day.
//
// The alias is written through the same store the TUI reads, so a rename here retitles the
// session in a running TUI too.

import { setSessionAlias } from "../api.js";
import { byId, toast } from "../dom.js";
import { getSession, refresh } from "../store.js";

const nodes = {};
let currentKey = null;

export function initRenameDialog() {
  nodes.overlay = byId("rename-overlay");
  nodes.input = byId("rename-input");
  nodes.hint = byId("rename-hint");
  nodes.error = byId("rename-error");
  nodes.save = byId("rename-save");
  nodes.clear = byId("rename-clear");
  nodes.cancel = byId("rename-cancel");

  nodes.save.addEventListener("click", () => submit(nodes.input.value));
  nodes.clear.addEventListener("click", () => submit(""));
  nodes.cancel.addEventListener("click", close);
  nodes.overlay.addEventListener("click", (event) => {
    if (event.target === nodes.overlay) close();
  });
  nodes.input.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      submit(nodes.input.value);
    }
  });
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && !nodes.overlay.hidden) close();
  });
}

export function openRenameDialog(key) {
  const session = getSession(key);
  if (!session) return;
  currentKey = key;
  nodes.input.value = session.alias || "";
  nodes.hint.textContent = session.alias
    ? "Clearing the name restores the title Agent Console derives from the session."
    : `Without a name this session is titled “${session.title}”.`;
  nodes.clear.hidden = !session.alias;
  nodes.error.hidden = true;
  nodes.overlay.hidden = false;
  nodes.input.focus();
  nodes.input.select();
}

function close() {
  nodes.overlay.hidden = true;
  currentKey = null;
}

async function submit(value) {
  const key = currentKey;
  if (!key) return;
  // An empty field means "no alias", which is how the alias is cleared -- not an empty name.
  const alias = value.trim() === "" ? null : value.trim();
  nodes.save.disabled = true;
  nodes.clear.disabled = true;
  try {
    await setSessionAlias(key, alias);
    close();
    await refresh();
    toast(alias ? `Renamed to “${alias}”.` : "Name cleared.");
  } catch (error) {
    nodes.error.textContent = `Could not rename: ${error.message}`;
    nodes.error.hidden = false;
  } finally {
    nodes.save.disabled = false;
    nodes.clear.disabled = false;
  }
}
