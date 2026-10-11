import init, * as wasm from "./pkg/drift_wasm.js";
import { initModes, RECORD_EXAMPLES } from "./modes.js";
import { EXAMPLES } from "./examples.js";

const $ = (id) => document.getElementById(id);
const MAX_LINES_PER_VALUE = 400;
const MAX_ROWS = 6000;

const { run_diff, run_records, detect_format, version } = wasm;
const state = {
  mode: "diff", report: null, reports: { diff: null, records: null },
  hints: { diff: { old: "", new: "" }, records: { old: "", new: "" } },
  rawMode: "json", opFilter: new Set(), wasmBytes: 0,
};

// ---------- small helpers ----------
const el = (tag, cls, text) => {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text !== undefined) node.textContent = text;
  return node;
};
const fmtMs = (ms) => (ms < 0.001 ? "< 1 µs" : ms < 1 ? `${(ms * 1000).toFixed(0)} µs` : ms < 100 ? `${ms.toFixed(2)} ms` : `${ms.toFixed(0)} ms`);
const fmtBytes = (n) => (n < 1024 ? `${n} B` : n < 1048576 ? `${(n / 1024).toFixed(1)} KB` : `${(n / 1048576).toFixed(2)} MB`);
const debounce = (fn, ms) => { let t; return (...a) => { clearTimeout(t); t = setTimeout(() => fn(...a), ms); }; };

// ---------- inputs ----------
// Two kinds of input share the page and the results below it: documents, and files of
// records. Their controls have the same ids, those of the records ones starting "r-".
function detectRecords(text, hint) {
  const h = (hint || "").toLowerCase();
  if (["ndjson", "csv", "tsv"].includes(h)) return h;
  if (/\.(ndjson|jsonl)$/.test(h)) return "ndjson";
  if (/\.csv$/.test(h)) return "csv";
  if (/\.tsv$/.test(h)) return "tsv";
  const first = (text.split("\n").find((l) => l.trim()) ?? "").trim();
  if (first.startsWith("{") || first.startsWith("[")) return "ndjson";
  const count = (ch) => first.split(ch).length - 1;
  return count("\t") > count(",") ? "tsv" : "csv";
}

const KINDS = {
  diff: { p: "", detect: (text, hint) => detect_format(text, hint) },
  records: { p: "r-", detect: detectRecords },
};

function hintFor(kind, side) {
  return $(`${KINDS[kind].p}fmt-${side}`).value || state.hints[kind][side] || "";
}

function updateDetected(kind, side) {
  const { p, detect } = KINDS[kind];
  const text = $(`${p}text-${side}`).value;
  const label = $(`${p}detected-${side}`);
  if (!text.trim()) { label.textContent = ""; $(`${p}foot-${side}`).textContent = ""; return; }
  label.textContent = `detected: ${detect(text, hintFor(kind, side))}`;
  const lines = text.split("\n").length;
  $(`${p}foot-${side}`).textContent = `${fmtBytes(new Blob([text]).size)} · ${lines} lines`;
}

function setText(kind, side, text, hint = "") {
  const p = KINDS[kind].p;
  $(`${p}text-${side}`).value = text;
  state.hints[kind][side] = hint;
  $(`${p}fmt-${side}`).value = "";
  updateDetected(kind, side);
}

function readFile(kind, side, file) {
  const reader = new FileReader();
  reader.onload = () => { setText(kind, side, String(reader.result), file.name); compute(); };
  reader.readAsText(file);
}

// Computing runs on the page's own thread, so big inputs are not recomputed on every
// keystroke: past this many characters the result waits for Compare or Ctrl+Enter.
const LIVE_LIMIT = 1_000_000;

const scheduleLive = debounce(() => {
  const records = state.mode === "records";
  if (!$(records ? "r-live" : "live").checked) return;
  const p = records ? "r-" : "";
  const size = $(`${p}text-old`).value.length + $(`${p}text-new`).value.length;
  const note = $("live-note");
  note.hidden = size <= LIVE_LIMIT;
  if (size > LIVE_LIMIT) {
    note.textContent = `${fmtBytes(size)} of input: not recomputing as you type. Press Compare, or Ctrl+Enter, to run.`;
    return;
  }
  compute();
}, 350);

