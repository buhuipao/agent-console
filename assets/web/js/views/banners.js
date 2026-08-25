// Conversation banners: unavailable notices, pending decisions, session summary.

import { el } from "../dom.js";
import { agentHash } from "../router.js";

export function unavailableBanner(key, kind) {
  const link = el("a", { href: key ? agentHash(key) : "#/" });
  if (kind === "messages") {
    link.textContent = "Open the Agent TUI tab";
    return el("div", { class: "banner warn" }, [
      el("strong", { class: "banner-title", text: "Conversation feed unavailable" }),
      el("span", { text: "This server build does not expose /api/sessions/:key/messages yet. " }),
      link,
      el("span", { text: " to drive this session in the meantime." }),
    ]);
  }
  link.textContent = "Use the Agent TUI tab";
  return el("div", { class: "banner warn" }, [
    el("strong", { class: "banner-title", text: "Sending is unavailable" }),
    el("span", { text: "This server build does not expose /api/sessions/:key/prompt yet. " }),
    link,
    el("span", { text: " to type into the session." }),
  ]);
}

/**
 * The blocking prompts a session is stuck on.
 *
 * A blocking dialog never reaches the transcript, so without this the conversation of a
 * session that is waiting for an answer looks exactly like one that is simply idle. It is a
 * card rather than a line of small print for that reason: it is the only thing on screen
 * that explains why nothing is happening.
 *
 * Two sources feed one card, because to the reader they are the same interruption. The
 * hook-driven `pending_decisions` know a decision exists but not what its choices are, so
 * they still hand off to the terminal. `prompt` comes from the screen read behind
 * `/prompt-status`, which carries the provider's own numbered options, so it renders those
 * as buttons and is answerable without ever leaving this view. It is listed first for that
 * reason: it is the item the reader can actually resolve here.
 *
 * Each item keeps its own `.decision-actions` row, addressable by `data-decision-id`.
 */
export function decisionsBanner(session, { prompt = null, onAnswer = null } = {}) {
  const items = [];
  if (prompt) items.push({ id: "blocking-prompt", question: prompt.question, prompt });
  for (const decision of session.pending_decisions || []) {
    items.push({ id: decision.id, question: decision.question || decision.id });
  }
  const agent = session.agent || "the agent";
  return el("section", { class: "decision-card", role: "alert" }, [
    el("div", { class: "decision-head" }, [
      el("span", { class: "decision-badge", text: "Needs you" }),
      el("span", {
        class: "decision-head-text",
        text: items.length === 1
          ? `${agent} is blocked until you answer.`
          : `${agent} is blocked on ${items.length} answers.`,
      }),
    ]),
    ...items.map((item, index) => decisionItem(session, item, onAnswer, index, items.length)),
  ]);
}

function decisionItem(session, item, onAnswer, index, total) {
  return el("div", { class: "decision-item", dataset: { decisionId: String(item.id ?? "") } }, [
    total > 1 ? el("span", { class: "decision-index", text: `${index + 1}/${total}` }) : null,
    el("p", {
      class: "decision-question",
      text: item.question || "The agent is waiting for input.",
    }),
    item.prompt && onAnswer
      ? optionRow(item.prompt, onAnswer)
      : el("div", { class: "decision-actions" }, [
          el("a", {
            class: "btn btn-primary decision-answer",
            href: agentHash(session.key),
            text: "Answer in the Agent TUI",
          }),
        ]),
  ]);
}

/**
 * One button per option the provider is offering.
 *
 * No option is styled as the recommended one. The safe answer is "1. Yes, I trust this
 * folder" on one dialog and "2. No, keep asking" on the next, so a primary button here would
 * be the UI guessing on the user's behalf about a choice it cannot read.
 */
function optionRow(prompt, onAnswer) {
  const row = el("div", { class: "decision-actions" });
  for (const option of prompt.options) {
    row.append(
      el(
        "button",
        {
          class: "btn decision-option",
          type: "button",
          title: `Answer with option ${option.number}`,
          onclick: () => {
            setBusy(row, true);
            // The row may be replaced by a re-render before this settles; re-enabling a
            // detached button is harmless, and leaving a live one disabled is not.
            Promise.resolve(onAnswer(option)).finally(() => setBusy(row, false));
          },
        },
        [
          el("span", { class: "option-number", text: String(option.number) }),
          el("span", { class: "option-label", text: option.label }),
        ],
      ),
    );
  }
  return row;
}

function setBusy(row, busy) {
  for (const button of row.querySelectorAll("button")) button.disabled = busy;
}

/**
 * The rolling summary, and the way to ask for it again.
 *
 * The retry lives here rather than in a menu because this is where its absence is felt: a
 * summary that failed to generate shows up as a card with nothing in it, and the useful
 * response to that is one button away. That is also why an empty summary still renders a
 * card -- returning `null` for "no summary" would hide the retry in exactly the case that
 * needs it.
 */
export function summaryCard(session, { onRetry = null } = {}) {
  const summary = session.summary;
  const hasDetail = Boolean(
    summary &&
      (summary.current_action || summary.next_step ||
        (summary.progress || []).length || (summary.blockers || []).length),
  );
  if (!summary || (!summary.task && !hasDetail)) {
    return onRetry ? emptySummaryCard(onRetry) : null;
  }

  const body = el("dl", { class: "summary-body" });
  if (summary.current_action) {
    body.append(el("dt", { text: "Now" }), el("dd", { text: summary.current_action }));
  }
  if (summary.next_step) {
    body.append(el("dt", { text: "Next" }), el("dd", { text: summary.next_step }));
  }
  appendList(body, "Progress", summary.progress);
  appendList(body, "Blockers", summary.blockers);

  return el("details", { class: "summary-card" }, [
    el("summary", {}, [
      el("span", { class: "summary-label", text: "Summary" }),
      el("span", { class: "summary-task", text: summary.task || summary.current_action || "" }),
      onRetry ? retryButton(onRetry) : null,
      el("span", { class: "fold-caret", "aria-hidden": "true", text: "\u25b8" }),
    ]),
    body,
  ]);
}

function emptySummaryCard(onRetry) {
  return el("div", { class: "summary-card summary-card-empty" }, [
    el("div", { class: "summary-empty-row" }, [
      el("span", { class: "summary-label", text: "Summary" }),
      el("span", { class: "summary-task muted", text: "not generated yet" }),
      retryButton(onRetry),
    ]),
  ]);
}

/** Inside a `<summary>` the click has to be stopped, or asking for a retry folds the card. */
function retryButton(onRetry) {
  return el("button", {
    class: "summary-retry",
    type: "button",
    text: "Retry",
    title: "Queue this session's summary again, ahead of the others",
    onclick: (event) => {
      event.preventDefault();
      event.stopPropagation();
      const button = event.currentTarget;
      button.disabled = true;
      Promise.resolve(onRetry()).finally(() => {
        button.disabled = false;
      });
    },
  });
}

function appendList(body, label, items) {
  if (!(items || []).length) return;
  const list = el("ul", {});
  for (const item of items) list.append(el("li", { text: item }));
  body.append(el("dt", { text: label }), el("dd", {}, [list]));
}
