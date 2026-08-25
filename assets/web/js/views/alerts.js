// The alerts inbox: a header button with an unread count, and a panel of the alerts behind it.
//
// The TUI treats an alert as a keypress -- "jump to the next one", which selects the session
// and consumes the entry in the same motion. That does not survive being moved to a server
// several clients poll, and it is not what a phone wants either: the useful question there is
// "what needs me?", answered by a list you can scan, not a cursor you advance blindly.
//
// So this is an inbox. Newest first (the API sends oldest-first, which is the order the TUI's
// "next" key walks, not the order a person reads). Tapping an entry goes to the session and
// marks it read; "Mark all read" clears the badge the TUI shares.

import { byId, clear, el, relativeTime, toast } from "../dom.js";
import { navigate, sessionHash } from "../router.js";
import {
  desktopAlertsEnabled,
  disableDesktopAlerts,
  enableDesktopAlerts,
  getNotificationState,
  notificationPermission,
  notificationsSupported,
  readAllNotifications,
  readNotification,
  refreshNotifications,
  subscribeNotifications,
} from "../notifications.js";

const nodes = {};
let open = false;

export function initAlerts() {
  nodes.button = byId("alerts-btn");
  nodes.badge = byId("alerts-badge");
  nodes.panel = byId("alerts-panel");
  nodes.list = byId("alerts-list");
  nodes.empty = byId("alerts-empty");
  nodes.readAll = byId("alerts-read-all");
  nodes.notify = byId("alerts-notify");
  nodes.notifyRow = byId("alerts-notify-row");
  nodes.notifyHint = byId("alerts-notify-hint");

  nodes.button.addEventListener("click", (event) => {
    event.stopPropagation();
    togglePanel();
  });
  nodes.readAll.addEventListener("click", onReadAll);
  nodes.notify.addEventListener("change", onNotifyToggle);
  nodes.panel.addEventListener("click", (event) => event.stopPropagation());
  document.addEventListener("click", () => closeAlerts());
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape") closeAlerts();
  });

  subscribeNotifications(render);
  render(getNotificationState());
}

export function closeAlerts() {
  if (!open) return;
  open = false;
  nodes.panel.hidden = true;
  nodes.button.setAttribute("aria-expanded", "false");
}

function togglePanel() {
  open = !open;
  nodes.panel.hidden = !open;
  nodes.button.setAttribute("aria-expanded", String(open));
  // The badge is polled every few seconds; opening the panel should show *now*, not the
  // state from up to a poll ago.
  if (open) refreshNotifications();
}

// ----------------------------------------------------------------- rendering

function render(state) {
  const unread = state.unread;
  nodes.badge.hidden = unread === 0;
  nodes.badge.textContent = unread > 99 ? "99+" : String(unread);
  nodes.button.classList.toggle("has-unread", unread > 0);
  nodes.button.setAttribute(
    "aria-label",
    unread ? `Alerts: ${unread} unread` : "Alerts: nothing unread",
  );
  nodes.readAll.disabled = unread === 0;
  renderNotifyRow();
  if (!open) return;
  renderList(state);
}

function renderList(state) {
  clear(nodes.list);
  if (state.unavailable) {
    nodes.empty.hidden = false;
    nodes.empty.textContent = "This server build has no alert queue yet.";
    return;
  }
  // Newest first: the API orders oldest-first for the TUI's "walk to the next one" key, and
  // an inbox that opened on the oldest alert would bury the one that just fired.
  const entries = [...state.entries].reverse();
  nodes.empty.hidden = entries.length > 0;
  nodes.empty.textContent = state.loaded
    ? "No alerts. A session that starts waiting or fails shows up here."
    : "Loading alerts…";
  for (const entry of entries) nodes.list.append(alertRow(entry));
}

function alertRow(entry) {
  return el(
    "a",
    {
      class: `alert-row${entry.read ? " read" : ""}`,
      href: sessionHash(entry.key),
      dataset: { id: entry.id },
      onclick: (event) => {
        event.preventDefault();
        openAlert(entry);
      },
    },
    [
      el("span", { class: `dot ${entry.status}`, "aria-hidden": "true" }),
      el("span", { class: "alert-body" }, [
        el("span", { class: "alert-title", text: entry.title }),
        el("span", { class: "alert-message", text: entry.message || statusText(entry.status) }),
        el("span", { class: "alert-meta" }, [
          el("span", { class: `status-word ${entry.status}`, text: entry.status }),
          el("span", { class: "sep", text: "·" }),
          el("span", { text: relativeTime(entry.createdAt) }),
          entry.read ? el("span", { class: "sep", text: "·" }) : null,
          entry.read ? el("span", { text: "read" }) : null,
        ]),
      ]),
    ],
  );
}

function statusText(status) {
  return status === "failed" ? "The session failed." : "The agent is waiting for you.";
}

function openAlert(entry) {
  closeAlerts();
  navigate(sessionHash(entry.key));
  readNotification(entry.id);
}

async function onReadAll() {
  const ok = await readAllNotifications();
  toast(ok ? "All alerts marked read." : "Could not mark the alerts read.", ok ? "info" : "error");
}

// ------------------------------------------------------ system notifications

/**
 * The opt-in, and an honest account of why it may not be available.
 *
 * Permission is requested from this checkbox and nowhere else: a prompt on page load is the
 * thing browsers penalise and users refuse permanently, and a refusal here is silent -- the
 * app is fully usable without it.
 */
function renderNotifyRow() {
  const permission = notificationPermission();
  if (!notificationsSupported()) {
    nodes.notifyRow.hidden = true;
    nodes.notifyHint.textContent = "This browser cannot show system notifications.";
    nodes.notifyHint.hidden = false;
    return;
  }
  nodes.notifyRow.hidden = false;
  nodes.notify.checked = desktopAlertsEnabled();
  nodes.notify.disabled = permission === "denied";
  if (permission === "denied") {
    nodes.notifyHint.textContent = "Notifications are blocked for this site in your browser settings.";
    nodes.notifyHint.hidden = false;
    return;
  }
  nodes.notifyHint.hidden = true;
}

async function onNotifyToggle() {
  if (!nodes.notify.checked) {
    disableDesktopAlerts();
    return;
  }
  const permission = await enableDesktopAlerts();
  if (permission !== "granted") {
    nodes.notify.checked = false;
    toast(
      permission === "denied"
        ? "Your browser blocked notifications for this site."
        : "Notifications were not allowed.",
    );
  }
  renderNotifyRow();
}
