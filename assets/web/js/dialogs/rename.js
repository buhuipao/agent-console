// Renaming a session.
//
// The field opens on the title the list is showing -- the session's `alias` when it has one,
// otherwise the derived title (the first prompt, or the directory). Renaming is nearly always
// editing a long derived title down to something readable, and an empty field makes that
// retyping. Confirming the prefill unchanged does pin that title as an explicit name, which
// is what asking to rename and then keeping the name means; Clear puts it back to derived.
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
  nodes.input.value = session.alias || session.title || "";
  nodes.hint.textContent = session.alias
    ? "Clearing the name restores the title Agent Console derives from the session."
    : "Saving keeps this as the session's name; clearing it later restores the derived title.";
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
