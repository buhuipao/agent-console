// Copying text out of the app, on an origin where the clipboard API may not exist.
//
// This console is normally served over plain HTTP on a LAN address, which is not a secure
// context, and `navigator.clipboard` is simply undefined there. So the copy path is a ladder:
// the async clipboard API, then the legacy `execCommand` on a scratch textarea (which does
// work on http), and finally a dialog that puts the text on screen pre-selected so it can be
// copied by hand. The last rung matters most on a phone, where the whole point of the button
// is that selecting terminal text by hand is miserable.

import { byId, el } from "./dom.js";

/** Resolves to `true` if the text is on the clipboard, `false` if the caller must show it. */
export async function copyText(text) {
  if (!text) return false;
  if (navigator.clipboard && window.isSecureContext) {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch (error) {
      // Permission refused or a non-secure context that still exposed the object; fall through.
    }
  }
  return legacyCopy(text);
}

function legacyCopy(text) {
  const area = el("textarea", {
    value: text,
    readOnly: true,
    "aria-hidden": "true",
    style: "position:fixed;top:-1000px;left:0;opacity:0",
  });
  document.body.append(area);
  let ok = false;
  try {
    area.select();
    area.setSelectionRange(0, text.length);
    ok = document.execCommand("copy");
  } catch (error) {
    ok = false;
  } finally {
    area.remove();
  }
  return ok;
}

/** The last rung: the text, selected, for a manual copy. */
export function showCopyFallback(text, { title = "Copy this text" } = {}) {
  const overlay = byId("capture-overlay");
  const field = byId("capture-text");
  const heading = byId("capture-title");
  heading.textContent = title;
  field.value = text;
  overlay.hidden = false;
  field.focus();
  field.select();
}

export function initCopyFallback() {
  const overlay = byId("capture-overlay");
  const close = () => {
    overlay.hidden = true;
  };
  byId("capture-close").addEventListener("click", close);
  overlay.addEventListener("click", (event) => {
    if (event.target === overlay) close();
  });
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && !overlay.hidden) close();
  });
}
