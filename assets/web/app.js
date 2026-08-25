// Entry point: wires the shell, routing, and the shared session chrome.
// The feature modules live under /js.

import { ApiError, archiveSession, deleteSession, loadAuthMode, onUnauthorized } from "./js/api.js";
import { initCopyFallback } from "./js/clipboard.js";
import { byId, el, inferHome, prettyPath, toast } from "./js/dom.js";
import { initNewSessionDialog } from "./js/dialogs/newSession.js";
import { initRenameDialog, openRenameDialog } from "./js/dialogs/rename.js";
import { ensureToken, initTokenDialog, promptForToken } from "./js/dialogs/token.js";
import { initTakeoverDialog } from "./js/lease.js";
import { initNotifications, refreshNotifications, startNotificationPolling } from "./js/notifications.js";
import { agentHash, currentRoute, navigate, onRoute, sessionHash, shellHash, startRouter } from "./js/router.js";
import {
  getSession,
  getState,
  refresh,
  restorePreferences,
  startPolling,
  subscribe,
} from "./js/store.js";
import { closeAlerts, initAlerts } from "./js/views/alerts.js";
import { initDashboard, setActiveSession } from "./js/views/dashboard.js";
import { initOverview, setOverviewActive } from "./js/views/overview.js";
import { initDoctor, openDoctor } from "./js/views/doctor.js";
import {
  closeConversation,
  initConversation,
  openConversation,
  keepPinned,
  refreshChrome,
} from "./js/views/conversation.js";
import { closeShell, initShell, openShell, resizeShell } from "./js/views/shell.js";
import { closeTerminal, initTerminal, openTerminal, resizeTerminal } from "./js/views/terminal.js";

const nodes = {};

function cacheNodes() {
  nodes.app = byId("app");
  nodes.overviewPane = byId("overview-pane");
  nodes.pane = byId("session-pane");
  nodes.chat = byId("chat-view");
  nodes.shell = byId("shell-view");
  nodes.terminal = byId("terminal-view");
  nodes.title = byId("session-title");
  nodes.dot = byId("session-status-dot");
  nodes.meta = byId("session-meta");
  nodes.tabChat = byId("tab-chat");
  nodes.tabShell = byId("tab-shell");
  nodes.tabTerminal = byId("tab-terminal");
  nodes.actionsBtn = byId("actions-btn");
  nodes.actionsMenu = byId("actions-menu");
  nodes.rename = byId("action-rename");
  nodes.archive = byId("action-archive");
  nodes.delete = byId("action-delete");
  nodes.refresh = byId("refresh-btn");
  nodes.doctorPane = byId("doctor-pane");
  nodes.fullscreen = byId("fullscreen-btn");
  nodes.listToggle = byId("list-toggle");
  nodes.drawerScrim = byId("drawer-scrim");
}

// ------------------------------------------------------------------- routing

function applyRoute(route) {
  nodes.app.dataset.route = route.name;
  closeMenu();
  closeAlerts();
  // Following a link out of the drawer has to close it, or the session it opened is
  // behind the list that opened it.
  if (layoutMode() === "compact") setListOpen(false);
  // Full screen belongs to one terminal pane, not to the app: leaving that pane leaves it.
  if (route.name !== "shell" && route.name !== "agent") setFullscreen(false);

  if (route.name === "dashboard" || route.name === "doctor") {
    nodes.pane.hidden = true;
    nodes.doctorPane.hidden = route.name !== "doctor";
    nodes.overviewPane.hidden = route.name !== "dashboard";
    closeConversation();
    closeShell();
    closeTerminal();
    setActiveSession(null);
    setOverviewActive(null);
    if (route.name === "doctor") openDoctor();
    return;
  }

  nodes.pane.hidden = false;
  nodes.doctorPane.hidden = true;
  nodes.overviewPane.hidden = true;
  setActiveSession(route.key);
  setOverviewActive(route.key);
  nodes.tabChat.href = sessionHash(route.key);
  // Always the bare Shell route: entering the tab re-lists, which is what picks up a shell
  // another surface opened. The view then rewrites the hash with the shell it settled on.
  nodes.tabShell.href = shellHash(route.key);
  nodes.tabTerminal.href = agentHash(route.key);
  nodes.tabChat.classList.toggle("active", route.name === "session");
  nodes.tabShell.classList.toggle("active", route.name === "shell");
  nodes.tabTerminal.classList.toggle("active", route.name === "agent");

  nodes.chat.hidden = route.name !== "session";
  nodes.shell.hidden = route.name !== "shell";
  nodes.terminal.hidden = route.name !== "agent";

  if (route.name === "shell") {
    closeConversation();
    closeTerminal();
    openShell(route.key, route.shell);
  } else if (route.name === "agent") {
    closeConversation();
    closeShell();
    openTerminal(route.key);
  } else {
    closeShell();
    closeTerminal();
    openConversation(route.key);
  }
  updateSessionHeader(route.key);
}

