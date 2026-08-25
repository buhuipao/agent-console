// Tiny DOM helpers. Everything in this app builds nodes and sets `textContent`
// instead of assigning `innerHTML`, so agent output can never inject markup.

export function el(tag, props = {}, children = []) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (value === null || value === undefined || value === false) continue;
    if (key === "class") node.className = value;
    else if (key === "text") node.textContent = value;
    else if (key === "dataset") Object.assign(node.dataset, value);
    else if (key.startsWith("on") && typeof value === "function") {
      node.addEventListener(key.slice(2).toLowerCase(), value);
    } else if (key in node && key !== "list" && typeof value !== "object") {
      node[key] = value;
    } else {
      node.setAttribute(key, value === true ? "" : value);
    }
  }
  for (const child of [].concat(children)) {
    if (child === null || child === undefined || child === false) continue;
    node.append(child);
  }
  return node;
}

export function clear(node) {
  while (node.firstChild) node.removeChild(node.firstChild);
}

export function byId(id) {
  const node = document.getElementById(id);
  if (!node) throw new Error(`missing element #${id}`);
  return node;
}

/** Collapses a long single-line label for a collapsed summary row. */
export function oneLine(value, max = 240) {
  const text = String(value ?? "").replace(/\s+/g, " ").trim();
  return text.length > max ? `${text.slice(0, max - 1)}…` : text;
}

/** Shortens `/Users/me/github/foo` to `~/github/foo` when it is under $HOME. */
export function prettyPath(path, home) {
  if (!path) return "";
  if (home && path.startsWith(home)) return `~${path.slice(home.length)}`;
  return path;
}

/** Guesses $HOME from the session paths so long absolute paths can show as `~/…`. */
export function inferHome(paths) {
  for (const path of paths) {
    const match = /^(\/(?:Users|home)\/[^/]+)(?:\/|$)/.exec(path || "");
    if (match) return match[1];
  }
  return null;
}

export function basename(path) {
  if (!path) return "";
  const parts = String(path).split("/").filter(Boolean);
  return parts.length ? parts[parts.length - 1] : path;
}

let toastTimer = null;

export function toast(message, kind = "info") {
  const node = document.getElementById("toast");
  if (!node) return;
  node.textContent = message;
  node.className = kind === "error" ? "toast error" : "toast";
  node.hidden = false;
  window.clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => {
    node.hidden = true;
  }, kind === "error" ? 6000 : 3000);
}

/** "just now" / "4m ago" for a unix-seconds timestamp, so an alert list reads at a glance. */
export function relativeTime(seconds) {
  const value = Number(seconds);
  if (!Number.isFinite(value) || value <= 0) return "";
  const delta = Math.max(0, Math.round(Date.now() / 1000 - value));
  if (delta < 45) return "just now";
  if (delta < 3600) return `${Math.round(delta / 60)}m ago`;
  if (delta < 86400) return `${Math.round(delta / 3600)}h ago`;
  return `${Math.round(delta / 86400)}d ago`;
}
