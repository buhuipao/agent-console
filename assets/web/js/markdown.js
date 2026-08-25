// A small, dependency-free markdown renderer that emits DOM nodes.
//
// It covers what agents actually emit: fenced code, headings, ordered/unordered
// lists (nested), blockquotes, tables, rules, inline code, bold/italic/strike and
// links. It never touches `innerHTML`, so untrusted text cannot inject markup, and
// link hrefs are restricted to http/https/mailto.

const FENCE = /^\s{0,3}(`{3,}|~{3,})\s*([^\s`]*)/;
const HEADING = /^\s{0,3}(#{1,6})\s+(.*)$/;
const HR = /^\s{0,3}(?:-{3,}|\*{3,}|_{3,})\s*$/;
const QUOTE = /^\s{0,3}>\s?(.*)$/;
const BULLET = /^(\s*)([-*+])\s+(.*)$/;
const ORDERED = /^(\s*)(\d{1,9})[.)]\s+(.*)$/;
const TABLE_DIVIDER = /^\s*\|?[\s:-]*-[\s:|-]*\|?\s*$/;

const INLINE_SOURCE = [
  "(`+)([\\s\\S]*?)\\1", // 1,2 inline code
  "\\[([^\\]]*)\\]\\(([^)\\s]+)(?:\\s+\"[^\"]*\")?\\)", // 3,4 link
  "\\*\\*([\\s\\S]+?)\\*\\*", // 5 bold
  "__([\\s\\S]+?)__", // 6 bold
  "~~([\\s\\S]+?)~~", // 7 strike
  "\\*([^*\\n]+)\\*", // 8 italic
  "(https?://[^\\s<>()\\[\\]]+)", // 9 autolink
].join("|");

export function renderMarkdown(text) {
  const fragment = document.createDocumentFragment();
  for (const node of blocksToNodes(splitLines(text))) fragment.append(node);
  return fragment;
}

function splitLines(text) {
  return String(text ?? "").replace(/\r\n?/g, "\n").split("\n");
}

function blocksToNodes(lines) {
  const nodes = [];
  let index = 0;

  while (index < lines.length) {
    const line = lines[index];

    if (!line.trim()) {
      index += 1;
      continue;
    }

    const fence = line.match(FENCE);
    if (fence) {
      const marker = fence[1];
      const body = [];
      index += 1;
      while (index < lines.length && !lines[index].trim().startsWith(marker)) {
        body.push(lines[index]);
        index += 1;
      }
      index += 1; // consume the closing fence (or run off the end)
      nodes.push(codeBlock(body.join("\n"), fence[2]));
      continue;
    }

    const heading = line.match(HEADING);
    if (heading) {
      const level = Math.min(heading[1].length, 4);
      const node = document.createElement(`h${level}`);
      node.append(renderInline(heading[2]));
      nodes.push(node);
      index += 1;
      continue;
    }

    if (HR.test(line)) {
      nodes.push(document.createElement("hr"));
      index += 1;
      continue;
    }

    if (QUOTE.test(line)) {
      const body = [];
      while (index < lines.length && QUOTE.test(lines[index])) {
        body.push(lines[index].match(QUOTE)[1]);
        index += 1;
      }
      const quote = document.createElement("blockquote");
      for (const child of blocksToNodes(body)) quote.append(child);
      nodes.push(quote);
      continue;
    }

    if (BULLET.test(line) || ORDERED.test(line)) {
      const [list, next] = readList(lines, index);
      nodes.push(list);
      index = next;
      continue;
    }

    if (line.includes("|") && index + 1 < lines.length && TABLE_DIVIDER.test(lines[index + 1])) {
      const [table, next] = readTable(lines, index);
      if (table) {
        nodes.push(table);
        index = next;
        continue;
      }
    }

    const paragraph = [];
    while (index < lines.length && lines[index].trim() && !isBlockStart(lines, index)) {
      paragraph.push(lines[index]);
      index += 1;
    }
    if (!paragraph.length) {
      // A block-start line that fell through every branch above; keep it as text
      // rather than looping forever.
      paragraph.push(lines[index]);
      index += 1;
    }
    const p = document.createElement("p");
    appendWithBreaks(p, paragraph);
    nodes.push(p);
  }

  return nodes;
}

function isBlockStart(lines, index) {
  const line = lines[index];
  return (
    FENCE.test(line) ||
    HEADING.test(line) ||
    HR.test(line) ||
    QUOTE.test(line) ||
    BULLET.test(line) ||
    ORDERED.test(line) ||
    (line.includes("|") && index + 1 < lines.length && TABLE_DIVIDER.test(lines[index + 1]))
  );
}

function appendWithBreaks(parent, lines) {
  lines.forEach((line, position) => {
    if (position > 0) parent.append(document.createElement("br"));
    parent.append(renderInline(line));
  });
}