function updateSessionHeader(key) {
  const session = getSession(key);
  const state = getState();
  if (!session) {
    nodes.title.textContent = state.loaded ? "Session not found" : "Loading…";
    nodes.dot.className = "dot idle";
    nodes.meta.textContent = key || "";
    return;
  }
  const home = inferHome([session.cwd]);
  nodes.title.textContent = session.title;
  nodes.title.title = session.title;
  nodes.dot.className = `dot ${session.status}`;
  nodes.dot.title = `status: ${session.status}`;

  nodes.meta.replaceChildren(
    el("span", { class: `agent-badge ${session.agent}`, text: session.agent }),
    el("span", { class: "path", text: prettyPath(session.cwd, home), title: session.cwd }),
    session.branch ? el("span", { class: "branch", text: `· ${session.branch}` }) : "",
    session.activity_age ? el("span", { class: "age", text: `· ${session.activity_age}` }) : "",
    session.archived ? el("span", { class: "archived-flag", text: "archived" }) : "",
  );
  nodes.archive.textContent = session.archived ? "Restore session" : "Archive session";
}

// -------------------------------------------------------------- actions menu

function toggleMenu() {
  const open = nodes.actionsMenu.hidden;
  nodes.actionsMenu.hidden = !open;
  nodes.actionsBtn.setAttribute("aria-expanded", String(open));
}

function closeMenu() {
  nodes.actionsMenu.hidden = true;
  nodes.actionsBtn.setAttribute("aria-expanded", "false");
  disarmDelete();
}

/* Terminating is confirmed inside the menu rather than with `window.confirm`:
   a native modal blocks the page and looks alien inside an installed PWA. */
let deleteArmed = false;
let deleteTimer = null;

function armDelete() {
  deleteArmed = true;
  nodes.delete.textContent = "Tap again to terminate";
  nodes.delete.classList.add("armed");
  window.clearTimeout(deleteTimer);
  deleteTimer = window.setTimeout(disarmDelete, 5000);
}

function disarmDelete() {
  window.clearTimeout(deleteTimer);
  deleteArmed = false;
  nodes.delete.textContent = "Terminate session";
  nodes.delete.classList.remove("armed");
}

function onRename() {
  const key = currentRoute().key;
  closeMenu();
  if (key) openRenameDialog(key);
}

async function onArchive() {
  const key = currentRoute().key;
  closeMenu();
  if (!key) return;
  try {
    await archiveSession(key);
    await refresh();
    const session = getSession(key);
    toast(session && session.archived ? "Session archived." : "Session restored.");
  } catch (error) {
    toast(failureText(error, "Could not archive the session"), "error");
  }
}

async function onDelete() {
  const key = currentRoute().key;
  if (!key) return;
  if (!deleteArmed) {
    armDelete();
    return;
  }
  closeMenu();
  try {
    await deleteSession(key);
    await refresh();
    toast("Session terminated.");
    navigate("#/");
  } catch (error) {
    toast(failureText(error, "Could not terminate the session"), "error");
  }
}

function failureText(error, prefix) {
  if (error instanceof ApiError && error.code === "unavailable") {
    return `${prefix}: this server build does not support it yet.`;
  }
  return `${prefix}: ${error.message}`;
}

// ---------------------------------------------------------------- full screen

/* The TUI has `maximize`, `hide_shells`, `grow_shell` and `shrink_shell`; a browser window
   already resizes, and a phone already shows one pane at a time, so the only part worth
   carrying over is "give the terminal everything". It is one toggle on desktop, hidden on a
   phone where the pane is full-screen already, and Escape gets out. */
function setFullscreen(on) {
  nodes.app.dataset.fullscreen = String(Boolean(on));
  nodes.fullscreen.setAttribute("aria-pressed", String(Boolean(on)));
  nodes.fullscreen.querySelector(".btn-text").textContent = on ? "Exit full screen" : "Full screen";
  resizeShell();
  resizeTerminal();
}

function toggleFullscreen() {
  setFullscreen(nodes.app.dataset.fullscreen !== "true");
}

// ------------------------------------------------------------- layout mode

