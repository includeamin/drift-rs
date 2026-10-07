import init, { run_diff, detect_format, version } from "./pkg/drift_wasm.js";
import { EXAMPLES } from "./examples.js";

const $ = (id) => document.getElementById(id);
const MAX_LINES_PER_VALUE = 400;
const MAX_ROWS = 6000;

const state = { report: null, hints: { old: "", new: "" }, rawMode: "json", opFilter: new Set(), wasmBytes: 0 };

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
function hintFor(side) {
  return $(`fmt-${side}`).value || state.hints[side] || "";
}

function updateDetected(side) {
  const text = $(`text-${side}`).value;
  const label = $(`detected-${side}`);
  if (!text.trim()) { label.textContent = ""; $(`foot-${side}`).textContent = ""; return; }
  label.textContent = `detected: ${detect_format(text, hintFor(side))}`;
  const lines = text.split("\n").length;
  $(`foot-${side}`).textContent = `${fmtBytes(new Blob([text]).size)} · ${lines} lines`;
}

function setText(side, text, hint = "") {
  $(`text-${side}`).value = text;
  state.hints[side] = hint;
  $(`fmt-${side}`).value = "";
  updateDetected(side);
}

function readFile(side, file) {
  const reader = new FileReader();
  reader.onload = () => { setText(side, String(reader.result), file.name); compute(); };
  reader.readAsText(file);
}

for (const side of ["old", "new"]) {
  $(`file-${side}`).addEventListener("change", (e) => e.target.files[0] && readFile(side, e.target.files[0]));
  $(`text-${side}`).addEventListener("input", () => { state.hints[side] = ""; updateDetected(side); scheduleLive(); });
  $(`fmt-${side}`).addEventListener("change", () => { updateDetected(side); compute(); });
  const pane = $(`pane-${side}`);
  pane.addEventListener("dragover", (e) => { e.preventDefault(); pane.classList.add("drag"); });
  pane.addEventListener("dragleave", () => pane.classList.remove("drag"));
  pane.addEventListener("drop", (e) => {
    e.preventDefault();
    pane.classList.remove("drag");
    if (e.dataTransfer.files[0]) readFile(side, e.dataTransfer.files[0]);
  });
}

$("array-keys").addEventListener("input", () => scheduleLive());

const scheduleLive = debounce(() => { if ($("live").checked) compute(); }, 350);

const select = $("example");
EXAMPLES.forEach((ex, i) => select.append(new Option(ex.name, String(i))));
select.addEventListener("change", () => {
  const ex = EXAMPLES[Number(select.value)];
  if (!ex) return;
  setText("old", ex.old.text, ex.old.hint);
  setText("new", ex.new.text, ex.new.hint);
  $("array-keys").value = ex.keys ?? "";
  compute();
});

$("run").addEventListener("click", compute);
$("swap").addEventListener("click", () => {
  const [o, n, ho, hn] = [$("text-old").value, $("text-new").value, state.hints.old, state.hints.new];
  setText("old", n, hn); setText("new", o, ho); compute();
});
$("clear").addEventListener("click", () => {
  setText("old", ""); setText("new", "");
  $("results").hidden = true; $("error").hidden = true; select.value = "";
});
document.addEventListener("keydown", (e) => { if ((e.ctrlKey || e.metaKey) && e.key === "Enter") compute(); });

// ---------- compute ----------
function compute() {
  const oldText = $("text-old").value, newText = $("text-new").value;
  if (!oldText.trim() && !newText.trim()) return;
  const started = performance.now();
  let result;
  try {
    result = JSON.parse(run_diff(oldText, newText, hintFor("old"), hintFor("new"), $("array-keys").value));
  } catch (err) {
    showError(`Unexpected failure: ${err}`);
    return;
  }
  const roundTrip = performance.now() - started;
  if (!result.ok) { showError(`Could not parse the ${result.side} document.\n${result.error}`); return; }
  $("error").hidden = true;

  const renderStart = performance.now();
  state.report = result;
  render();
  result.metrics.render_ms = performance.now() - renderStart;
  result.metrics.roundtrip_ms = roundTrip;
  renderMetrics(result.metrics);
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
  box.append(metric("Nodes", String(m.old.nodes + m.new.nodes), `${m.old.nodes} old · ${m.new.nodes} new`));
  box.append(metric("Depth", String(Math.max(m.old.depth, m.new.depth)), `${m.old.depth} old · ${m.new.depth} new`));
  const mbps = (m.old.bytes + m.new.bytes) / 1048576 / (m.diff_ms / 1000 || Infinity);
  box.append(metric("Throughput", Number.isFinite(mbps) ? [mbps.toFixed(1), "MB/s"] : "—", "diff step only"));
  box.append(metric("Formats", `${m.old.format} → ${m.new.format}`, m.old.format !== m.new.format ? "cross-format comparison" : "same format"));
  box.append(metric("Patch check", m.roundtrip_ok ? "✓ verified" : "✗ failed", `apply in ${fmtMs(m.patch_ms)}`, m.roundtrip_ok ? "ok" : "bad"));
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
    $("loading").hidden = true;
    const wanted = new URLSearchParams(location.search).get("example");
    select.value = wanted !== null && EXAMPLES[Number(wanted)] ? wanted : "0";
    select.dispatchEvent(new Event("change"));
  } catch (err) {
    $("loading").hidden = true;
    showError(`Failed to load WebAssembly: ${err}`);
  }
}
boot();