for (const kind of Object.keys(KINDS)) {
  const p = KINDS[kind].p;
  for (const side of ["old", "new"]) {
    $(`${p}file-${side}`).addEventListener("change", (e) => e.target.files[0] && readFile(kind, side, e.target.files[0]));
    $(`${p}text-${side}`).addEventListener("input", () => { state.hints[kind][side] = ""; updateDetected(kind, side); scheduleLive(); });
    $(`${p}fmt-${side}`).addEventListener("change", () => { updateDetected(kind, side); compute(); });
    const pane = $(`${p}pane-${side}`);
    pane.addEventListener("dragover", (e) => { e.preventDefault(); pane.classList.add("drag"); });
    pane.addEventListener("dragleave", () => pane.classList.remove("drag"));
    pane.addEventListener("drop", (e) => {
      e.preventDefault();
      pane.classList.remove("drag");
      if (e.dataTransfer.files[0]) readFile(kind, side, e.dataTransfer.files[0]);
    });
  }
  $(`${p}run`).addEventListener("click", compute);
  $(`${p}swap`).addEventListener("click", () => {
    const [o, n, ho, hn] = [$(`${p}text-old`).value, $(`${p}text-new`).value, state.hints[kind].old, state.hints[kind].new];
    setText(kind, "old", n, hn); setText(kind, "new", o, ho); compute();
  });
  $(`${p}clear`).addEventListener("click", () => {
    setText(kind, "old", ""); setText(kind, "new", "");
    state.reports[kind] = null; state.report = null;
    $("results").hidden = true; $("error").hidden = true;
    $(`${p}example`).value = "";
  });
}
for (const id of ["array-keys", "ignore-paths", "r-key", "r-ignore"]) $(id).addEventListener("input", () => scheduleLive());
for (const id of ["r-dups", "r-keyless"]) $(id).addEventListener("change", compute);

const select = $("example");
EXAMPLES.forEach((ex, i) => select.append(new Option(ex.name, String(i))));
select.addEventListener("change", () => {
  const ex = EXAMPLES[Number(select.value)];
  if (!ex) return;
  setText("diff", "old", ex.old.text, ex.old.hint);
  setText("diff", "new", ex.new.text, ex.new.hint);
  $("array-keys").value = ex.keys ?? "";
  $("ignore-paths").value = ex.ignore ?? "";
  compute();
});

const recordSelect = $("r-example");
RECORD_EXAMPLES.forEach((ex, i) => recordSelect.append(new Option(ex.name, String(i))));
recordSelect.addEventListener("change", () => {
  const ex = RECORD_EXAMPLES[Number(recordSelect.value)];
  if (!ex) return;
  setText("records", "old", ex.old, ex.hint);
  setText("records", "new", ex.new, ex.hint);
  $("r-key").value = ex.key; $("r-keyless").checked = !!ex.keyless;
  $("r-dups").value = ex.dups ?? ""; $("r-ignore").value = ex.ignore ?? "";
  compute();
});

document.addEventListener("keydown", (e) => {
  if ((e.ctrlKey || e.metaKey) && e.key === "Enter" && state.mode !== "patch") compute();
});

