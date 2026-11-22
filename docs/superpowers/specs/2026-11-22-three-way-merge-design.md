# Three-way merge: design

Status: draft, awaiting approval. Date: 2026-11-22.

## Intent and why

Combine two edited versions of one document (`ours`, `theirs`) that both
started from `base`, into one document, structurally rather than by lines:
`drift merge BASE OURS THEIRS`, plus a library function. drift already has
diff, patch, invert and compose, so merge is the natural next primitive for
config files edited on separate branches or by concurrent writers. The user
did not say which of those it is for, so the design favours general behaviour
over a specific workflow.

## Goals

- Clean merges when the two sides touch different parts of the document.
- Conflicts are detected, reported with enough data to resolve them, and never
  silently dropped.
- Works for every format drift reads (JSON, YAML, TOML, XML); YAML and TOML
  keep the base file's comments and layout where the result is written.
- Arrays are merged by item identity when `--array-key` is given.
- Same library/CLI shape as the rest of the crate (`DiffOptions` style builder,
  `#[non_exhaustive]`, exit codes like `diff --exit-code`).

## Non-goals

- Line-level or text merging inside strings (a string is one value).
- Git-style conflict markers in the output file (no place for them in JSON).
- Streaming or larger-than-memory merge; all three documents are loaded.
- A git merge-driver, three-way merge of record files (NDJSON/CSV), and a web
  demo tab. Each could follow later.

## Anti-goals

- Guessing silently. A merge that picks a winner without being asked is worse
  than one that reports a conflict.
- Order-dependent results: `merge(base, a, b)` must equal `merge(base, b, a)`
  whenever it is clean.

## Approaches considered

1. **Walk the three document trees together (chosen).** At each node compare
   `base`, `ours`, `theirs`: equal sides, or one side unchanged, resolve at
   once; both sides objects recurse per key; keyed arrays recurse per item;
   anything else is a conflict. Trades: simple, predictable, handles each path
   in isolation. Needs its own array-ordering rule.
2. **Diff both sides against base and combine the patches.** Reuses `diff` and
   `compose`, but RFC 6902 operations address arrays by index and use `move`,
   so two correct patches shift each other's paths and overlap detection
   becomes unreliable. Rejected.
3. **Shell out to `git merge-file` on the text.** No new logic, but it ignores
   structure, so reformatting or key reordering creates false conflicts, and it
   needs git installed. Rejected.

## Design

### Merge rule at a node (`Option<Value>`, `None` = absent)

| Situation | Result |
|---|---|
| `ours == theirs` | that value (including both deleted) |
| `ours == base` | `theirs` (a deletion by theirs wins) |
| `theirs == base` | `ours` |
| both objects | merge per key over the union of keys, recursively; if `base` is absent or not an object, use an empty object as base (both sides added the key) |
| all three arrays, usable key | keyed array merge (below) |
| anything else | conflict at this path |

Delete vs. modify is a conflict (one side removed it, the other changed it).
Key order in objects: base order, then keys new in ours, then keys new in
theirs.

### Keyed arrays

Applies when `--array-key` (any listed key) is usable on base, ours and theirs,
using the existing rule from `match_array_items` (every item an object with a
unique string, number or boolean value). Items are merged by key with the node
rule above. Order: if only one side reordered the items common to base, use
that side's order; if both reordered them differently, conflict on the array;
items added by one side go after their predecessor in that side's array, else
at the front; additions at the same position from both sides are ordered by the
identity string of each side's first added item. Arrays without a usable key are merged only by the equal-or-
unchanged rows above; otherwise a conflict on the whole array. This is a call,
not a request: it avoids the index-shifting problem and tells users to supply a
key.

### Conflicts

`Conflict { path, base: Option<Value>, ours: Option<Value>, theirs: Option<Value> }`.
`path` is a JSON Pointer. All conflicts are collected in one pass, not just the
first. `--ignore-path` is not supported in v1.

### Library

```rust
pub fn merge(base: &Value, ours: &Value, theirs: &Value) -> Merged;
pub fn merge_with(base: &Value, ours: &Value, theirs: &Value, options: &MergeOptions) -> Merged;

pub enum Merged { Clean(Value), Conflicted(Vec<Conflict>) }

#[non_exhaustive]
pub struct MergeOptions { /* array_key(..), prefer(Side) */ }
pub enum Side { Ours, Theirs }
```

With `prefer`, every conflict is resolved to that side's value (a deletion
included) and the result is always `Clean`.

### CLI

```
drift merge BASE OURS THEIRS [--array-key K]... [--prefer ours|theirs]
                             [--format F] [--xml-arrays] [-o FILE]
                             [--indent N] [--compact]
```

- Clean: writes the merged document in the format of BASE (YAML/TOML through
  `dump_like` against the base text so comments survive; JSON and XML written
  fresh), exit 0.
- Conflicts: writes nothing to the document destination, prints
  `{"conflicts":[...]}` to stdout, exit 1. Real errors (unreadable file, bad
  format) print to stderr and also exit 1, as everywhere else in the CLI today.
- stdin (`-`) allowed for one of the three inputs, as for `diff`.

## Tests

- Library unit tests for each row of the rule table, keyed arrays (insert on
  both sides, one side reorders, delete vs modify) and `prefer`.
- Property tests in `tests/`: `merge(b, x, b) == x`, `merge(b, b, x) == x`,
  `merge(b, x, x) == x`, clean results commute, and for documents where the two
  edits touch disjoint top-level keys the result equals applying both diffs.
- CLI tests: clean merge, conflict exit code and JSON, `--prefer`, `-o`,
  YAML comments preserved after a clean merge.

## Left to the builder

Module layout (`src/merge.rs`), exact conflict JSON field names beyond the ones
above, README section wording, whether to expose a `--conflicts-only` flag.
