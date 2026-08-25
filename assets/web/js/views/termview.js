// The xterm.js surface every raw-PTY view is built on.
//
// The Shell tab and the Agent TUI tab differ only in which websocket they attach to, so
// terminal creation, fitting, reconnect backoff, selection handling and the on-screen key
// row live here once rather than being copied into each view.
//
// xterm.js is fetched the first time any of these opens, so the conversation UI stays light
// on a phone connection.

import { getAuthMode, getToken } from "../api.js";
import { el } from "../dom.js";

const RECONNECT_BASE_MS = 500;
const RECONNECT_MAX_MS = 10000;

// The key row a phone keyboard has no room for. Ctrl is sticky: tap it, then a letter.
const TOOLBAR_KEYS = [
  { key: "esc", label: "Esc", title: "Escape key", bytes: [0x1b] },
  { key: "tab", label: "Tab", title: "Tab key", bytes: [0x09] },
  { key: "ctrl", label: "Ctrl", title: "Ctrl modifier: tap, then press a letter" },
  { key: "up", label: "↑", title: "Arrow up", bytes: [0x1b, 0x5b, 0x41] },
  { key: "down", label: "↓", title: "Arrow down", bytes: [0x1b, 0x5b, 0x42] },
  { key: "left", label: "←", title: "Arrow left", bytes: [0x1b, 0x5b, 0x44] },
  { key: "right", label: "→", title: "Arrow right", bytes: [0x1b, 0x5b, 0x43] },
  { key: "enter", label: "↵", title: "Enter key", bytes: [0x0d] },
];

let loading = null;

function loadScript(src) {
  return new Promise((resolve, reject) => {
    const script = document.createElement("script");
    script.src = src;
    script.onload = resolve;
    script.onerror = () => reject(new Error(`failed to load ${src}`));
    document.head.append(script);
  });
}

function ensureXterm() {
  if (window.Terminal && window.FitAddon) return Promise.resolve();
  if (!loading) {
    loading = loadScript("/vendor/xterm.js").then(() => loadScript("/vendor/xterm-addon-fit.js"));
  }
  return loading;
}

function wsUrl(path) {
  const protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
  return `${protocol}//${window.location.host}${path}`;
}

/**
 * Builds one attachable terminal.
 *
 * @param view          the element that is hidden when this tab is not showing
 * @param container     where xterm renders
 * @param toolbar       an empty element the mobile key row is rendered into
 * @param shouldReconnect  guards the reconnect loop for streams that must not be revived
 * @param onLeaseDenied    called when the server refuses this socket's keystrokes because
 *                         another surface holds the session's input lease
 */
