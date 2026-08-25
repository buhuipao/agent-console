// The alert queue as this device sees it: poll, remember what has already been shown, and
// raise a real system notification for anything new.
//
// The server's `read` flag is shared -- a TUI clearing it clears it for everyone -- so it is
// the wrong thing to decide "has this phone been told yet". This module keeps its own set of
// ids in `localStorage` for that, which is also what survives a reload without replaying the
// whole backlog as a burst of notifications.
//
// Raising a system notification is the one thing the TUI cannot do at all, and the reason
// this app is worth installing: a phone in a pocket can say "a session needs you" without the
// page being open. Permission is therefore never requested on load -- only from the explicit
// toggle in the alerts panel, which is a user gesture -- and everything degrades to a silent
// no-op when the API is missing or the user said no.

import { fetchNotifications, markAllNotificationsRead, markNotificationRead } from "./api.js";

const POLL_MS = 6000;
/* Hidden is the case that matters most here: the point of a system notification is to reach
   someone who is not looking. Backgrounded timers are throttled by the browser anyway, so
   this is a floor rather than a promise. */
const HIDDEN_POLL_MS = 15000;
const SEEN_KEY = "agent-console-seen-alerts";
const OPT_IN_KEY = "agent-console-desktop-alerts";
/** Ids kept across reloads. Larger than the server's 100-entry cap, so nothing is re-announced. */
const SEEN_LIMIT = 300;

const listeners = new Set();

const state = {
  /** Oldest-first, exactly as the server sends it. The view orders for humans. */
  entries: [],
  unread: 0,
  loaded: false,
  unavailable: false,
  error: null,
  seen: new Set(),
  /** Set by the first poll that actually returned a queue; see `announce`. */
  seeded: false,
  /** Whether the user asked this device to raise system notifications. */
  optIn: false,
  timer: null,
  polling: false,
};

export function getNotificationState() {
  return state;
}

export function subscribeNotifications(handler) {
  listeners.add(handler);
  return () => listeners.delete(handler);
}

function emit() {
  for (const handler of listeners) handler(state);
}

// ---------------------------------------------------------------- lifecycle

export function initNotifications() {
  state.seen = loadSeen();
  state.optIn = window.localStorage.getItem(OPT_IN_KEY) === "1";
}

export function startNotificationPolling() {
  stopNotificationPolling();
  const tick = async () => {
    await refreshNotifications();
    if (state.timer === null) return;
    state.timer = window.setTimeout(tick, delay());
  };
  // Immediately, not after the first interval: the badge is the only thing on the dashboard
  // that says a session needs you, and waiting a poll to draw it means the page loads
  // claiming there is nothing.
  state.timer = window.setTimeout(tick, 0);
}

function stopNotificationPolling() {
  if (state.timer !== null) window.clearTimeout(state.timer);
  state.timer = null;
}

function delay() {
  return document.visibilityState === "visible" ? POLL_MS : HIDDEN_POLL_MS;
}

export async function refreshNotifications() {
  if (state.polling || state.unavailable) return;
  state.polling = true;
  try {
    const payload = await fetchNotifications();
    const entries = (payload?.notifications || []).map(normalise).filter(Boolean);
    state.entries = entries;
    state.unread = Number(payload?.unread) || 0;
    state.loaded = true;
    state.error = null;
    announce(entries);
  } catch (error) {
    if (error && error.code === "unavailable") {
      // An older server build has no alert queue; stop asking rather than logging every poll.
      state.unavailable = true;
      stopNotificationPolling();
    }
    state.error = error;
    state.loaded = true;
  } finally {
    state.polling = false;
    emit();
  }
}

function normalise(raw) {
  if (!raw || !raw.id) return null;
  return {
    id: String(raw.id),
    key: raw.session_key || "",
    title: raw.session_title || raw.session_key || "session",
    status: raw.status === "failed" ? "failed" : "waiting",
    message: String(raw.message || "").trim(),
    createdAt: Number(raw.created_at) || 0,
    read: Boolean(raw.read),
  };
}

// ------------------------------------------------------------- mark as read

/** Marks one alert read on the server. A 404 (aged out of the queue) is not worth surfacing. */
export async function readNotification(id) {
  try {
    const payload = await markNotificationRead(id);
    applyUnread(payload, id);
  } catch (error) {
    // Nothing the reader can act on: the badge corrects itself on the next poll.
  }
  emit();
}