/* Which layout the window gets, decided from the signals that describe the *device* rather
   than from its width alone.

   A width-only breakpoint gets a landscape phone wrong in the most visible way there is:
   844x390 is wider than any phone breakpoint, so it was handed the desktop two-pane layout
   -- a 360px list beside a 484px detail pane, on a screen with 390px of height. The signals
   that actually separate that device from a small laptop are its coarse pointer, its lack of
   hover, and its height.

     compact   one pane at a time. Narrow (<=768px, any pointer), or a coarse pointer with
               almost no vertical room (a phone on its side, a tablet with a keyboard tray).
     regular   two panes. Everything else, including an iPad in portrait, which at 820x1180
               genuinely has room for both.

   `short` and `touch` are separate axes on purpose: a desktop window dragged down to 400px
   tall should shed vertical chrome without losing its second pane, and a tablet in portrait
   should get 44px tap targets without losing one either. */
const COMPACT_QUERY = "(max-width: 768px), (pointer: coarse) and (max-height: 560px)";
const SHORT_QUERY = "(max-height: 560px)";
const TOUCH_QUERY = "(pointer: coarse), (hover: none)";

const LIST_KEY = "agent-console-list-collapsed";

function layoutMode() {
  return window.matchMedia(COMPACT_QUERY).matches ? "compact" : "regular";
}

/** The remembered choice for two-pane layouts. A drawer always starts closed. */
function listPreference() {
  return window.localStorage.getItem(LIST_KEY) !== "1";
}

function applyLayout() {
  const app = document.getElementById("app");
  const mode = layoutMode();
  const changed = app.dataset.layout !== mode;
  app.dataset.layout = mode;
  app.dataset.short = String(window.matchMedia(SHORT_QUERY).matches);
  app.dataset.touch = String(window.matchMedia(TOUCH_QUERY).matches);
  // Crossing between the two presentations resets the list to that mode's default: a
  // drawer left open would otherwise reappear as a pane, and vice versa.
  if (changed || !app.dataset.list) {
    setListOpen(mode === "regular" && listPreference());
  }
  resizePanes();
}

function setListOpen(open) {
  const app = document.getElementById("app");
  app.dataset.list = open ? "open" : "collapsed";
  const toggle = nodes.listToggle;
  if (!toggle) return;
  const label = open ? "Hide the session list" : "Show the session list";
  toggle.setAttribute("aria-expanded", String(open));
  toggle.setAttribute("aria-label", label);
  toggle.title = label;
  // Only the two-pane fold is a preference; opening a drawer is not a setting.
  if (layoutMode() === "regular") {
    window.localStorage.setItem(LIST_KEY, open ? "0" : "1");
  }
  resizePanes();
}

function toggleList() {
  setListOpen(document.getElementById("app").dataset.list !== "open");
}

function watchLayout() {
  for (const query of [COMPACT_QUERY, SHORT_QUERY, TOUCH_QUERY]) {
    window.matchMedia(query).addEventListener("change", applyLayout);
  }
}

/* The terminals only exist once their views are built, and the layout is applied before
   that -- the first paint must not be the wrong layout while an auth round trip is in
   flight. Until then a refit is a no-op rather than a crash. */
let panesReady = false;

function resizePanes() {
  if (!panesReady) return;
  resizeShell();
  resizeTerminal();
}

// ---------------------------------------------------------- sidebar splitter

const SPLIT_KEY = "agent-console-sidebar-width";
const MIN_SIDEBAR = 220;
const MAX_SIDEBAR = 720;

/** A drag handle on the sidebar's edge. Desktop only -- on a phone the list is the whole screen. */
function initSplitter() {
  const stored = Number(window.localStorage.getItem(SPLIT_KEY));
  if (stored >= MIN_SIDEBAR && stored <= MAX_SIDEBAR) applySidebarWidth(stored);

  const handle = el("div", {
    id: "sidebar-splitter",
    class: "sidebar-splitter",
    role: "separator",
    "aria-orientation": "vertical",
    "aria-label": "Resize the session list",
    title: "Drag to resize the session list",
  });
  byId("sidebar").append(handle);

  // A second press in quick succession restores the default width. It is detected here
  // rather than with a `dblclick` listener because the drag has to `preventDefault()` the
  // pointerdown, and that suppresses the compatibility click events a `dblclick` is built
  // from -- so the listener never fired at all.
  let lastPress = 0;
  handle.addEventListener("pointerdown", (event) => {
    event.preventDefault();
    if (event.timeStamp - lastPress < 400) {
      lastPress = 0;
      window.localStorage.removeItem(SPLIT_KEY);
      document.documentElement.style.removeProperty("--sidebar-width");
      resizeShell();
      resizeTerminal();
      return;
    }
    lastPress = event.timeStamp;
    handle.setPointerCapture(event.pointerId);
    let width = clampWidth(event.clientX);
    const move = (moved) => {
      width = clampWidth(moved.clientX);
      applySidebarWidth(width);
    };
    const up = () => {
      handle.removeEventListener("pointermove", move);
      handle.removeEventListener("pointerup", up);
      // The width the drag ended on, not the one it started from.
      window.localStorage.setItem(SPLIT_KEY, String(width));
      resizeShell();
      resizeTerminal();
    };
    handle.addEventListener("pointermove", move);
    handle.addEventListener("pointerup", up);
  });
}

