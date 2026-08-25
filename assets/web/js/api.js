// REST layer: credential bootstrap plus a thin fetch wrapper.
//
// The server serves `index.html` as a catch-all fallback, so an endpoint that has
// not shipped yet answers with `200 text/html` rather than `404`. Every JSON call
// therefore checks the content type and raises `ApiError("unavailable")` so the UI
// can degrade instead of choking on `<!doctype html>`.
//
// Two credential modes, and the server decides which -- `GET /api/health` is public
// precisely so the page can ask before it has one. With HTTP Basic the browser owns
// the prompt and this module must stay out of its way: no `Authorization` header of
// our own (it would replace the cached credentials and suppress the native dialog),
// and no token overlay on top of it.

const STORAGE_KEY = "agent-console-token";

export class ApiError extends Error {
  constructor(code, message, status = 0) {
    super(message);
    this.name = "ApiError";
    this.code = code; // "unavailable" | "http" | "network" | "unauthorized"
    this.status = status;
  }
}

let token = null;
let authMode = "token";
let unauthorizedHandler = () => {};

/** `"basic"` or `"token"`. Meaningful only after `loadAuthMode()` has resolved. */
export function getAuthMode() {
  return authMode;
}

/**
 * Asks the server which credential it wants.
 *
 * Deliberately a bare `fetch`: `/api/health` is the one unauthenticated JSON route,
 * and sending a stale token to it would be pointless. A server too old to report a
 * mode, or one that cannot be reached, leaves the historical token path in place.
 */
export async function loadAuthMode() {
  try {
    const response = await fetch("/api/health", { headers: { Accept: "application/json" } });
    if (!response.ok) return authMode;
    const data = await response.json();
    if (data && data.auth === "basic") authMode = "basic";
  } catch (error) {
    // Offline shell start-up: leave the default and let the first real call report it.
  }
  return authMode;
}

/** Reads `?token=` once, persists it, and strips it from the visible URL. */
export function bootstrapToken() {
  const url = new URL(window.location.href);
  const fromUrl = url.searchParams.get("token");
  if (fromUrl) {
    window.localStorage.setItem(STORAGE_KEY, fromUrl);
    url.searchParams.delete("token");
    const query = url.searchParams.toString();
    window.history.replaceState({}, "", url.pathname + (query ? `?${query}` : "") + url.hash);
  }
  token = window.localStorage.getItem(STORAGE_KEY);
  return token;
}

export function getToken() {
  return token;
}

export function setToken(value) {
  token = value;
  window.localStorage.setItem(STORAGE_KEY, value);
}

function clearToken() {
  token = null;
  window.localStorage.removeItem(STORAGE_KEY);
}

export function onUnauthorized(handler) {
  unauthorizedHandler = handler;
}

async function request(path, options = {}) {
  const headers = Object.assign({}, options.headers);
  if (authMode !== "basic") headers.Authorization = `Bearer ${token}`;
  let response;
  try {
    response = await fetch(path, Object.assign({}, options, { headers }));
  } catch (error) {
    throw new ApiError("network", error.message || "network error");
  }
  if (response.status === 401 || response.status === 403) {
    if (authMode === "basic") {
      // The 401 carried a `WWW-Authenticate: Basic` challenge, so the browser has
      // already offered its own prompt and this one was dismissed or answered wrong.
      // Opening the token overlay here would be a second, useless login box.
      throw new ApiError("unauthorized", "authentication required", response.status);
    }
    clearToken();
    unauthorizedHandler();
    throw new ApiError("unauthorized", "invalid or missing token", response.status);
  }
  return response;
}

/** GETs JSON, treating the HTML app-shell fallback as "endpoint not implemented". */
async function getJson(path) {
  const response = await request(path);
  return readJson(response, path);
}

