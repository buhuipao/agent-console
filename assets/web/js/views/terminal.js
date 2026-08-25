// Agent TUI tab: the agent's own PTY -- the Codex / Claude Code interface itself.
//
// This is the escape hatch for what the conversation view cannot do: answering a blocking
// dialog ("trust this folder", a tool permission prompt) that never reaches the transcript.
// Anything typed here is typed *at the agent*. For a command line in the session's working
// directory, see the Shell tab (`shell.js`).

import { byId, toast } from "../dom.js";
import { offerTakeover } from "../lease.js";
import { getSession } from "../store.js";
import { createTerminalView } from "./termview.js";

let view = null;
let current = null;

export function initTerminal() {
  view = createTerminalView({
    view: byId("terminal-view"),
    container: byId("terminal-container"),
    toolbar: byId("terminal-toolbar"),
    // A terminated agent is gone for good; reconnecting would only spawn a fresh one behind
    // the user's back.
    shouldReconnect: () => {
      const session = getSession(current);
      return !(session && session.managed_alive === false);
    },
    // Typing at an agent a TUI is already attached to is refused by the daemon. Without this
    // the keystrokes simply disappeared; now the same takeover the composer offers is here.
    onLeaseDenied: () => offerTakeover(current),
  });
}

export function openTerminal(key) {
  current = key;
  view
    .open({
      id: `agent:${key}`,
      path: `/ws/sessions/${encodeURIComponent(key)}`,
    })
    .catch(() => {
      toast("Could not load the terminal library.", "error");
    });
}

export function closeTerminal() {
  current = null;
  view.close();
}

export function resizeTerminal() {
  view.resize();
}