export async function readAllNotifications() {
  try {
    const payload = await markAllNotificationsRead();
    for (const entry of state.entries) entry.read = true;
    state.unread = Number(payload?.unread) || 0;
  } catch (error) {
    return false;
  }
  emit();
  return true;
}

function applyUnread(payload, id) {
  const entry = state.entries.find((item) => item.id === id);
  if (entry) entry.read = true;
  if (payload && typeof payload.unread === "number") state.unread = payload.unread;
}

// --------------------------------------------------- system notifications

export function notificationsSupported() {
  return typeof window.Notification === "function";
}

export function notificationPermission() {
  return notificationsSupported() ? window.Notification.permission : "unsupported";
}

/** True only when this device is both allowed and opted in. */
export function desktopAlertsEnabled() {
  return state.optIn && notificationPermission() === "granted";
}

/**
 * Turns system notifications on for this device, asking for permission if needed.
 *
 * Must be called from a user gesture: Chrome ignores -- and Safari rejects -- a permission
 * prompt that is not one. Resolves to the permission that ended up in force.
 */
export async function enableDesktopAlerts() {
  if (!notificationsSupported()) return "unsupported";
  let permission = window.Notification.permission;
  if (permission === "default") {
    try {
      permission = await window.Notification.requestPermission();
    } catch (error) {
      permission = window.Notification.permission;
    }
  }
  setOptIn(permission === "granted");
  return permission;
}

export function disableDesktopAlerts() {
  setOptIn(false);
}

function setOptIn(value) {
  state.optIn = value;
  window.localStorage.setItem(OPT_IN_KEY, value ? "1" : "0");
  emit();
}

/**
 * Raises a notification for every entry this device has not seen before.
 *
 * The first successful poll only records what is already there. Opening the app must not
 * fire a notification per queued alert -- the queue holds up to a hundred, and every one of
 * them is by definition older than the tab that would be announcing it.
 */
function announce(entries) {
  // Tracked separately from `loaded`, which a *failed* first poll also sets: reusing it there
  // would make the first poll that did succeed treat the whole backlog as newly arrived.
  const seeding = !state.seeded;
  state.seeded = true;
  const fresh = entries.filter((entry) => !state.seen.has(entry.id));
  if (!fresh.length) return;
  for (const entry of fresh) state.seen.add(entry.id);
  persistSeen();
  if (seeding || !desktopAlertsEnabled()) return;
  for (const entry of fresh) {
    if (!entry.read) raise(entry);
  }
}

/**
 * Shows one system notification.
 *
 * Through the service worker registration where there is one: constructing `Notification`
 * directly throws on Android Chrome, which is exactly the installed-PWA case this feature
 * exists for. Failures are swallowed -- a notification that could not be shown is not
 * something to interrupt the page about.
 */
function raise(entry) {
  const title = entry.status === "failed" ? `Session failed: ${entry.title}` : `Needs you: ${entry.title}`;
  const options = {
    body: entry.message || (entry.status === "failed" ? "The session failed." : "The agent is waiting for you."),
    tag: entry.id,
    data: { hash: `#/s/${encodeURIComponent(entry.key)}` },
    icon: "/icons/icon-192.png",
    badge: "/icons/icon-192.png",
  };
  const viaWorker = navigator.serviceWorker && navigator.serviceWorker.ready;
  if (viaWorker) {
    viaWorker
      .then((registration) => registration.showNotification(title, options))
      .catch(() => fallbackRaise(title, options));
    return;
  }
  fallbackRaise(title, options);
}

function fallbackRaise(title, options) {
  try {
    const notification = new window.Notification(title, options);
    notification.onclick = () => {
      window.focus();
      window.location.hash = options.data.hash;
      notification.close();
    };
  } catch (error) {
    // No service worker and no constructible Notification: nothing more to try.
  }
}

// -------------------------------------------------------------- seen record

function loadSeen() {
  try {
    const raw = JSON.parse(window.localStorage.getItem(SEEN_KEY) || "[]");
    return new Set(Array.isArray(raw) ? raw.map(String) : []);
  } catch (error) {
    return new Set();
  }
}

function persistSeen() {
  const ids = [...state.seen].slice(-SEEN_LIMIT);
  state.seen = new Set(ids);
  try {
    window.localStorage.setItem(SEEN_KEY, JSON.stringify(ids));
  } catch (error) {
    // A full quota is not worth breaking the poll over; worst case an alert repeats.
  }
}