// ---------- compute ----------
function compute() {
  $("live-note").hidden = true;
  const kind = state.mode === "records" ? "records" : "diff";
  const p = KINDS[kind].p;
  const oldText = $(`${p}text-old`).value, newText = $(`${p}text-new`).value;
  if (!oldText.trim() && !newText.trim()) return;
  const started = performance.now();
  let result;
  try {
    if (kind === "records") {
      const old = KINDS.records.detect(oldText, hintFor(kind, "old")), neu = KINDS.records.detect(newText, hintFor(kind, "new"));
      // A side with no text takes the other's format.
      const formats = [oldText.trim() ? old : null, newText.trim() ? neu : null].filter(Boolean);
      if (formats.length === 2 && formats[0] !== formats[1]) {
        showError(`The two files are different formats (old: ${formats[0]}, new: ${formats[1]}). Choose the same format for both.`);
        return;
      }
      result = JSON.parse(run_records(oldText, newText, formats[0], $("r-key").value, $("r-dups").value, $("r-keyless").checked, $("r-ignore").value));
    } else {
      result = JSON.parse(run_diff(oldText, newText, hintFor(kind, "old"), hintFor(kind, "new"), $("array-keys").value, $("ignore-paths").value));
    }
  } catch (err) {
    showError(`Unexpected failure: ${err}`);
    return;
  }
  const roundTrip = performance.now() - started;
  if (!result.ok) {
    showError(kind === "records" ? result.error : `Could not parse the ${result.side} document.\n${result.error}`);
    return;
  }
  $("error").hidden = true;

  const renderStart = performance.now();
  state.report = state.reports[kind] = result;
  render();
  result.metrics.render_ms = performance.now() - renderStart;
  result.metrics.roundtrip_ms = roundTrip;
  renderMetrics(result.metrics);
}

// The results below belong to the mode that made them; switching modes shows that mode's.
function onMode(name) {
  state.mode = name;
  const kind = name === "records" ? "records" : "diff";
  $("shared").hidden = name === "patch";
  $("live-note").hidden = true;
  $("error").hidden = true;
  state.report = state.reports[kind];
  if (state.report) { render(); renderMetrics(state.report.metrics); }
  else $("results").hidden = true;
}

function showError(message) {
  $("error").textContent = message;
  $("error").hidden = false;
  $("results").hidden = true;
}

// ---------- metrics ----------
function metric(label, value, sub, cls = "") {
  const card = el("div", `metric ${cls}`);
  card.append(el("div", "label", label));
  const v = el("div", "value");
  if (Array.isArray(value)) { v.append(value[0]); v.append(el("small", "", ` ${value[1]}`)); } else v.append(value);
  card.append(v);
  if (sub) card.append(el("div", "sub", sub));
  return card;
}

function renderMetrics(m) {
  const box = $("metrics");
  box.replaceChildren();
  box.append(metric("Diff time", fmtMs(m.diff_ms), `parse ${fmtMs(m.old.parse_ms + m.new.parse_ms)} · total ${fmtMs(m.total_ms)}`));
  const ops = metric("Operations", String(m.operations), Object.entries(m.by_op).map(([k, v]) => `${v} ${k}`).join(" · ") || "documents are identical");
  if (m.operations) {
    const bar = el("div", "bar");
    for (const [k, cls] of [["add", "a"], ["remove", "r"], ["replace", "c"]]) {
      if (m.by_op[k]) { const seg = el("i", cls); seg.style.width = `${(m.by_op[k] / m.operations) * 100}%`; bar.append(seg); }
    }
    ops.append(bar);
  }
  box.append(ops);
  box.append(metric("Input size", fmtBytes(m.old.bytes + m.new.bytes), `${fmtBytes(m.old.bytes)} old · ${fmtBytes(m.new.bytes)} new`));
  if (m.rows) {
    const { old: o, new: n, old_used: ou, new_used: nu } = m.rows;
    box.append(metric("Rows", `${ou} → ${nu}`, ou === o && nu === n ? `${o} old · ${n} new` : `${o} old, ${n} new, repeats left out`));
  }
  box.append(metric("Nodes", String(m.old.nodes + m.new.nodes), `${m.old.nodes} old · ${m.new.nodes} new`));
  box.append(metric("Depth", String(Math.max(m.old.depth, m.new.depth)), `${m.old.depth} old · ${m.new.depth} new`));
  const mbps = (m.old.bytes + m.new.bytes) / 1048576 / (m.diff_ms / 1000 || Infinity);
  box.append(metric("Throughput", Number.isFinite(mbps) ? [mbps.toFixed(1), "MB/s"] : "—", "diff step only"));
  box.append(metric("Formats", `${m.old.format} → ${m.new.format}`, m.old.format !== m.new.format ? "cross-format comparison" : "same format"));
  const ignoring = $(state.mode === "records" ? "r-ignore" : "ignore-paths").value.trim() ? " · ignored parts excluded" : "";
  box.append(metric("Patch check", m.roundtrip_ok ? "✓ verified" : "✗ failed", `apply in ${fmtMs(m.patch_ms)}${ignoring}`, m.roundtrip_ok ? "ok" : "bad"));
  box.append(metric("Render", fmtMs(m.render_ms), `round-trip to WASM ${fmtMs(m.roundtrip_ms)}`));
  box.append(metric("WASM module", fmtBytes(state.wasmBytes), `drift ${m.version}`));
}

