// Renders one conversation message (and its blocks) into DOM nodes.

import { el, oneLine } from "../dom.js";
import { renderMarkdown } from "../markdown.js";

const ROLE_LABELS = { user: "You", assistant: "Agent", system: "System" };

export function renderMessage(message, { agent = "", pending = false } = {}) {
  const role = ROLE_LABELS[message.role] ? message.role : "system";
  const label = role === "assistant" && agent ? agent : ROLE_LABELS[role];

  const body = el("div", { class: "msg-body" });
  const blocks = normaliseBlocks(message);
  if (!blocks.length) body.append(el("p", { class: "msg-empty", text: "(empty message)" }));
  for (const block of blocks) body.append(renderBlock(block));

  // Transcripts carry tool results as `user` turns and tool calls as bare
  // `assistant` turns. Labelling those "You"/"Claude" and wrapping each in a
  // bubble buries the actual conversation, so tool-only turns render as a
  // compact, unlabelled row of chips instead.
  const toolOnly = blocks.length > 0 && blocks.every(isToolBlock);

  return el(
    "article",
    {
      class: `msg ${toolOnly ? "tool" : role}${pending ? " pending" : ""}`,
      dataset: { id: String(message.id ?? "") },
    },
    [
      toolOnly
        ? null
        : el("div", { class: "msg-role" }, [
            el("span", { text: label }),
            pending ? el("span", { class: "msg-status sending", text: "sending…" }) : null,
            message.ts ? el("time", { text: formatTime(message.ts), title: String(message.ts) }) : null,
          ]),
      body,
    ],
  );
}

function isToolBlock(block) {
  return Boolean(block) && (block.type === "tool_use" || block.type === "tool_result");
}

function normaliseBlocks(message) {
  if (Array.isArray(message.blocks) && message.blocks.length) return message.blocks;
  if (typeof message.text === "string" && message.text) return [{ type: "text", text: message.text }];
  if (typeof message.content === "string" && message.content) {
    return [{ type: "text", text: message.content }];
  }
  return [];
}

function renderBlock(block) {
  switch (block && block.type) {
    case "text":
      return textBlock(block.text);
    case "thinking":
      return fold({
        className: "fold thinking",
        head: [el("span", { class: "fold-line", text: "Thinking" })],
        detail: block.text,
        markdown: true,
      });
    case "tool_use":
      return fold({
        className: "fold",
        head: [
          el("span", { class: "chip", text: block.name || "tool" }),
          el("span", { class: "fold-line", text: oneLine(block.summary) }),
        ],
        detail: block.summary,
      });
    case "tool_result": {
      const ok = block.ok !== false;
      return fold({
        className: `fold${ok ? "" : " error"}`,
        head: [
          el("span", { class: `chip ${ok ? "ok" : "fail"}`, text: ok ? "result" : "error" }),
          el("span", { class: "fold-line", text: oneLine(block.summary) }),
        ],
        detail: block.summary,
      });
    }
    case "image":
      return block.data ? imageBlock(block.data) : oversizedImageBlock();
    default:
      return fold({
        className: "fold",
        head: [
          el("span", { class: "chip", text: (block && block.type) || "block" }),
          el("span", { class: "fold-line", text: oneLine(safeJson(block)) }),
        ],
        detail: safeJson(block),
      });
  }
}

function textBlock(text) {
  const node = el("div", { class: "msg-block md" });
  node.append(renderMarkdown(text || ""));
  return node;
}

function imageBlock(dataUri) {
  const thumb = el("img", {
    class: "msg-image",
    src: dataUri,
    alt: "Attached image",
    loading: "lazy",
  });
  return el("div", { class: "msg-block" }, [
    el(
      "button",
      {
        class: "msg-image-btn",
        type: "button",
        title: "Click to view full size",
        onclick: () => openImagePreview(dataUri),
      },
      [thumb],
    ),
  ]);
}

function oversizedImageBlock() {
  return el("div", { class: "msg-block" }, [
    el("span", { class: "chip", text: "image" }),
    el("span", { class: "fold-line", text: " (too large to preview here)" }),
  ]);
}

function openImagePreview(dataUri) {
  const close = () => {
    overlay.remove();
    document.removeEventListener("keydown", onKey);
  };
  const onKey = (event) => {
    if (event.key === "Escape") close();
  };
  const overlay = el(
    "div",
    { class: "overlay image-preview-overlay", role: "dialog", "aria-label": "Image preview", onclick: close },
    [el("img", { class: "image-preview-full", src: dataUri, alt: "Attached image" })],
  );
  document.addEventListener("keydown", onKey);
  document.body.append(overlay);
}

function fold({ className, head, detail, markdown = false }) {
  const summary = el("summary", {}, [
    el("span", { class: "fold-caret", "aria-hidden": "true", text: "▸" }),
    ...head,
  ]);
  const body = el("div", { class: "fold-detail" });
  if (markdown) body.append(renderMarkdown(detail || ""));
  else body.append(el("pre", { text: String(detail ?? "") }));
  return el("details", { class: `msg-block ${className}` }, [summary, body]);
}

function safeJson(value) {
  try {
    return JSON.stringify(value, null, 2);
  } catch (error) {
    return String(value);
  }
}

function formatTime(ts) {
  const date = toDate(ts);
  if (!date) return "";
  const now = new Date();
  const sameDay =
    date.getFullYear() === now.getFullYear() &&
    date.getMonth() === now.getMonth() &&
    date.getDate() === now.getDate();
  const time = date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  return sameDay ? time : `${date.toLocaleDateString([], { month: "short", day: "numeric" })} ${time}`;
}

function toDate(ts) {
  if (typeof ts === "number") {
    const millis = ts > 1e12 ? ts : ts * 1000;
    const date = new Date(millis);
    return Number.isNaN(date.getTime()) ? null : date;
  }
  if (typeof ts === "string" && ts) {
    const numeric = Number(ts);
    if (Number.isFinite(numeric) && ts.trim() !== "") return toDate(numeric);
    const date = new Date(ts);
    return Number.isNaN(date.getTime()) ? null : date;
  }
  return null;
}
