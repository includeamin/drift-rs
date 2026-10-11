// The Compare records and Apply, undo & squash modes, and the mode switcher.
const $ = (id) => document.getElementById(id);
const el = (tag, cls, text) => {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text !== undefined) node.textContent = text;
  return node;
};
const fmtMs = (ms) => (ms < 0.001 ? "< 1 µs" : ms < 1 ? `${(ms * 1000).toFixed(0)} µs` : ms < 100 ? `${ms.toFixed(2)} ms` : `${ms.toFixed(0)} ms`);
const debounce = (fn, ms) => { let t; return (...a) => { clearTimeout(t); t = setTimeout(() => fn(...a), ms); }; };

const RECORD_EXAMPLES = [
  {
    name: "NDJSON · a row added at the front",
    format: "ndjson", key: "id",
    old: `{"id":1,"name":"Ada","plan":"free"}
{"id":2,"name":"Grace","plan":"pro"}
{"id":3,"name":"Linus","plan":"free"}
`,
    new: `{"id":0,"name":"Alan","plan":"free"}
{"id":1,"name":"Ada","plan":"pro"}
{"id":2,"name":"Grace","plan":"pro"}
{"id":3,"name":"Linus","plan":"free"}
`,
  },
  {
    name: "CSV · changed and removed rows",
    format: "csv", key: "sku",
    old: `sku,name,price
A-100,Widget,9.99
A-101,Gadget,14.50
A-102,Gizmo,3.25
`,
    new: `sku,name,price
A-100,Widget,10.99
A-102,Gizmo,3.25
A-103,Doohickey,7.00
`,
  },
  {
    name: "NDJSON · composite key (id + region)",
    format: "ndjson", key: "id,region",
    old: `{"id":1,"region":"eu","stock":5}
{"id":1,"region":"us","stock":9}
`,
    new: `{"id":1,"region":"us","stock":9}
{"id":1,"region":"eu","stock":4}
`,
  },
  {
    name: "NDJSON · keyless, rows matched by content",
    format: "ndjson", key: "", keyless: true,
    old: `{"event":"login","user":"ada"}
{"event":"login","user":"ada"}
{"event":"logout","user":"ada"}
`,
    new: `{"event":"login","user":"ada"}
{"event":"logout","user":"ada"}
{"event":"login","user":"grace"}
`,
  },
  {
    name: "NDJSON · repeated keys (keep the last)",
    format: "ndjson", key: "id", dups: "last",
    old: `{"id":1,"v":"a"}
{"id":1,"v":"b"}
{"id":2,"v":"c"}
`,
    new: `{"id":1,"v":"b"}
{"id":2,"v":"d"}
`,
  },
];

const PATCH_EXAMPLES = [
  {
    name: "YAML · comments survive an apply",
    format: "yaml",
    doc: `# web service
name: web  # public name
replicas: 2
image: web:1.4  # pinned
`,
    patch: `[
  { "op": "replace", "path": "/replicas", "value": 5 },
  { "op": "replace", "path": "/image", "value": "web:1.5" }
]`,
    run: "apply",
  },
  {
    name: "TOML · comments and layout survive an apply",
    format: "toml",
    doc: `# server settings
host = "localhost"  # bind address
port = 80           # http

[limits]
connections = 100
`,
    patch: `[
  { "op": "replace", "path": "/port", "value": 8080 },
  { "op": "add", "path": "/limits/timeout", "value": 30 }
]`,
    run: "apply",
  },
  {
    name: "JSON · undo a patch",
    format: "json",
    doc: `{
  "name": "Ada",
  "roles": ["admin", "editor"],
  "active": true
}`,
    patch: `[
  { "op": "replace", "path": "/name", "value": "Ada King" },
  { "op": "remove", "path": "/roles/0" },
  { "op": "add", "path": "/email", "value": "ada@example.com" }
]`,
    run: "invert",
  },
  {
    name: "JSON · squash three patches into one",
    format: "json",
    doc: `{ "version": 1, "tags": ["a"] }`,
    patch: `[
  [{ "op": "replace", "path": "/version", "value": 2 }],
  [{ "op": "add", "path": "/tags/-", "value": "b" }],
  [{ "op": "replace", "path": "/version", "value": 3 }, { "op": "remove", "path": "/tags/1" }]
]`,
    run: "compose",
  },
];