// ---------- tabs ----------
function showTab(name) {
  for (const t of document.querySelectorAll(".tab")) {
    const on = t.dataset.tab === name;
    t.classList.toggle("active", on);
    t.setAttribute("aria-selected", String(on));
  }
  for (const id of ["visual", "ops", "raw"]) $(`tab-${id}`).hidden = id !== name;
}
document.querySelectorAll(".tab").forEach((t) => t.addEventListener("click", () => showTab(t.dataset.tab)));

function render() {
  const { tree, operations } = state.report;
  $("results").hidden = false;
  $("count-ops").textContent = String(operations.length);
  // A run of in-place replacements inside an array is the signature of a shifted insertion.
  const shifted = operations.filter((o) => o.op === "replace" && /\/\d+$/.test(o.path)).length;
  $("array-note").hidden = shifted < 2;
  const hasIgnored = (n) => n.status === "ignored" || (n.children ?? []).some(hasIgnored);
  $("legend-ignored").hidden = !hasIgnored(tree);
  renderVisual(tree);
  renderOps();
  renderRaw();
}

// ---------- visual diff ----------
const indent = (depth) => "  ".repeat(depth);

// Lines of a JSON value; the first line carries the key prefix.
function valueLines(value, key, depth, comma) {
  const body = JSON.stringify(value, null, 2).split("\n");
  const prefix = key === null ? "" : `${key}: `;
  const out = body.map((line, i) => indent(depth) + (i === 0 ? prefix + line : line));
  if (comma) out[out.length - 1] += ",";
  if (out.length > MAX_LINES_PER_VALUE) {
    const extra = out.length - MAX_LINES_PER_VALUE;
    return [...out.slice(0, MAX_LINES_PER_VALUE), `${indent(depth)}… ${extra} more lines`];
  }
  return out;
}

// `side` is "old" or "new": key-matched array items sit at different indices on each side.
function keyLabel(node, parentContainer, side) {
  if (node.key === null) return null;
  if (parentContainer !== "array") return JSON.stringify(node.key);
  if (side === "old") return `[${node.old_index ?? node.key}]`;
  return node.moved_from === undefined ? `[${node.key}]` : `[${node.key}] ↕moved`;
}

function cell(text, status, sign) {
  const c = el("div", "cell" + (status ? ` ${status}` : ""));
  if (text === null) { c.classList.add("empty"); c.textContent = " "; return c; }
  if (sign) c.append(el("span", "sign", sign));
  const m = /^(\s*)(\[\d+\]|"(?:[^"\\]|\\.)*"): (.*)$/s.exec(text);
  if (m && !text.includes("\n")) {
    c.append(document.createTextNode(m[1]));
    c.append(el("span", "k", m[2] + ": "));
    c.append(document.createTextNode(m[3]));
  } else c.append(document.createTextNode(text));
  return c;
}

function row(left, right) {
  const r = el("div", "row");
  r.append(left, right);
  return r;
}

