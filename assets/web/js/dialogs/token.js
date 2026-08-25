// Access-token overlay. Matters most on a phone, where the tokenised URL shown in
// the dashboard header cannot simply be clicked.
//
// Only ever used when the server runs in token mode: with HTTP Basic the browser
// owns the credential prompt and this overlay is never wired up (see app.js).

import { bootstrapToken, setToken } from "../api.js";
import { byId } from "../dom.js";

let resolvePending = null;

export function initTokenDialog() {
  const overlay = byId("token-overlay");
  const input = byId("token-input");
  const error = byId("token-error");
  const save = byId("token-save");

  const submit = () => {
    const value = input.value.trim();
    if (!value) {
      error.textContent = "Enter the token shown in the dashboard header.";
      error.hidden = false;
      return;
    }
    setToken(value);
    overlay.hidden = true;
    error.hidden = true;
    input.value = "";
    const resolve = resolvePending;
    resolvePending = null;
    if (resolve) resolve(value);
    else window.location.reload();
  };

  save.addEventListener("click", submit);
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter") submit();
  });
}

export function promptForToken() {
  const overlay = byId("token-overlay");
  overlay.hidden = false;
  byId("token-input").focus();
  return new Promise((resolve) => {
    resolvePending = resolve;
  });
}

/** Resolves with a token, showing the overlay only when none is stored yet. */
export function ensureToken() {
  const existing = bootstrapToken();
  if (existing) return Promise.resolve(existing);
  return promptForToken();
}
