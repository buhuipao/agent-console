// Diagnostics: `agent-console doctor`, rendered in the browser.
//
// The probes spawn the provider binaries and take the better part of ten seconds, so this is
// never polled and never run on navigation -- it runs when someone presses the button, and
// says up front how long that will take. The last report stays on screen for the rest of the
// session so switching away and back does not mean paying for it again.

import { fetchDoctor } from "../api.js";
import { byId, clear, el } from "../dom.js";

const nodes = {};

const state = {
  report: null,
  running: false,
  error: null,
  ranAt: null,
};

export function initDoctor() {
  nodes.pane = byId("doctor-pane");
  nodes.run = byId("doctor-run");
  nodes.status = byId("doctor-status");
  nodes.body = byId("doctor-body");
  nodes.run.addEventListener("click", run);
  render();
}

/** Entering the route only shows what is already known; the probes need an explicit press. */
export function openDoctor() {
  render();
}

async function run() {
  if (state.running) return;
  state.running = true;
  state.error = null;
  render();
  try {
    state.report = await fetchDoctor();
    state.ranAt = new Date();
  } catch (error) {
    state.error = error;
    state.report = null;
  } finally {
    state.running = false;
    render();
  }
}

function render() {
  if (!nodes.pane) return;
  nodes.run.disabled = state.running;
  nodes.run.textContent = state.running ? "Running…" : "Run diagnostics";
  nodes.status.className = state.running ? "doctor-status running" : "doctor-status";
  nodes.status.textContent = state.running
    ? "Probing providers — this takes several seconds."
    : "";

  clear(nodes.body);
  if (state.error) {
    nodes.body.append(
      el("p", { class: "banner error", text: `Could not run diagnostics: ${state.error.message}` }),
    );
    return;
  }
  if (!state.report) {
    if (!state.running) {
      nodes.body.append(
        el("p", {
          class: "empty-hint",
          text: "Nothing has been checked yet. Running the probes starts each provider's binary, so it takes several seconds.",
        }),
      );
    }
    return;
  }
  nodes.body.append(...reportSections(state.report));
}

function reportSections(report) {
  const sections = [verdict(report), providersSection(report), discoverySection(report)];
  if ((report.checks || []).length) {
    sections.push(section("Checks", (report.checks || []).map(checkRow)));
  }
  if (report.diagnostics_path) {
    sections.push(
      section("Log", [
        el("p", { class: "doctor-path", text: report.diagnostics_path }),
      ]),
    );
  }
  return sections;
}

function verdict(report) {
  const ok = Boolean(report.ok);
  return el("div", { class: `doctor-verdict ${ok ? "ok" : "bad"}` }, [
    el("span", { class: "doctor-verdict-word", text: ok ? "Healthy" : "Problems found" }),
    el("span", {
      class: "doctor-verdict-detail",
      text: ok
        ? "Every required capability answered."
        : `${report.failures} check${report.failures === 1 ? "" : "s"} failed.`,
    }),
    el("span", { class: "doctor-verdict-meta", text: `agent-console ${report.version || "?"}` }),
    state.ranAt
      ? el("span", { class: "doctor-verdict-meta", text: `checked ${state.ranAt.toLocaleTimeString()}` })
      : null,
  ]);
}

function providersSection(report) {
  const enabled = report.providers_enabled || [];
  const rows = (report.providers || []).map(providerCard);
  if (!rows.length) rows.push(el("p", { class: "empty-hint", text: "No providers are enabled." }));
  return section(`Providers${enabled.length ? ` (enabled: ${enabled.join(", ")})` : ""}`, rows);
}

function providerCard(provider) {
  const support = provider.version_support;
  return el("article", { class: "doctor-provider" }, [
    el("header", { class: "doctor-provider-head" }, [
      el("span", { class: `doctor-mark ${provider.available ? "ok" : "bad"}`, text: provider.available ? "OK" : "MISSING" }),
      el("span", { class: "doctor-provider-name", text: provider.name }),
      support ? el("span", { class: `version-badge ${support}`, text: versionLabel(support) }) : null,
    ]),
    el("p", { class: "doctor-detail", text: provider.detail || "" }),
    (provider.capabilities || []).length
      ? el("ul", { class: "doctor-checks" }, (provider.capabilities || []).map(checkRow))
      : null,
  ]);
}

/** The report's own vocabulary, spelled out: `too_old` means nothing to someone reading it cold. */
function versionLabel(support) {
  if (support === "supported") return "version supported";
  if (support === "too_old") return "version too old";
  return "version unknown";
}

function checkRow(check) {
  return el("li", { class: `doctor-check ${check.ok ? "ok" : "bad"}` }, [
    el("span", { class: `doctor-mark ${check.ok ? "ok" : "bad"}`, text: check.ok ? "OK" : "FAIL" }),
    el("span", { class: "doctor-check-name", text: check.name }),
    el("span", { class: "doctor-detail", text: check.detail || "" }),
  ]);
}

function discoverySection(report) {
  const paths = report.discovery || [];
  if (!paths.length) {
    return section("Discovery", [
      el("p", { class: "empty-hint", text: "No transcript directories were resolved." }),
    ]);
  }
  return section(
    "Discovery",
    [
      el(
        "ul",
        { class: "doctor-checks" },
        paths.map((entry) =>
          el("li", { class: `doctor-check ${entry.exists ? "ok" : "bad"}` }, [
            el("span", { class: `doctor-mark ${entry.exists ? "ok" : "bad"}`, text: entry.exists ? "OK" : "MISSING" }),
            el("span", { class: "doctor-check-name", text: entry.name }),
            el("span", { class: "doctor-path", text: entry.path }),
          ]),
        ),
      ),
    ],
  );
}

function section(title, children) {
  return el("section", { class: "doctor-section" }, [
    el("h3", { class: "doctor-section-title", text: title }),
    ...children,
  ]);
}
