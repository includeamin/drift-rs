# Three-way merge Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `drift::merge` / `merge_with` and a `drift merge BASE OURS THEIRS` command that structurally merges two edited versions of a document.

**Architecture:** A new `src/merge.rs` walks base/ours/theirs value trees together (rule table in the spec), collecting conflicts. Keyed arrays are merged per item by identity. The CLI loads the three files, calls the library, and writes the result with `dump_like` against the base text.

**Tech Stack:** Rust 1.77 MSRV, serde_json, clap (existing); proptest and tempfile (already dev-dependencies).

**Spec:** [docs/superpowers/specs/2026-11-22-three-way-merge-design.md](../specs/2026-11-22-three-way-merge-design.md)

## Global Constraints

- MSRV `rust-version = "1.77"`; no new non-dev dependencies.
- `MergeOptions` is `#[non_exhaustive]`, built with `MergeOptions::new()` and methods (like `DiffOptions`).
- Clean merges commute: `merge(b, x, y)` equals `merge(b, y, x)` whenever both are `Clean`.
- Conflicts exit code 1 with `{"conflicts":[...]}` on stdout and nothing written to the destination; real errors go to stderr, also exit 1.
- Conflict `path` is a JSON Pointer; an absent value is omitted from the conflict JSON (not `null`).
- Positional (key-less) arrays that both sides changed differently are one conflict on the whole array.
- `cargo fmt`, `cargo clippy --workspace --all-targets --all-features -- -D warnings` and `cargo test --workspace` stay green. Commit messages are conventional (`feat:`, `test:`, `docs:`) with no mention of Claude.

## Review Focus

- Both sides add the same object key with different nested content: recurse with an empty-object base, conflict only on the differing leaves.
- Both sides add the same object key with identical values: clean, not a conflict.
- Root-level type change on one side (object becomes array) while the other edits inside it: conflict reported with path `""` (the empty pointer), not a panic.
- Keyed array where one side deletes an item the other modified: conflict at that item's path, other items still merge.
- Both sides insert items at the same position in a keyed array: both kept, order identical regardless of argument order.
- `--prefer` where the preferred side deleted the key: the key is removed from the result.
- Keys containing `/` or `~` in conflict paths are escaped (`~1`, `~0`).

---

### Task 1: Core merge over objects and values

**Files:**
- Create: `src/merge.rs`
- Modify: `src/lib.rs` (add `mod merge;` and `pub use merge::{merge, merge_with, Conflict, MergeOptions, Merged, Side};`)

**Interfaces:**
- Produces (used by Tasks 2-4):
  - `pub enum Merged { Clean(Value), Conflicted(Vec<Conflict>) }`
  - `#[derive(Debug, Clone, PartialEq, Serialize)] pub struct Conflict { pub path: String, pub base: Option<Value>, pub ours: Option<Value>, pub theirs: Option<Value> }`, with `Option` fields skipped when `None` during serialization.
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum Side { Ours, Theirs }`
  - `#[derive(Debug, Clone, Default)] #[non_exhaustive] pub struct MergeOptions { pub array_keys: Vec<String>, pub prefer: Option<Side> }` with `new()`, `array_key(impl Into<String>) -> Self`, `prefer(Side) -> Self`.
  - `pub fn merge(base: &Value, ours: &Value, theirs: &Value) -> Merged`
  - `pub fn merge_with(base: &Value, ours: &Value, theirs: &Value, options: &MergeOptions) -> Merged`
  - Internal `fn node(base: Option<&Value>, ours: Option<&Value>, theirs: Option<&Value>, path: &str, ctx: &mut Ctx) -> Option<Value>` where `Ctx` holds the options and the `Vec<Conflict>`; `None` result means "absent".