function clampWidth(x) {
  return Math.round(Math.min(MAX_SIDEBAR, Math.max(MIN_SIDEBAR, x)));
}

function applySidebarWidth(width) {
  document.documentElement.style.setProperty("--sidebar-width", `${width}px`);
}

// ------------------------------------------------------------------ viewport

function trackViewport() {
  const viewport = window.visualViewport;
  const apply = () => {
    const height = viewport ? viewport.height : window.innerHeight;
    document.documentElement.style.setProperty("--app-height", `${Math.round(height)}px`);
    resizeShell();
    resizeTerminal();
  };
  apply();
  if (viewport) {
    viewport.addEventListener("resize", () => {
      apply();
      // The on-screen keyboard just changed the visible height; keep the newest
      // message in view instead of leaving the composer floating over old text.
      if (currentRoute().name === "session") keepPinned();
    });
    viewport.addEventListener("scroll", apply);
  } else {
    window.addEventListener("resize", apply);
  }
}

// ---------------------------------------------------------------- bootstrap

function registerServiceWorker() {
  if ("serviceWorker" in navigator) {
    navigator.serviceWorker.register("/service-worker.js").catch(() => {});
  }
}

async function init() {
  cacheNodes();
  // Before the first paint, and before any network round trip: a flash of the wrong
  // layout is the exact thing this attribute exists to prevent.
  applyLayout();
  restorePreferences();
  // Which credential this server wants decides whether the app prompts at all. With HTTP
  // Basic the browser draws the prompt itself, and a token overlay on top of it would be a
  // second login box for a token that does not exist.
  const basic = (await loadAuthMode()) === "basic";
  if (!basic) {
    initTokenDialog();
    onUnauthorized(() => {
      promptForToken().then(() => window.location.reload());
    });
    await ensureToken();
  }

  initNotifications();
  initDashboard();
  initOverview();
  initAlerts();
  initDoctor();
  initConversation();
  initShell();
  initTerminal();
  panesReady = true;
  initNewSessionDialog({ onCreated: (key) => navigate(sessionHash(key)) });
  initRenameDialog();
  initTakeoverDialog();
  initCopyFallback();
  initSplitter();
  watchLayout();

  nodes.refresh.addEventListener("click", () => refresh());
  nodes.listToggle.addEventListener("click", toggleList);
  nodes.drawerScrim.addEventListener("click", () => setListOpen(false));
  nodes.rename.addEventListener("click", onRename);
  nodes.fullscreen.addEventListener("click", toggleFullscreen);
  nodes.actionsBtn.addEventListener("click", (event) => {
    event.stopPropagation();
    toggleMenu();
  });
  nodes.archive.addEventListener("click", onArchive);
  nodes.delete.addEventListener("click", onDelete);
  document.addEventListener("click", (event) => {
    if (!nodes.actionsMenu.hidden && !nodes.actionsMenu.contains(event.target)) closeMenu();
  });
  document.addEventListener("keydown", (event) => {
    if (event.key !== "Escape") return;
    closeMenu();
    setFullscreen(false);
    if (layoutMode() === "compact") setListOpen(false);
  });

  subscribe(() => {
    const route = currentRoute();
    if (route.name === "dashboard") return;
    updateSessionHeader(route.key);
    if (route.name === "session") refreshChrome();
  });

  trackViewport();
  // Load the session list before the first route is applied: the conversation
  // labels messages with the session's agent name, and the header needs a title.
  await refresh();
  onRoute(applyRoute);
  startRouter();
  startPolling();
  startNotificationPolling();
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState !== "visible") return;
    refresh();
    refreshNotifications();
  });
  registerServiceWorker();
}

init().catch((error) => {
  // eslint-disable-next-line no-console
  console.error("agent-console failed to start", error);
  toast(`Startup failed: ${error.message}`, "error");
});
