// Hash routing: `#/`, `#/doctor`, `#/s/<key>`, `#/s/<key>/shell[/<id>]`, `#/s/<key>/agent`.
//
// Everything the UI shows is derivable from the hash, so a reload restores the
// same screen -- including *which* shell was open -- and the browser Back button
// walks the same path the user took.

const listeners = new Set();

export function sessionHash(key) {
  return `#/s/${encodeURIComponent(key)}`;
}

/** The Shell tab. Without an id the view resolves one and rewrites the hash to name it. */
export function shellHash(key, id = null) {
  const base = `${sessionHash(key)}/shell`;
  return id ? `${base}/${encodeURIComponent(id)}` : base;
}

export function agentHash(key) {
  return `${sessionHash(key)}/agent`;
}

export const DOCTOR_HASH = "#/doctor";

function parseHash(hash) {
  const path = String(hash || "").replace(/^#/, "");
  const parts = path.split("/").filter(Boolean);
  if (parts[0] === "doctor") return { name: "doctor", key: null, shell: null };
  if (parts[0] === "s" && parts[1]) {
    const key = safeDecode(parts[1]);
    // `terminal` was this tab's name before the Shell tab existed, when it was the only
    // terminal in the UI. Kept so a bookmark or an open tab from then still resolves.
    if (parts[2] === "agent" || parts[2] === "terminal") return { name: "agent", key, shell: null };
    if (parts[2] === "shell") {
      return { name: "shell", key, shell: parts[3] ? safeDecode(parts[3]) : null };
    }
    return { name: "session", key, shell: null };
  }
  return { name: "dashboard", key: null, shell: null };
}

function safeDecode(value) {
  try {
    return decodeURIComponent(value);
  } catch (error) {
    return value;
  }
}

export function currentRoute() {
  return parseHash(window.location.hash);
}

export function navigate(hash, { replace = false } = {}) {
  if (window.location.hash === hash) return;
  if (replace) window.history.replaceState({}, "", hash || "#/");
  else window.location.hash = hash || "#/";
  if (replace) emit();
}

export function onRoute(handler) {
  listeners.add(handler);
}

function emit() {
  const route = currentRoute();
  for (const handler of listeners) handler(route);
}

export function startRouter() {
  window.addEventListener("hashchange", emit);
  if (!window.location.hash) window.history.replaceState({}, "", "#/");
  emit();
}