- [ ] **Step 1: Write failing unit tests in `src/merge.rs` (`#[cfg(test)] mod tests`)** using `serde_json::json!`. Each asserts `merge(&base, &ours, &theirs)` equals `Merged::Clean(..)` or the exact `Conflicted(vec![Conflict{..}])`:
  - `identical_sides_win`: ours == theirs != base → that value.
  - `unchanged_side_yields_other`: ours == base → theirs; theirs == base → ours.
  - `disjoint_keys_merge`: base `{"a":1,"b":1}`, ours `{"a":2,"b":1}`, theirs `{"a":1,"b":3}` → `{"a":2,"b":3}`.
  - `same_leaf_different_values_conflicts`: `{"a":1}` / `{"a":2}` / `{"a":3}` → one conflict, path `/a`, base/ours/theirs `Some(1/2/3)`.
  - `delete_vs_modify_conflicts`: base `{"a":1}`, ours `{}`, theirs `{"a":2}` → conflict `/a` with `ours: None`.
  - `both_delete_is_clean`.
  - `both_add_same_key_recurses`: base `{}`, ours `{"k":{"x":1}}`, theirs `{"k":{"y":2}}` → `{"k":{"x":1,"y":2}}`; and identical additions are clean.
  - `object_key_order`: result keys are base order, then ours' new keys, then theirs' new keys.
  - `type_change_conflicts_at_root`: base `{"a":1}`, ours `[1]`, theirs `{"a":2}` → conflict with path `""`.
  - `pointer_escaping`: key `"a/b~"` conflicts at `/a~1b~0`.
  - `prefer_resolves_every_conflict`: with `.prefer(Side::Theirs)` the `delete_vs_modify` and leaf cases return `Clean` with theirs' value; with `Side::Ours` the deleted key is absent from the result.
  - `unkeyed_arrays_changed_by_both_conflict_whole_array`: base `[1]`, ours `[1,2]`, theirs `[0,1]` nested at `/l` → conflict path `/l`.

- [ ] **Step 2: Run** `cargo test --lib merge::` — Expected: FAIL (module/functions missing).

- [ ] **Step 3: Implement** `merge.rs` following the spec's rule table. Equality checks first (`ours == theirs`, then `ours == base`, then `theirs == base`, all on `Option<&Value>`), then both-objects recursion over the key union in the spec's order (base keys, ours-new, theirs-new; base treated as an empty map when absent or not an object), otherwise record a `Conflict` (or, with `prefer`, return the preferred side's `Option<Value>`). Build child paths with `crate::join_pointer`. `merge_with` returns `Clean` when no conflicts were collected; with a conflict at the root and `prefer` set, return the preferred root. Arrays fall to the conflict branch for now.

- [ ] **Step 4: Run** `cargo test --lib merge::` — Expected: PASS. Then `cargo clippy --all-targets --all-features -- -D warnings`.

- [ ] **Step 5: Commit** `feat: add three-way merge for objects and values`

---

### Task 2: Keyed array merge

**Files:**
- Modify: `src/merge.rs`

**Interfaces:**
- Consumes: Task 1's `node`, `Ctx`, `MergeOptions::array_keys`; `crate::diff::identity(item: &Value, key: &str) -> Option<String>` (pub(crate)).
- Produces: `fn keyed_array(base: &[Value], ours: &[Value], theirs: &[Value], key: &str, path: &str, ctx: &mut Ctx) -> Option<Value>` and `fn usable_key<'a>(arrays: [&[Value]; 3], keys: &'a [String]) -> Option<&'a str>` (the first key for which every item of all three arrays has an identity and identities are unique within each array; empty arrays qualify, unlike `match_array_items`, so a side that added items to an empty base is mergeable).

- [ ] **Step 1: Write failing tests** (same style as Task 1, options `MergeOptions::new().array_key("id")`, items like `{"id":1,"v":..}`):
  - `keyed_disjoint_item_edits_merge`.
  - `keyed_inserts_on_both_sides_keep_both`: base `[1,2]`, ours adds 3 at end, theirs adds 0 at front → ids `[0,1,2,3]`.
  - `keyed_same_anchor_inserts_commute`: both insert after id 1; asserts `merge(b,o,t) == merge(b,t,o)` and both items present.
  - `keyed_delete_vs_modify_conflicts_at_item`: the conflict path uses the item's index in the base array (assert `/items/1`).
  - `keyed_both_delete_is_clean` and `keyed_one_side_deletes_unchanged_item`.
  - `keyed_one_side_reorders`: ours reorders, theirs edits an item → ours' order with theirs' edit.
  - `keyed_both_reorder_differently_conflicts_on_array`: conflict at the array path with the three whole arrays.
  - `keyed_falls_back_when_key_not_unique`: duplicate ids → whole-array conflict.

- [ ] **Step 2: Run** `cargo test --lib merge::keyed` — Expected: FAIL.

- [ ] **Step 3: Implement** both functions and call them from `node` when all three values are arrays and `usable_key` finds a key. Algorithm: merge each identity present in any array with `node` (absent side = `None`), deleted result drops the item. Ordering per the spec: a side "reordered" if its ids restricted to ids also in base differ in order from base's ids restricted to its own; both reordered differently → conflict on the array; otherwise the skeleton is the reordered side (ours if neither). Items missing from the skeleton but added by the other side are inserted after their nearest preceding item (in that side's array) that is in the result, or at the end when none; several additions at one anchor are ordered by comparing the identity string of each side's first added item (so the result is argument-order independent). Conflict paths for items use the item's index in the base array when present, else its index in the side that has it.

