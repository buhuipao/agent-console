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

// How much output is held while the tail is being read before the stream is reopened
// instead. Well past what an agent prints while somebody scrolls back through a few
// screens; beyond it, replaying the escape codes costs more than a fresh snapshot.
const HELD_BYTES_LIMIT = 8 * 1024 * 1024;

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
  // Where earlier output lives when this terminal has none. An agent that takes the
  // alternate screen has no scrollback by construction, so scrolling up finds nothing
  // and the reader is left guessing; a shell draws in the normal screen and needs none.
  altScreenHint = null,
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
    // The reader has scrolled off the tail, so output is held rather than written.
    away: false,
    held: [],
    heldBytes: 0,
    resync: false,
    // How much room this window has, measured from the container. What the server is *asked*
    // for, and never the size it answers with -- see `applyTermSize`.
    want: null,
    // The size the PTY is actually running at, as the server last reported it.
    pty: null,
    sent: null,
  };

  const ctrlKey = renderToolbar();
  const jumpToLatest = renderJump();
  const sizeNote = renderSizeNote();
  const altNotice = renderAltNotice();

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
      onclick: () => resume(),
    });
    view.append(jump);
    return jump;
  }

  function renderAltNotice() {
    if (!altScreenHint) return null;
    const note = el("div", { class: "term-alt-note", hidden: true });
    note.append(
      el("span", {
        text: "This agent draws on the alternate screen, so there is nothing above this view.",
      }),
    );
    const link = el("a", { class: "term-alt-link", text: "Read the conversation \u2192" });
    link.addEventListener("click", () => {
      const href = altScreenHint();
      if (href) window.location.hash = href;
    });
    note.append(link);
    view.append(note);
    return note;
  }

  /** xterm knows which buffer the program switched to, so no server round trip is needed. */
  function onAlternateScreen() {
    return Boolean(state.term) && state.term.buffer.active.type === "alternate";
  }

  function updateAltNotice() {
    if (altNotice) altNotice.hidden = !onAlternateScreen();
  }

  /* ------------------------------------------------- one PTY, several windows

     A session can be open in more than one place at once -- a desktop browser, a phone, the
     dashboard's own workspace -- and they all look at the same PTY, which has exactly one
     size. The server sizes it to the smallest window attached, so the agent's output fits
     every one of them, and tells each window what it settled on.

     A window with room left over shows the terminal at the PTY's size and leaves the rest
     empty. It deliberately does not stretch or reflow the output to fill the space: those
     lines are what somebody is scrolling back through, and re-wrapping them is the corruption
     this whole arrangement exists to avoid. The empty area is given its own backdrop and the
     size is named, so it reads as a decision rather than as a terminal that failed to fit. */

  function renderSizeNote() {
    const note = el("div", { class: "term-size", hidden: true });
    view.append(note);
    return note;
  }

  /** How many cells this window has room for, independent of the size xterm is currently at. */
  function measure() {
    if (!state.fit) return null;
    let proposed = null;
    try {
      proposed = state.fit.proposeDimensions();
    } catch (error) {
      // The container may not be laid out yet; the socket still opens at a default size.
      return null;
    }
    if (!proposed) return null;
    const cols = Math.floor(proposed.cols);
    const rows = Math.floor(proposed.rows);
    if (!Number.isFinite(cols) || !Number.isFinite(rows)) return null;
    return { cols: Math.max(2, cols), rows: Math.max(2, rows) };
  }

  /**
   * Points the emulator at the size the PTY is running at, letterboxing the rest.
   *
   * Never at `state.want`, once the server has answered: writing the measured size back into
   * xterm and then re-measuring xterm would ratchet every other window down to this one's
   * size and never let go.
   */
  function applyTermSize() {
    if (!state.term || !state.want) return;
    // Resizing reflows, which is exactly what somebody reading the history must not have
    // happen under them. It waits until they come back to the tail; `resume` calls this.
    if (state.away) return;
    const target = state.pty || state.want;
    const cols = Math.max(2, Math.min(target.cols, state.want.cols));
    const rows = Math.max(2, Math.min(target.rows, state.want.rows));
    if (state.term.cols !== cols || state.term.rows !== rows) state.term.resize(cols, rows);
    const short = cols < state.want.cols || rows < state.want.rows;
    view.classList.toggle("term-letterboxed", short);
    sizeNote.hidden = !short;
    if (short) sizeNote.textContent = `${cols}×${rows} · sized to the smallest window on this session`;
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

     Read the emulator's own two rows rather than the scroll container's pixels. A drag, a
     wheel and a keyboard scroll all reach `viewportY` through xterm's scroll handler, so it
     still tracks the finger -- but it is updated in the same turn as the write that moved
     it, where the container's height and `scrollTop` are only reconciled on the next
     animation frame. Reading the pixels meant a burst of output could be measured against a
     stale height, come out as "scrolled away", and hold the terminal's own output back. */
  function atBottom() {
    if (!state.term) return true;
    const buffer = state.term.buffer.active;
    return buffer.viewportY >= buffer.baseY;
  }

  /** `onScroll` alone misses a plain viewport drag, so the scroll container is watched too. */
  function watchScroll() {
    const viewport = container.querySelector(".xterm-viewport");
    if (viewport) viewport.addEventListener("scroll", updateJump, { passive: true });
  }

  function updateJump() {
    updateAltNotice();
    const bottom = atBottom();
    jumpToLatest.hidden = bottom || onAlternateScreen();
    if (!bottom) {
      state.away = true;
      return;
    }
    if (state.away) resume();
  }

  /* --------------------------------------------------- reading the history

     xterm's viewport is a real scrolling element, and every write reassigns its `scrollTop`
     to whichever row the emulator now has on top. A wheel's smooth scroll and a finger's
     momentum are browser animations against that same property, so an agent printing ten
     times a second cancels the gesture ten times a second: the Agent TUI tab crawled on a
     desktop and would not move at all under a thumb. The Shell tab looked fine only because
     a shell waiting at its prompt prints nothing while it is being read.

     So the tail stops while somebody is reading it. Arriving output is held in order and
     written when they come back to the bottom -- by scrolling there or by pressing the pill
     -- which is the one moment moving the viewport is what they asked for. */
  function writeToTerm(data) {
    if (!state.term) return;
    if (state.away) {
      hold(data);
      return;
    }
    // A program switches buffers by writing, so the notice has to be re-evaluated here.
    // Doing it only on scroll meant it never appeared for a reader who had not scrolled --
    // which is everyone who has just opened the tab.
    state.term.write(data, updateAltNotice);
  }

  function hold(data) {
    if (state.resync) return;
    state.heldBytes += data.length;
    if (state.heldBytes > HELD_BYTES_LIMIT) {
      // Reopening asks the server for a whole fresh snapshot, so dropping what is held here
      // cannot leave the screen half-rebuilt out of a partial escape sequence.
      state.held = [];
      state.heldBytes = 0;
      state.resync = true;
      return;
    }
    state.held.push(data);
  }

  function resume() {
    if (!state.term) return;
    const held = state.held;
    const resync = state.resync;
    releaseHold();
    // Held back while the history was being read, because it reflows the screen.
    applyTermSize();
    if (resync) {
      // The close handler reconnects: one path back onto the wire rather than two.
      if (state.socket) state.socket.close();
    } else {
      for (const data of held) state.term.write(data);
    }
    state.term.scrollToBottom();
    updateJump();
    scheduleFit();
  }

  function releaseHold() {
    state.away = false;
    state.held = [];
    state.heldBytes = 0;
    state.resync = false;
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

  /** Re-measures the window and tells the server, then re-applies whatever size it is at. */
  function remeasure() {
    const want = measure();
    if (want) state.want = want;
    sendResize();
    applyTermSize();
  }

  function scheduleFit() {
    if (state.fitScheduled || !state.term || view.hidden || state.away) return;
    state.fitScheduled = true;
    requestAnimationFrame(() => {
      state.fitScheduled = false;
      if (!state.term || view.hidden) return;
      remeasure();
    });
  }

  /** Reports the room this window has. Only on a change: a resize costs the PTY a repaint. */
  function sendResize() {
    if (!state.want) return;
    if (!state.socket || state.socket.readyState !== WebSocket.OPEN) return;
    if (state.sent && state.sent.cols === state.want.cols && state.sent.rows === state.want.rows) {
      return;
    }
    state.sent = state.want;
    state.socket.send(JSON.stringify({ type: "resize", cols: state.want.cols, rows: state.want.rows }));
  }

  function connect(stream) {
    // Every connection is answered with a fresh snapshot of the whole retained terminal, so
    // the buffer starts empty for each one. A reconnect that kept what the last connection
    // wrote would show the history twice.
    state.term.reset();
    releaseHold();
    jumpToLatest.hidden = true;
    // A fresh socket is answered with a `size` frame of its own before any output, so what
    // the last one was told says nothing about this one.
    state.pty = null;
    state.sent = null;
    // The `WebSocket` constructor takes no headers, so the handshake's credential has to
    // ride in the URL (token mode) or on the connection itself (Basic mode: the browser
    // replays its cached credentials, and the session cookie the server set backs that up).
    const size = state.want || { cols: state.term.cols, rows: state.term.rows };
    state.sent = size;
    const params = [`cols=${size.cols}`, `rows=${size.rows}`];
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
      writeToTerm(new Uint8Array(event.data));
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
    if (message.type === "size") {
      // What the PTY is running at, which is the smallest window attached to this session --
      // this one, or somebody else's. Either way it arrives ahead of the output drawn for it.
      if (message.cols > 0 && message.rows > 0) {
        state.pty = { cols: message.cols, rows: message.rows };
        applyTermSize();
      }
      return;
    }
    if (message.type === "scrollback") {
      seedScrollback(message.text);
      return;
    }
    if (message.type === "exit") {
      writeToTerm(`\r\n\x1b[33m[session ended: ${message.detail || "process exited"}]\x1b[0m\r\n`);
      return;
    }
    // The stream is fine; the keystrokes are not. Printing it into the terminal as well as
    // raising the dialog is deliberate -- the line stays on screen after the dialog is
    // dismissed, so a user who declined the takeover still knows why nothing is happening.
    if (message.type === "lease_denied") {
      writeToTerm(
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
    const measured = measure();
    if (measured) state.want = measured;
    applyTermSize();
    connect(stream);
    state.term.focus();
    scheduleFit();
    updateJump();
  }

  function close() {
    window.clearTimeout(state.reconnectTimer);
    releaseHold();
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