function renderVisual(root) {
  const box = $("sbs");
  box.replaceChildren();
  const frag = document.createDocumentFragment();
  const budget = { rows: 0, truncated: false };
  const expandAll = $("show-same").checked;

  const push = (r) => {
    if (budget.rows >= MAX_ROWS) { budget.truncated = true; return; }
    budget.rows++; frag.append(r);
  };

  const sideBySide = (oldLines, newLines, oldStatus, newStatus, oldSign, newSign) => {
    const n = Math.max(oldLines.length, newLines.length);
    for (let i = 0; i < n; i++) {
      push(row(
        i < oldLines.length ? cell(oldLines[i], oldStatus, i === 0 ? oldSign : "") : cell(null),
        i < newLines.length ? cell(newLines[i], newStatus, i === 0 ? newSign : "") : cell(null),
      ));
    }
  };

  // Collapsed unchanged subtree: one row with a button to expand it in place.
  const sameRow = (node, depth, labels, comma) => {
    const lines = { old: valueLines(node.new, labels.old, depth, comma), new: valueLines(node.new, labels.new, depth, comma) };
    if (lines.new.length === 1 || expandAll) { sideBySide(lines.old, lines.new, "", "", "", ""); return; }
    const [open, close] = Array.isArray(node.new) ? ["[", "]"] : ["{", "}"];
    const count = Array.isArray(node.new) ? `${node.new.length} items` : `${Object.keys(node.new).length} keys`;
    const summary = (label) => `${indent(depth)}${label === null ? "" : label + ": "}${open} … ${count} ${close}${comma ? "," : ""}`;
    const r = el("div", "row");
    for (const side of ["old", "new"]) {
      const c = el("div", "cell");
      const btn = el("button", "fold", summary(labels[side]));
      btn.type = "button";
      btn.title = "Expand unchanged section";
      btn.addEventListener("click", () => {
        const holder = document.createDocumentFragment();
        lines.new.forEach((_, i) => holder.append(row(cell(lines.old[i], "", ""), cell(lines.new[i], "", ""))));
        r.replaceWith(holder);
      });
      c.append(btn);
      r.append(c);
    }
    push(r);
  };

  const walk = (node, depth, parentContainer, comma) => {
    const labels = { old: keyLabel(node, parentContainer, "old"), new: keyLabel(node, parentContainer, "new") };
    switch (node.status) {
      case "same": return sameRow(node, depth, labels, comma);
      case "added": return sideBySide([], valueLines(node.new, labels.new, depth, comma), "", "added", "", "+");
      case "removed": return sideBySide(valueLines(node.old, labels.old, depth, comma), [], "removed", "", "−", "");
      case "ignored": {
        // Shown for context but not part of the diff; either side may be missing.
        const oldLines = node.old === undefined ? [] : valueLines(node.old, labels.old, depth, comma);
        const newLines = node.new === undefined ? [] : valueLines(node.new, labels.new, depth, comma);
        return sideBySide(oldLines, newLines, "ignored", "ignored", "⊘", "⊘");
      }
      case "changed":
        return sideBySide(valueLines(node.old, labels.old, depth, comma), valueLines(node.new, labels.new, depth, comma), "changed", "changed", "~", "~");
      case "nested": {
        const [open, close] = node.container === "array" ? ["[", "]"] : ["{", "}"];
        const head = (label) => `${indent(depth)}${label === null ? "" : label + ": "}${open}`;
        sideBySide([head(labels.old)], [head(labels.new)], "", "", "", "");
        node.children.forEach((child, i) => walk(child, depth + 1, node.container, i < node.children.length - 1));
        const tail = `${indent(depth)}${close}${comma ? "," : ""}`;
        sideBySide([tail], [tail], "", "", "", "");
      }
    }
  };
  walk(root, 0, null, false);
  if (budget.truncated) {
    const note = el("div", "cell", `… output truncated after ${MAX_ROWS} rows. Use the Operations or Raw tab for the complete result.`);
    note.style.gridColumn = "1 / -1";
    const r = el("div", "row"); r.append(note); frag.append(r);
  }
  box.append(frag);
}
$("show-same").addEventListener("change", () => state.report && renderVisual(state.report.tree));