- [ ] **Step 4: Run** `cargo test --lib merge::` — Expected: PASS.

- [ ] **Step 5: Commit** `feat: merge arrays matched by key`

---

### Task 3: Property tests

**Files:**
- Create: `tests/merge.rs`

**Interfaces:** Consumes `drift::{merge, merge_with, diff, patch, MergeOptions, Merged}`. Reuse the `arb_value` and `arb_keyed` strategies from `tests/roundtrip.rs` by copying them (integration test files cannot share modules without a `common` dir; keep it a copy, and note that in a comment).

- [ ] **Step 1: Write** proptest properties (512 cases):
  - `merge(b, x, b) == Clean(x)` and `merge(b, b, x) == Clean(x)`.
  - `merge(b, x, x) == Clean(x)`.
  - `clean_merges_commute`: if both orders are `Clean`, they are equal; and one is `Clean` iff the other is.
  - `prefer_is_always_clean` for `Side::Ours` and `Side::Theirs`.
  - `disjoint_edits_equal_both_patches`: base an object with keys `a..d` holding arbitrary values; ours edits only `a`/`b`, theirs only `c`/`d`; result equals applying `diff` of both to base in sequence.
  - `keyed_merge_commutes` using `arb_keyed` documents and `array_key("id")`.

- [ ] **Step 2: Run** `cargo test --test merge` — Expected: PASS. If a property fails, the counterexample is a real bug in Tasks 1-2: fix it there (add a regression unit test), not by weakening the property.

- [ ] **Step 3: Commit** `test: add property tests for three-way merge`

---

### Task 4: `drift merge` command

**Files:**
- Modify: `src/cli/mod.rs` (add `Merge(MergeArgs)` to `Command`, `MergeArgs`, `cmd_merge`, a match arm in `run`)
- Modify: `tests/cli.rs`

**Interfaces:**
- Consumes: `drift::{merge_with, MergeOptions, Merged, Side}`, existing `resolve`, `read`, `write_output`, `formats::parse_with`/`dump_like`.
- Produces: `struct MergeArgs { base: String, ours: String, theirs: String, #[arg(long="array-key")] array_keys: Vec<String>, #[arg(long, value_enum)] prefer: Option<PreferArg>, #[arg(long, value_enum)] format: Option<Format>, #[arg(long)] xml_arrays: bool, #[command(flatten)] output: OutputArgs }`, `enum PreferArg { Ours, Theirs }` (clap `ValueEnum`), and `fn cmd_merge(args: MergeArgs) -> Result<i32, Box<dyn std::error::Error>>`.