export function initModes(wasm, diffState) {
  // ---- switcher ----
  const modes = ["diff", "records", "patch"];
  function showMode(name) {
    for (const m of modes) $(`mode-${m}`).hidden = m !== name;
    for (const b of document.querySelectorAll(".mode")) {
      const on = b.dataset.mode === name;
      b.classList.toggle("active", on);
      b.setAttribute("aria-selected", String(on));
    }
    try { history.replaceState(null, "", name === "diff" ? location.pathname : `#${name}`); } catch { /* not allowed in some contexts */ }
  }
  document.querySelectorAll(".mode").forEach((b) => b.addEventListener("click", () => showMode(b.dataset.mode)));

  const copyButton = (button, getText) => button.addEventListener("click", async () => {
    try { await navigator.clipboard.writeText(getText()); button.textContent = "Copied"; }
    catch { button.textContent = "Copy failed"; }
    setTimeout(() => (button.textContent = "Copy"), 1200);
  });

  // ---- records ----
  const rec = { report: null, filter: new Set() };

  function recError(message) {
    $("rec-error").textContent = message;
    $("rec-error").hidden = false;
    $("rec-results").hidden = true;
  }

  function recCompare() {
    const oldText = $("rec-old").value, newText = $("rec-new").value;
    if (!oldText.trim() && !newText.trim()) return;
    let result;
    try {
      result = JSON.parse(wasm.run_records(oldText, newText, $("rec-format").value, $("rec-key").value, $("rec-dups").value, $("rec-keyless").checked, $("rec-ignore").value));
    } catch (err) { recError(`Unexpected failure: ${err}`); return; }
    if (!result.ok) { recError(result.error); return; }
    $("rec-error").hidden = true;
    rec.report = result;
    recRender();
  }

  function recMetric(label, value, sub) {
    const card = el("div", "metric");
    card.append(el("div", "label", label), el("div", "value", value));
    if (sub) card.append(el("div", "sub", sub));
    return card;
  }

  function recRender() {
    const r = rec.report;
    $("rec-results").hidden = false;
    const box = $("rec-metrics");
    box.replaceChildren();
    box.append(recMetric("Operations", String(r.operations.length), Object.entries(r.by_op).map(([k, v]) => `${v} ${k}`).join(" · ") || "the files hold the same rows"));
    const used = (n, total) => (n === total ? `${total} rows` : `${n} of ${total} rows used`);
    box.append(recMetric("Old file", used(r.old_used, r.old_rows), "rows after repeated keys are resolved"));
    box.append(recMetric("New file", used(r.new_used, r.new_rows)));
    box.append(recMetric("Diff time", fmtMs(r.diff_ms), `total ${fmtMs(r.total_ms)}`));

    const filters = $("rec-filters");
    filters.replaceChildren();
    for (const [op, count] of Object.entries(r.by_op)) {
      const b = el("button", "filter", `${op} ${count}`);
      b.type = "button";
      b.setAttribute("aria-pressed", String(rec.filter.has(op)));
      b.addEventListener("click", () => { rec.filter.has(op) ? rec.filter.delete(op) : rec.filter.add(op); recRender(); });
      filters.append(b);
    }
    const body = $("rec-body");
    body.replaceChildren();
    let shown = 0;
    for (const op of r.operations) {
      if (rec.filter.size && !rec.filter.has(op.op)) continue;
      if (shown++ >= 2000) break;
      const tr = el("tr");
      const text = "value" in op ? JSON.stringify(op.value) : op.from ? `from ${op.from}` : "";
      tr.append(el("td", `op ${op.op}`, op.op), el("td", "", op.path || "(root)"), el("td", "val", text.length > 300 ? text.slice(0, 300) + "…" : text));
      body.append(tr);
    }
    if (!shown) {
      const tr = el("tr"); const td = el("td", "val", r.operations.length ? "No operations match the filter." : "No differences.");
      td.colSpan = 3; tr.append(td); body.append(tr);
    }
  }

  const recSelect = $("rec-example");
  RECORD_EXAMPLES.forEach((ex, i) => recSelect.append(new Option(ex.name, String(i))));
  recSelect.addEventListener("change", () => {
    const ex = RECORD_EXAMPLES[Number(recSelect.value)];
    if (!ex) return;
    $("rec-old").value = ex.old; $("rec-new").value = ex.new;
    $("rec-format").value = ex.format; $("rec-key").value = ex.key;
    $("rec-keyless").checked = !!ex.keyless; $("rec-dups").value = ex.dups ?? ""; $("rec-ignore").value = "";
    recCompare();
  });
  $("rec-run").addEventListener("click", recCompare);
  const recLive = debounce(recCompare, 350);
  for (const id of ["rec-old", "rec-new", "rec-key", "rec-ignore"]) $(id).addEventListener("input", recLive);
  for (const id of ["rec-format", "rec-dups", "rec-keyless"]) $(id).addEventListener("change", recCompare);
  copyButton($("rec-copy"), () => rec.report?.raw_pretty ?? "");

  // ---- apply, undo, squash ----
  function ptError(message) {
    $("pt-error").textContent = message;
    $("pt-error").hidden = false;
    $("pt-results").hidden = true;
  }

  function ptRun(kind) {
    const doc = $("pt-doc").value, patch = $("pt-patch").value, hint = $("pt-format").value;
    if (!doc.trim() || !patch.trim()) { ptError("Paste a document and a patch first."); return; }
    let result;
    try {
      result = JSON.parse(
        kind === "apply" ? wasm.run_apply(doc, hint, patch)
        : kind === "invert" ? wasm.run_invert(doc, hint, patch)
        : wasm.run_compose(doc, hint, patch, $("pt-keys").value, $("pt-ignore").value));
    } catch (err) { ptError(`Unexpected failure: ${err}`); return; }
    if (!result.ok) { ptError(result.error); return; }
    $("pt-error").hidden = true;
    $("pt-results").hidden = false;
    const note = $("pt-note");
    note.className = "muted";
    if (kind === "apply") {
      $("pt-title").textContent = `Patched ${result.format} document`;
      $("pt-out").textContent = result.text;
      if (result.kept) { note.textContent = "Comments and layout of the original were kept."; note.className = "note-ok"; }
      else if (result.preserving) note.textContent = "Nothing in this document needed keeping, or it could not be edited in place, so it was written fresh.";
      else note.textContent = `${result.format.toUpperCase()} has no comments to keep: the document was written fresh.`;
    } else if (kind === "invert") {
      $("pt-title").textContent = "The patch that undoes it";
      $("pt-out").textContent = result.patch;
      note.textContent = `${result.before} operation${result.before === 1 ? "" : "s"} → ${result.after} to undo them. Apply this to the patched document to get the original back.`;
    } else {
      $("pt-title").textContent = "One patch with the same effect";
      $("pt-out").textContent = result.patch;
      note.textContent = `${result.before} operation${result.before === 1 ? "" : "s"} squashed into ${result.after}.`;
    }
  }
  $("pt-apply").addEventListener("click", () => ptRun("apply"));
  $("pt-invert").addEventListener("click", () => ptRun("invert"));
  $("pt-compose").addEventListener("click", () => ptRun("compose"));
  copyButton($("pt-copy"), () => $("pt-out").textContent);

  const ptSelect = $("pt-example");
  PATCH_EXAMPLES.forEach((ex, i) => ptSelect.append(new Option(ex.name, String(i))));
  ptSelect.addEventListener("change", () => {
    const ex = PATCH_EXAMPLES[Number(ptSelect.value)];
    if (!ex) return;
    $("pt-doc").value = ex.doc; $("pt-patch").value = ex.patch; $("pt-format").value = ex.format;
    $("pt-keys").value = ""; $("pt-ignore").value = "";
    ptRun(ex.run);
  });

  $("pt-from-diff").addEventListener("click", () => {
    const report = diffState.report;
    if (!report) { ptError("Compute a diff in Compare documents first."); return; }
    $("pt-doc").value = $("text-old").value;
    $("pt-format").value = "";
    $("pt-patch").value = report.raw_pretty;
    $("pt-error").hidden = true;
  });

  const wanted = location.hash.slice(1);
  showMode(modes.includes(wanted) ? wanted : "diff");
  return { showMode };
}