// ---------- operations table ----------
function renderFilters() {
  const box = $("op-filters");
  box.replaceChildren();
  for (const op of Object.keys(state.report.metrics.by_op)) {
    const b = el("button", "filter", `${op} ${state.report.metrics.by_op[op]}`);
    b.type = "button";
    b.setAttribute("aria-pressed", String(state.opFilter.has(op)));
    b.addEventListener("click", () => {
      state.opFilter.has(op) ? state.opFilter.delete(op) : state.opFilter.add(op);
      renderOps();
    });
    box.append(b);
  }
}

function renderOps() {
  renderFilters();
  const query = $("op-search").value.trim().toLowerCase();
  const body = $("ops-body");
  body.replaceChildren();
  let shown = 0;
  for (const op of state.report.operations) {
    if (state.opFilter.size && !state.opFilter.has(op.op)) continue;
    const valueText = "value" in op ? JSON.stringify(op.value) : "";
    if (query && !op.path.toLowerCase().includes(query) && !valueText.toLowerCase().includes(query)) continue;
    if (shown++ >= 2000) break;
    const tr = el("tr");
    tr.append(el("td", `op ${op.op}`, op.op), el("td", "", op.path || "(root)"));
    const td = el("td", "val", valueText.length > 300 ? valueText.slice(0, 300) + "…" : valueText);
    tr.append(td);
    body.append(tr);
  }
  if (!shown) {
    const tr = el("tr"); const td = el("td", "val", state.report.operations.length ? "No operations match the filter." : "No differences.");
    td.colSpan = 3; tr.append(td); body.append(tr);
  }
}
$("op-search").addEventListener("input", debounce(() => state.report && renderOps(), 120));

// ---------- raw ----------
function rawText() {
  const { operations, raw_pretty } = state.report;
  if (state.rawMode === "json") return raw_pretty;
  const sign = { add: "+", remove: "-", replace: "~", move: "m", copy: "c", test: "?" };
  return operations.map((o) => `${sign[o.op]} ${o.path}`).join("\n") || "(no differences)";
}
function renderRaw() { $("raw").textContent = rawText(); }
document.querySelectorAll(".seg-btn").forEach((b) => b.addEventListener("click", () => {
  document.querySelectorAll(".seg-btn").forEach((x) => x.classList.toggle("active", x === b));
  state.rawMode = b.dataset.raw;
  renderRaw();
}));
$("copy").addEventListener("click", async () => {
  try { await navigator.clipboard.writeText(rawText()); $("copy").textContent = "Copied"; }
  catch { $("copy").textContent = "Copy failed"; }
  setTimeout(() => ($("copy").textContent = "Copy"), 1200);
});
$("download").addEventListener("click", () => {
  const blob = new Blob([rawText()], { type: state.rawMode === "json" ? "application/json" : "text/plain" });
  const a = el("a");
  a.href = URL.createObjectURL(blob);
  a.download = state.rawMode === "json" ? "patch.json" : "patch.txt";
  a.click();
  URL.revokeObjectURL(a.href);
});

// ---------- boot ----------
async function boot() {
  try {
    const response = await fetch("./pkg/drift_wasm_bg.wasm");
    const bytes = await response.arrayBuffer();
    state.wasmBytes = bytes.byteLength;
    await init({ module_or_path: bytes });
    $("version").textContent = `v${version()}`;
    initModes(wasm, state, onMode);
    $("loading").hidden = true;
    const wanted = new URLSearchParams(location.search).get("example");
    select.value = wanted !== null && EXAMPLES[Number(wanted)] ? wanted : "0";
    // Start every tab with something to look at, and show the one in the address.
    recordSelect.value = "0";
    const current = state.mode;
    state.mode = "records"; recordSelect.dispatchEvent(new Event("change"));
    state.mode = "diff"; select.dispatchEvent(new Event("change"));
    state.mode = current;
    onMode(current);
  } catch (err) {
    $("loading").hidden = true;
    showError(`Failed to load WebAssembly: ${err}`);
  }
}
boot();