async function sendJson(path, method, body) {
  const response = await request(path, {
    method,
    headers: body === undefined ? {} : { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  return readJson(response, path, { allowEmpty: true });
}

async function del(path) {
  const response = await request(path, { method: "DELETE" });
  if (!response.ok) throw new ApiError("http", await errorText(response), response.status);
  return null;
}

async function readJson(response, path, { allowEmpty = false } = {}) {
  const contentType = response.headers.get("content-type") || "";
  if (!contentType.includes("json")) {
    if (response.ok && allowEmpty && response.status === 204) return null;
    // Either the app-shell fallback or a plain-text error body.
    const body = (await response.text()).trim();
    if (response.ok && body.startsWith("<")) {
      throw new ApiError("unavailable", `${path} is not implemented by this server build`, response.status);
    }
    if (!response.ok) throw new ApiError("http", body || `request failed (${response.status})`, response.status);
    return null;
  }
  const data = await response.json();
  if (!response.ok) {
    throw new ApiError("http", (data && (data.error || data.message)) || `request failed (${response.status})`, response.status);
  }
  return data;
}

async function errorText(response) {
  if (response.ok) return "";
  const body = (await response.text()).trim();
  return body.startsWith("<") ? `request failed (${response.status})` : body || `request failed (${response.status})`;
}

// ------------------------------------------------------------------ routes

export function fetchSessions(query = "") {
  const trimmed = String(query || "").trim();
  return getJson(`/api/sessions${trimmed ? `?q=${encodeURIComponent(trimmed)}` : ""}`);
}

export function fetchMessages(key, { after = null, before = null, limit = 200 } = {}) {
  const params = new URLSearchParams();
  if (after !== null && after !== undefined && after !== "") params.set("after", after);
  if (before !== null && before !== undefined && before !== "") params.set("before", before);
  if (limit) params.set("limit", String(limit));
  const query = params.toString();
  return getJson(`/api/sessions/${encodeURIComponent(key)}/messages${query ? `?${query}` : ""}`);
}

export function sendPrompt(key, text) {
  return sendJson(`/api/sessions/${encodeURIComponent(key)}/prompt`, "POST", { text });
}

export function interruptSession(key) {
  return sendJson(`/api/sessions/${encodeURIComponent(key)}/interrupt`, "POST");
}

export function completePath(path) {
  return getJson(`/api/fs/complete?path=${encodeURIComponent(path)}`);
}

export function createSession(agent, cwd) {
  return sendJson("/api/sessions", "POST", { agent, cwd });
}

export function archiveSession(key) {
  return sendJson(`/api/sessions/${encodeURIComponent(key)}/archive`, "POST");
}

export function deleteSession(key) {
  return del(`/api/sessions/${encodeURIComponent(key)}`);
}

/**
 * The session's shells -- login shells in its working directory, not the agent's own PTY.
 *
 * They live in the PTY daemon rather than in the web server, so this list also contains the
 * shells a TUI opened for the same session.
 */
export function fetchShells(key) {
  return getJson(`/api/sessions/${encodeURIComponent(key)}/shells`);
}

export function createShell(key) {
  return sendJson(`/api/sessions/${encodeURIComponent(key)}/shells`, "POST");
}

export function deleteShell(key, id) {
  return del(`/api/sessions/${encodeURIComponent(key)}/shells/${encodeURIComponent(id)}`);
}

/**
 * Whether the agent is currently blocked on a numbered menu.
 *
 * Blocking dialogs (Claude Code's "trust this folder", a tool permission request) never reach
 * the transcript and never reach the hook-driven `pending_decisions`, so this screen read is
 * the only way the conversation view can see one.
 */
export function fetchPromptStatus(key) {
  return getJson(`/api/sessions/${encodeURIComponent(key)}/prompt-status`);
}

/** Chooses one of the blocking menu's options. 409 means the dialog is no longer up. */
export function answerPrompt(key, option) {
  return sendJson(`/api/sessions/${encodeURIComponent(key)}/answer`, "POST", { option });
}

// ------------------------------------------------------- dashboard capability

/** The alert queue. `GET` is pure: reading it never clears anything. */
export function fetchNotifications() {
  return getJson("/api/notifications");
}

export function markNotificationRead(id) {
  return sendJson(`/api/notifications/${encodeURIComponent(id)}/read`, "POST");
}

export function markAllNotificationsRead() {
  return sendJson("/api/notifications/read-all", "POST");
}

/** Renames a session. `null` clears the alias and restores the derived title. */
export function setSessionAlias(key, alias) {
  return sendJson(`/api/sessions/${encodeURIComponent(key)}/alias`, "PUT", { alias });
}

export function retrySummary(key) {
  return sendJson(`/api/sessions/${encodeURIComponent(key)}/summary/retry`, "POST");
}

/**
 * Claims this session's input lease for the web server.
 *
 * A denial is a 200 carrying the holder, not an error, so `force: false` is a safe way to ask
 * *who* is holding the session before offering to evict them.
 */
export function acquireLease(key, force = false) {
  return sendJson(`/api/sessions/${encodeURIComponent(key)}/lease`, "POST", { force });
}

/** A shell's recent output as plain text. 409 means it has printed nothing yet. */
export function fetchShellCapture(key, id) {
  return getJson(
    `/api/sessions/${encodeURIComponent(key)}/shells/${encodeURIComponent(id)}/capture`,
  );
}

/** Pastes that same output into the agent's composer, without submitting a turn. */
export function stageShellCapture(key, id) {
  return sendJson(
    `/api/sessions/${encodeURIComponent(key)}/shells/${encodeURIComponent(id)}/stage`,
    "POST",
  );
}

/** Runs every diagnostic probe. Slow (several seconds) -- never poll this. */
export function fetchDoctor() {
  return getJson("/api/doctor");
}