- [ ] **Step 1: Write failing tests in `tests/cli.rs`:**
  - `merge_clean_prints_document_exit_zero`: JSON base `{"a":1,"b":1}`, ours changes `a`, theirs changes `b`; stdout JSON equals `{"a":2,"b":3}`, code 0.
  - `merge_conflict_exits_one_with_report`: both change `a` differently; code 1; stdout JSON `{"conflicts":[{"path":"/a","base":1,"ours":2,"theirs":3}]}`; with `-o FILE` the file is not created.
  - `merge_prefer_theirs_resolves`: same inputs plus `--prefer theirs` → code 0, `a == 3`.
  - `merge_keeps_yaml_comments`: base `# top\na: 1 # one\nb: 1\n`, ours `a: 2`, theirs `b: 2` (comments in base only) → output keeps `# top` and `# one`, with `a: 2`, `b: 2`.
  - `merge_array_key_merges_arrays`: keyed inserts on both sides merge cleanly with `--array-key id`; without it, exit 1 and the conflict path is the array.
  - `merge_missing_file_is_an_error`: nonzero exit, stderr non-empty, stdout has no `conflicts`.

- [ ] **Step 2: Run** `cargo test --test cli merge` — Expected: FAIL (unknown subcommand).

- [ ] **Step 3: Implement** `cmd_merge`: format from `resolve(args.format, &args.base)`; parse all three with `parse_with` and `xml_arrays`; keep the base text for `dump_like`; build `MergeOptions` from `--array-key` and `--prefer`. On `Clean(v)` write `dump_like(&base_text, &v, format, compact)` to `-o` or stdout and return 0. On `Conflicted(c)` print `serde_json::json!({"conflicts": c})` (pretty unless `--compact`) to stdout via `write_output(.., &None)`, never touching `-o`, and return 1. Add `merge` to the doc comment style used by the other subcommands.

- [ ] **Step 4: Run** `cargo test --workspace` and `cargo clippy --workspace --all-targets --all-features -- -D warnings`; Expected: all pass. Manually run `cargo run -- merge --help` and confirm all flags show.

- [ ] **Step 5: Commit** `feat: add drift merge command`

---

### Task 5: Documentation

**Files:**
- Modify: `README.md` (CLI list, a "Merging" section after "Patches as data, undo and squash"), `src/lib.rs` crate docs if they list the API.

- [ ] **Step 1: Add** `drift merge BASE OURS THEIRS` to the CLI synopsis and a README section covering: the rule in one paragraph, conflicts and exit code with the JSON example, `--prefer`, `--array-key` (and that key-less arrays conflict), YAML/TOML comments kept, what is not supported (`--ignore-path`, record files, streaming). Include a doctest example on `merge` in `src/merge.rs` that asserts a clean merge and a conflict.

- [ ] **Step 2: Run** `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` and `cargo test --doc` — Expected: pass.

- [ ] **Step 3: Commit** `docs: document drift merge`

---

## Self-Review

- **Spec coverage:** rule table, delete-vs-modify, key order, keyed arrays and ordering, conflict struct, `prefer`, library API, CLI behaviour (exit codes, `-o`, comments via `dump_like`, stdin through `read`), property tests and CLI tests are each owned by a task. Non-goals need no task.
- **Spec call changed:** the spec said additions with no predecessor go at the end; commutativity of same-anchor inserts needs an explicit tie-break, which Task 2 adds (ordered by first added identity). Mention this when handing over.
- **Types:** `Merged`, `Conflict`, `Side`, `MergeOptions` are defined in Task 1 and used unchanged in Tasks 2-4.
- **MSRV:** only std/serde_json APIs already used elsewhere; run `scripts/check-msrv.sh` once before the last commit.