export function createTerminalView({
  view,
  container,
  toolbar,
  shouldReconnect = () => true,
  onLeaseDenied = null,
}) {
  const state = {
    stream: null, // { id, path }
    term: null,
    fit: null,
    socket: null,
    attempt: 0,
    reconnectTimer: null,
    ctrlArmed: false,
    fitScheduled: false,
  };

  const ctrlKey = renderToolbar();
  const jumpToLatest = renderJump();

  function renderToolbar() {
    const buttons = TOOLBAR_KEYS.map((entry) =>
      el("button", {
        type: "button",
        class: "tb-key",
        title: entry.title,
        "aria-label": entry.title,
        dataset: { key: entry.key },
        text: entry.label,
      }),
    );
    toolbar.replaceChildren(...buttons);
    toolbar.addEventListener("click", onToolbarClick);
    return buttons.find((button) => button.dataset.key === "ctrl");
  }

  function onToolbarClick(event) {
    const button = event.target.closest(".tb-key");
    if (!button) return;
    const entry = TOOLBAR_KEYS.find((candidate) => candidate.key === button.dataset.key);
    if (!entry) return;
    if (entry.key === "ctrl") {
      state.ctrlArmed = !state.ctrlArmed;
      ctrlKey.classList.toggle("armed", state.ctrlArmed);
      return;
    }
    send(new Uint8Array(entry.bytes));
    if (state.term) state.term.focus();
  }

  /* ----------------------------------------------------------- scrollback

     Attaching used to give this terminal a checkpoint -- one screenful -- and then the live
     stream, so xterm started with an empty scrollback and only accumulated what arrived
     afterwards. Everything printed before the tab opened was not in the browser at all, which
     is the report "I cannot swipe back through the session's history on my phone" and is the
     same bug on a desktop: a phone just runs out of visible rows sooner.

     There is no separate history view for it. Scrolling back is what a terminal already does
     -- a wheel on a desktop, a drag on a phone -- so the fix is to put the missing rows into
     *this* terminal. The server's first poll now answers with the rows above the screen as
     well as the screen itself, taken from one parser at one instant (`RawPoll::scrollback`),
     and `seedScrollback` writes them in ahead of the checkpoint. */

  function renderJump() {
    const jump = el("button", {
      class: "term-jump",
      type: "button",
      hidden: true,
      title: "Scroll back down to the newest output",
      text: "\u2193 Jump to latest",
      onclick: () => {
        if (state.term) state.term.scrollToBottom();
        updateJump();
      },
    });
    view.append(jump);
    return jump;
  }

  /**
   * Writes the rows the terminal printed before this socket existed into xterm's scrollback.
   *
   * The trailing blank lines are the whole trick. Writing a row scrolls the row above it out
   * of the viewport and into the scrollback, so after the rows alone the last screenful of
   * them is still *on* the screen -- where the checkpoint's clear-screen would erase it, and
   * that band of lines would be gone. One blank line per visible row pushes every seeded row
   * past the fold first, leaving a blank screen for the checkpoint to repaint over.
   *
   * `term.rows` is read here, at write time, rather than being decided by the server: this is
   * the only place the emulator's real height is known, and a fit between the socket opening
   * and this frame arriving would make any server-side count wrong by exactly the difference.
   */
  function seedScrollback(text) {
    if (!state.term || !text) return;
    state.term.write(text + "\r\n".repeat(state.term.rows));
  }

  /* xterm follows the tail only while its viewport is already at the bottom -- which is
     exactly the rule we want, so the pill reports that state rather than overriding it.

     Read off the scroll container rather than off `buffer.viewportY`: the DOM position is
     what the reader's finger actually moved, and it is the one signal that is correct for a
     drag, a wheel, a keyboard scroll and a write alike. */
  function atBottom() {
    const viewport = container.querySelector(".xterm-viewport");
    if (!viewport) return true;
    return viewport.scrollTop + viewport.clientHeight >= viewport.scrollHeight - 2;
  }

  /** `onScroll` alone misses a plain viewport drag, so the scroll container is watched too. */
  function watchScroll() {
    const viewport = container.querySelector(".xterm-viewport");
    if (viewport) viewport.addEventListener("scroll", updateJump, { passive: true });
  }

  function updateJump() {
    jumpToLatest.hidden = atBottom();
  }

  function ensureTerm() {
    if (state.term) return;
    state.term = new window.Terminal({
      cursorBlink: true,
      fontSize: 13,
      // Comfortably more than the 2000 rows `pty.rs` retains and seeds, so the whole snapshot
      // survives alongside everything that arrives live afterwards.
      scrollback: 5000,
      theme: { background: "#000000", foreground: "#e2e8f0" },
    });
    state.fit = new window.FitAddon.FitAddon();
    state.term.loadAddon(state.fit);
    state.term.open(container);
    container.addEventListener("mousedown", () => state.term.focus());
    container.addEventListener("touchstart", () => state.term.focus(), { passive: true });
    state.term.onData(onTermData);
    state.term.onScroll(updateJump);
    watchScroll();
    // xterm only listens for a press on its own screen, so a selection made by dragging
    // could be dismissed by clicking back into the terminal and nowhere else: tapping the
    // key row, the header or the session list left stray highlighted bands on screen with
    // no way to clear them. Anything pressed outside the terminal cancels the selection.
    document.addEventListener("pointerdown", onDocumentPointerDown);

    new ResizeObserver(() => scheduleFit()).observe(container);
    if (window.visualViewport) {
      window.visualViewport.addEventListener("resize", () => scheduleFit());
    }
    // Coming back to a hidden tab is the other moment the measured size is stale.
    document.addEventListener("visibilitychange", () => {
      if (document.visibilityState === "visible") scheduleFit();
    });
  }

  function onDocumentPointerDown(event) {
    if (!state.term || !state.term.hasSelection()) return;
    if (container.contains(event.target)) return;
    state.term.clearSelection();
  }

  function onTermData(data) {
    if (!state.socket || state.socket.readyState !== WebSocket.OPEN) return;
    // Only a single character is a "key" the armed modifier can apply to. Escape sequences
    // arrive here too -- an arrow key, and every mouse report a full-screen app asks for --
    // and letting one of those spend the modifier meant a tap on the terminal to raise the
    // keyboard silently disarmed Ctrl before the letter that was meant to follow it.
    if (state.ctrlArmed && data.length === 1) {
      state.ctrlArmed = false;
      ctrlKey.classList.remove("armed");
      state.socket.send(new Uint8Array([data.charCodeAt(0) & 0x1f]));
      return;
    }
    state.socket.send(new TextEncoder().encode(data));
  }

  function send(bytes) {
    if (state.socket && state.socket.readyState === WebSocket.OPEN) state.socket.send(bytes);
  }

  function safeFit() {
    try {
      state.fit.fit();
    } catch (error) {
      // The container may not be laid out yet; the socket still opens at a default size.
    }
  }

  function scheduleFit() {
    if (state.fitScheduled || !state.term || view.hidden) return;
    state.fitScheduled = true;
    requestAnimationFrame(() => {
      state.fitScheduled = false;
      if (!state.term || view.hidden) return;
      safeFit();
      sendResize();
    });
  }

  function sendResize() {
    if (state.socket && state.socket.readyState === WebSocket.OPEN) {
      state.socket.send(JSON.stringify({ type: "resize", cols: state.term.cols, rows: state.term.rows }));
    }
  }

  function connect(stream) {
    // Every connection is answered with a fresh snapshot of the whole retained terminal, so
    // the buffer starts empty for each one. A reconnect that kept what the last connection
    // wrote would show the history twice.
    state.term.reset();
    // The `WebSocket` constructor takes no headers, so the handshake's credential has to
    // ride in the URL (token mode) or on the connection itself (Basic mode: the browser
    // replays its cached credentials, and the session cookie the server set backs that up).
    const params = [`cols=${state.term.cols}`, `rows=${state.term.rows}`];
    if (getAuthMode() !== "basic") params.push(`token=${encodeURIComponent(getToken())}`);
    const socket = new WebSocket(wsUrl(`${stream.path}?${params.join("&")}`));
    socket.binaryType = "arraybuffer";
    state.socket = socket;

    socket.addEventListener("open", () => {
      state.attempt = 0;
    });
    socket.addEventListener("message", (event) => {
      if (typeof event.data === "string") {
        onControlFrame(event.data);
        return;
      }
      state.term.write(new Uint8Array(event.data));
    });
    socket.addEventListener("close", () => {
      if (state.socket === socket) state.socket = null;
      scheduleReconnect(stream);
    });
    socket.addEventListener("error", () => socket.close());
  }

  function onControlFrame(raw) {
    let message;
    try {
      message = JSON.parse(raw);
    } catch (error) {
      return;
    }
    if (message.type === "scrollback") {
      seedScrollback(message.text);
      return;
    }
    if (message.type === "exit") {
      state.term.write(`\r\n\x1b[33m[session ended: ${message.detail || "process exited"}]\x1b[0m\r\n`);
      return;
    }
    // The stream is fine; the keystrokes are not. Printing it into the terminal as well as
    // raising the dialog is deliberate -- the line stays on screen after the dialog is
    // dismissed, so a user who declined the takeover still knows why nothing is happening.
    if (message.type === "lease_denied") {
      state.term.write(
        `\r\n\x1b[33m[not typed: ${message.detail || "another surface holds this session"}]\x1b[0m\r\n`,
      );
      if (onLeaseDenied) onLeaseDenied(message);
    }
  }

  function scheduleReconnect(stream) {
    if (!state.stream || state.stream.id !== stream.id) return;
    if (!shouldReconnect(stream)) return;
    const delay = Math.min(RECONNECT_BASE_MS * 2 ** state.attempt, RECONNECT_MAX_MS);
    state.attempt += 1;
    state.reconnectTimer = window.setTimeout(() => {
      if (state.stream && state.stream.id === stream.id) connect(stream);
    }, delay);
  }

  /** Attaches to `{ id, path }`; re-opening the stream already showing only refits it. */
  async function open(stream) {
    if (state.stream && state.stream.id === stream.id && state.socket) {
      scheduleFit();
      return;
    }
    close();
    state.stream = stream;
    try {
      await ensureXterm();
    } catch (error) {
      throw new Error("could not load the terminal library");
    }
    if (!state.stream || state.stream.id !== stream.id) return;

    ensureTerm();
    state.attempt = 0;
    // Connect synchronously. `requestAnimationFrame` never fires while the tab is
    // backgrounded, so deferring the socket to a frame callback left the terminal
    // permanently blank for anyone who opened it from a hidden tab.
    safeFit();
    connect(stream);
    state.term.focus();
    scheduleFit();
    updateJump();
  }

  function close() {
    window.clearTimeout(state.reconnectTimer);
    state.stream = null;
    if (state.socket) {
      const socket = state.socket;
      state.socket = null;
      socket.close();
    }
  }

  return {
    open,
    close,
    resize: scheduleFit,
    focus: () => state.term && state.term.focus(),
  };
}
