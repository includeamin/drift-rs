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

export const RECORD_EXAMPLES = [
  {
    name: "NDJSON · a row added at the front",
    hint: "ndjson", key: "id",
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
    hint: "csv", key: "sku",
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
    hint: "ndjson", key: "id,region",
    old: `{"id":1,"region":"eu","stock":5}
{"id":1,"region":"us","stock":9}
`,
    new: `{"id":1,"region":"us","stock":9}
{"id":1,"region":"eu","stock":4}
`,
  },
  {
    name: "NDJSON · keyless, rows matched by content",
    hint: "ndjson", key: "", keyless: true,
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
    hint: "ndjson", key: "id", dups: "last",
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

export function initModes(wasm, diffState, onMode) {
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
    onMode(name);
  }
  document.querySelectorAll(".mode").forEach((b) => b.addEventListener("click", () => showMode(b.dataset.mode)));

  const copyButton = (button, getText) => button.addEventListener("click", async () => {
    try { await navigator.clipboard.writeText(getText()); button.textContent = "Copied"; }
    catch { button.textContent = "Copy failed"; }
    setTimeout(() => (button.textContent = "Copy"), 1200);
  });

  const fmtBytes = (n) => (n < 1024 ? `${n} B` : n < 1048576 ? `${(n / 1024).toFixed(1)} KB` : `${(n / 1048576).toFixed(2)} MB`);

  // ---- apply, undo, squash ----
  const pt = { last: "apply", hint: "", output: null };

  function ptError(message) {
    $("pt-error").textContent = message;
    $("pt-error").hidden = false;
    $("pt-results").hidden = true;
  }

  function ptFooters() {
    const doc = $("pt-doc").value;
    $("pt-detected").textContent = doc.trim() ? `detected: ${wasm.detect_format(doc, $("pt-format").value || pt.hint)}` : "";
    for (const [id, text] of [["pt-foot-doc", doc], ["pt-foot-patch", $("pt-patch").value]]) {
      $(id).textContent = text.trim() ? `${fmtBytes(new Blob([text]).size)} · ${text.split("\n").length} lines` : "";
    }
  }

  function ptRun(kind) {
    pt.last = kind;
    const doc = $("pt-doc").value, patch = $("pt-patch").value, hint = $("pt-format").value || pt.hint;
    if (!doc.trim() && !patch.trim()) return;
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
    for (const [id, name] of [["pt-apply", "apply"], ["pt-invert", "invert"], ["pt-compose", "compose"]]) $(id).classList.toggle("primary", name === kind);
    const note = $("pt-note");
    note.className = "muted";
    if (kind === "apply") {
      pt.output = { text: result.text, name: `patched.${result.format}`, type: "text/plain" };
      $("pt-title").textContent = `Patched ${result.format} document`;
      if (result.kept) { note.textContent = "Comments and layout of the original were kept."; note.className = "note-ok"; }
      else if (result.preserving) note.textContent = "Nothing in this document needed keeping, or it could not be edited in place, so it was written fresh.";
      else note.textContent = `${result.format.toUpperCase()} has no comments to keep: the document was written fresh.`;
    } else {
      const plural = (n) => `${n} operation${n === 1 ? "" : "s"}`;
      pt.output = { text: result.patch, name: kind === "invert" ? "undo.json" : "squashed.json", type: "application/json" };
      $("pt-title").textContent = kind === "invert" ? "The patch that undoes it" : "One patch with the same effect";
      note.textContent = kind === "invert"
        ? `${plural(result.before)} → ${result.after} to undo them. Apply this to the patched document to get the original back.`
        : `${plural(result.before)} squashed into ${result.after}.`;
    }
    $("pt-out").textContent = pt.output.text;
  }

  $("pt-apply").addEventListener("click", () => ptRun("apply"));
  $("pt-invert").addEventListener("click", () => ptRun("invert"));
  $("pt-compose").addEventListener("click", () => ptRun("compose"));
  copyButton($("pt-copy"), () => $("pt-out").textContent);
  $("pt-download").addEventListener("click", () => {
    if (!pt.output) return;
    const a = el("a");
    a.href = URL.createObjectURL(new Blob([pt.output.text], { type: pt.output.type }));
    a.download = pt.output.name;
    a.click();
    URL.revokeObjectURL(a.href);
  });

  // Typing, dropping or uploading runs the last action again, as in the other tabs.
  const live = debounce(() => { ptFooters(); if ($("pt-live").checked) ptRun(pt.last); }, 350);
  const load = (id, file, isDoc) => {
    const reader = new FileReader();
    reader.onload = () => {
      $(id).value = String(reader.result);
      if (isDoc) { pt.hint = file.name; $("pt-format").value = ""; }
      ptFooters(); ptRun(pt.last);
    };
    reader.readAsText(file);
  };
  for (const [paneId, areaId, fileId, isDoc] of [["pt-pane-doc", "pt-doc", "pt-file-doc", true], ["pt-pane-patch", "pt-patch", "pt-file-patch", false]]) {
    $(fileId).addEventListener("change", (e) => e.target.files[0] && load(areaId, e.target.files[0], isDoc));
    $(areaId).addEventListener("input", () => { if (isDoc) pt.hint = ""; live(); });
    const pane = $(paneId);
    pane.addEventListener("dragover", (e) => { e.preventDefault(); pane.classList.add("drag"); });
    pane.addEventListener("dragleave", () => pane.classList.remove("drag"));
    pane.addEventListener("drop", (e) => {
      e.preventDefault();
      pane.classList.remove("drag");
      if (e.dataTransfer.files[0]) load(areaId, e.dataTransfer.files[0], isDoc);
    });
  }
  $("pt-format").addEventListener("change", () => { ptFooters(); ptRun(pt.last); });
  for (const id of ["pt-keys", "pt-ignore"]) $(id).addEventListener("input", live);
  $("pt-clear").addEventListener("click", () => {
    for (const id of ["pt-doc", "pt-patch"]) $(id).value = "";
    pt.hint = ""; pt.output = null; ptFooters();
    $("pt-results").hidden = true; $("pt-error").hidden = true; $("pt-example").value = "";
  });
  document.addEventListener("keydown", (e) => {
    if ((e.ctrlKey || e.metaKey) && e.key === "Enter" && !$("mode-patch").hidden) ptRun(pt.last);
  });

  const ptSelect = $("pt-example");
  PATCH_EXAMPLES.forEach((ex, i) => ptSelect.append(new Option(ex.name, String(i))));
  ptSelect.addEventListener("change", () => {
    const ex = PATCH_EXAMPLES[Number(ptSelect.value)];
    if (!ex) return;
    $("pt-doc").value = ex.doc; $("pt-patch").value = ex.patch; $("pt-format").value = ex.format;
    $("pt-keys").value = ""; $("pt-ignore").value = ""; pt.hint = "";
    ptFooters(); ptRun(ex.run);
  });

  $("pt-from-diff").addEventListener("click", () => {
    const report = diffState.reports.diff;
    if (!report) { ptError("Compute a diff in Compare documents first."); return; }
    $("pt-doc").value = $("text-old").value;
    $("pt-format").value = ""; pt.hint = diffState.hints.diff.old;
    $("pt-patch").value = report.raw_pretty;
    ptFooters(); ptRun("apply");
  });

  const wanted = location.hash.slice(1);
  showMode(modes.includes(wanted) ? wanted : "diff");
  return { showMode };
}