function codeBlock(code, language) {
  const pre = document.createElement("pre");
  if (language) {
    const label = document.createElement("span");
    label.className = "code-lang";
    label.textContent = language;
    pre.append(label);
  }
  const node = document.createElement("code");
  node.textContent = code;
  pre.append(node);
  return pre;
}

/** Reads one list block starting at `start`; returns `[node, nextIndex]`. */
function readList(lines, start) {
  const first = lines[start].match(BULLET) || lines[start].match(ORDERED);
  const ordered = !lines[start].match(BULLET);
  const baseIndent = first[1].length;
  const list = document.createElement(ordered ? "ol" : "ul");

  let index = start;
  let item = null;

  while (index < lines.length) {
    const line = lines[index];
    const match = line.match(BULLET) || line.match(ORDERED);
    const indent = line.match(/^\s*/)[0].length;

    if (match && indent <= baseIndent + 1) {
      if (item) list.append(listItem(item));
      item = [match[3]];
      index += 1;
      continue;
    }
    if (!line.trim()) {
      // A blank line only continues the list when the next line is still indented.
      const next = lines[index + 1];
      if (next && next.trim() && next.match(/^\s*/)[0].length > baseIndent) {
        if (item) item.push("");
        index += 1;
        continue;
      }
      break;
    }
    if (indent > baseIndent && item) {
      item.push(line.slice(Math.min(indent, baseIndent + 2)));
      index += 1;
      continue;
    }
    break;
  }

  if (item) list.append(listItem(item));
  return [list, index];
}

function listItem(lines) {
  const li = document.createElement("li");
  // A single-line item stays inline (no wrapping <p>); anything richer goes
  // through the block renderer so nested lists and code fences work.
  const hasBlocks = lines.some((line, position) => position > 0 && line.trim());
  if (!hasBlocks) {
    li.append(renderInline(lines[0] ?? ""));
    return li;
  }
  for (const node of blocksToNodes(lines)) {
    if (node.tagName === "P" && li.childNodes.length === 0) {
      while (node.firstChild) li.append(node.firstChild);
    } else {
      li.append(node);
    }
  }
  return li;
}

function readTable(lines, start) {
  const cells = (line) =>
    line
      .trim()
      .replace(/^\|/, "")
      .replace(/\|$/, "")
      .split("|")
      .map((cell) => cell.trim());

  const header = cells(lines[start]);
  if (header.length < 2) return [null, start];

  const table = document.createElement("table");
  const thead = document.createElement("thead");
  const headRow = document.createElement("tr");
  for (const cell of header) {
    const th = document.createElement("th");
    th.append(renderInline(cell));
    headRow.append(th);
  }
  thead.append(headRow);
  table.append(thead);

  const tbody = document.createElement("tbody");
  let index = start + 2;
  while (index < lines.length && lines[index].includes("|") && lines[index].trim()) {
    const row = document.createElement("tr");
    for (const cell of cells(lines[index])) {
      const td = document.createElement("td");
      td.append(renderInline(cell));
      row.append(td);
    }
    tbody.append(row);
    index += 1;
  }
  table.append(tbody);
  return [table, index];
}

function renderInline(text) {
  const fragment = document.createDocumentFragment();
  const source = String(text ?? "");
  let cursor = 0;

  // A fresh regex per call: `renderInline` recurses for bold/italic/strike, and a
  // shared `lastIndex` would be reset by the inner call and re-match forever.
  const scanner = new RegExp(INLINE_SOURCE, "g");
  for (const match of source.matchAll(scanner)) {
    if (match.index > cursor) fragment.append(source.slice(cursor, match.index));

    if (match[1] !== undefined) {
      const code = document.createElement("code");
      code.textContent = match[2].replace(/^ | $/g, "");
      fragment.append(code);
    } else if (match[3] !== undefined) {
      fragment.append(link(match[4], match[3] || match[4]));
    } else if (match[5] !== undefined || match[6] !== undefined) {
      fragment.append(wrap("strong", match[5] ?? match[6]));
    } else if (match[7] !== undefined) {
      fragment.append(wrap("del", match[7]));
    } else if (match[8] !== undefined) {
      fragment.append(wrap("em", match[8]));
    } else if (match[9] !== undefined) {
      fragment.append(link(match[9], match[9]));
    }

    cursor = match.index + match[0].length;
  }

  if (cursor < source.length) fragment.append(source.slice(cursor));
  return fragment;
}

function wrap(tag, inner) {
  const node = document.createElement(tag);
  node.append(renderInline(inner));
  return node;
}

function link(href, label) {
  const safe = /^(https?:|mailto:)/i.test(href.trim());
  if (!safe) {
    const code = document.createElement("span");
    code.textContent = label;
    return code;
  }
  const anchor = document.createElement("a");
  anchor.href = href.trim();
  anchor.target = "_blank";
  anchor.rel = "noreferrer noopener";
  anchor.textContent = label;
  return anchor;
}
