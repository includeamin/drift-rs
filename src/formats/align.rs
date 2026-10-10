//! Aligning two lists, so an edit changes only what differs.

use serde_json::Value;

/// What to do with the items of an array, in order, to turn `old` into `new`.
#[derive(Debug, PartialEq)]
pub(super) enum Step {
    /// Leave old item `.0` alone: it equals new item `.1`.
    Keep,
    /// Old item `.0` becomes new item `.1`, edited in place.
    Edit(usize, usize),
    /// Delete the old item at the cursor.
    Remove,
    /// Insert new item `.0` at the cursor.
    Insert(usize),
}

/// Aligns two lists so that unchanged items stay as they are and only the
/// differences are edited: equal items are kept, a changed item is edited in
/// place, and extra ones are removed or inserted. Without this, deleting the
/// first of three entries would rewrite entry one into entry two, leaving its
/// comment on the wrong entry.
///
/// Works out the longest common subsequence of equal items after trimming the
/// equal ends; lists too large for that fall back to matching by position.
pub(super) fn align(old: &[Value], new: &[Value]) -> Vec<Step> {
    const MAX_CELLS: usize = 250_000;
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let (mid_old, mid_new) = (&old[prefix..old.len() - suffix], &new[prefix..new.len() - suffix]);

    let mut steps: Vec<Step> = (0..prefix).map(|_| Step::Keep).collect();
    if mid_old.len().saturating_mul(mid_new.len()) > MAX_CELLS {
        gap(&mut steps, prefix..old.len() - suffix, prefix..new.len() - suffix);
    } else {
        // lcs[i][j]: length of the longest common subsequence of mid_old[i..] and mid_new[j..].
        let width = mid_new.len() + 1;
        let mut lcs = vec![0u32; (mid_old.len() + 1) * width];
        for i in (0..mid_old.len()).rev() {
            for j in (0..mid_new.len()).rev() {
                lcs[i * width + j] = if mid_old[i] == mid_new[j] {
                    lcs[(i + 1) * width + j + 1] + 1
                } else {
                    lcs[(i + 1) * width + j].max(lcs[i * width + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        let (mut gap_old, mut gap_new) = (prefix, prefix);
        while i < mid_old.len() && j < mid_new.len() {
            if mid_old[i] == mid_new[j] {
                gap(&mut steps, gap_old..prefix + i, gap_new..prefix + j);
                steps.push(Step::Keep);
                (i, j) = (i + 1, j + 1);
                (gap_old, gap_new) = (prefix + i, prefix + j);
            } else if lcs[(i + 1) * width + j] >= lcs[i * width + j + 1] {
                i += 1;
            } else {
                j += 1;
            }
        }
        gap(&mut steps, gap_old..old.len() - suffix, gap_new..new.len() - suffix);
    }
    steps.extend((0..suffix).map(|_| Step::Keep));
    steps
}

/// Between two kept items: pair the leftovers off as in-place edits, then
/// remove or insert whatever is left over on one side.
fn gap(steps: &mut Vec<Step>, old: std::ops::Range<usize>, new: std::ops::Range<usize>) {
    let paired = old.len().min(new.len());
    steps.extend((0..paired).map(|k| Step::Edit(old.start + k, new.start + k)));
    steps.extend((paired..old.len()).map(|_| Step::Remove));
    steps.extend((paired..new.len()).map(|k| Step::Insert(new.start + k)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn steps(old: Value, new: Value) -> Vec<Step> {
        align(old.as_array().unwrap(), new.as_array().unwrap())
    }

    #[test]
    fn alignment_keeps_equal_items_and_removes_or_inserts_the_rest() {
        use Step::*;
        assert_eq!(steps(json!([1, 2, 3]), json!([1, 2, 3])), [Keep, Keep, Keep]);
        assert_eq!(steps(json!([1, 2, 3]), json!([2, 3])), [Remove, Keep, Keep]);
        assert_eq!(steps(json!([1, 2, 3]), json!([1, 3])), [Keep, Remove, Keep]);
        assert_eq!(steps(json!([1, 2, 3]), json!([1, 2])), [Keep, Keep, Remove]);
        assert_eq!(steps(json!([2, 3]), json!([1, 2, 3])), [Insert(0), Keep, Keep]);
        assert_eq!(steps(json!([1, 2]), json!([1, 2, 3])), [Keep, Keep, Insert(2)]);
        assert_eq!(steps(json!([]), json!([1, 2])), [Insert(0), Insert(1)]);
        assert_eq!(steps(json!([1, 2]), json!([])), [Remove, Remove]);
    }

    #[test]
    fn a_changed_item_is_edited_in_place_not_replaced() {
        use Step::*;
        assert_eq!(steps(json!([1, 2, 3]), json!([1, 9, 3])), [Keep, Edit(1, 1), Keep]);
        // One changed and one dropped in the same gap: edit the first, remove the other.
        assert_eq!(steps(json!([1, 2, 3, 4]), json!([1, 9, 4])), [Keep, Edit(1, 1), Remove, Keep]);
        // Shifted by a removal, the surviving items still match their old selves.
        assert_eq!(steps(json!([{"a": 1}, {"a": 2}]), json!([{"a": 2}])), [Remove, Keep]);
    }

    #[test]
    fn alignment_replays_to_the_new_list() {
        // xorshift: deterministic, no extra dependencies.
        let mut state = 0x1B873593u64;
        let mut next = move |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        for _ in 0..500 {
            let make = |next: &mut dyn FnMut(u64) -> u64| -> Vec<Value> {
                (0..next(8)).map(|_| json!(next(4))).collect()
            };
            let (old, new) = (make(&mut next), make(&mut next));
            let mut list = old.clone();
            let mut at = 0;
            for step in align(&old, &new) {
                match step {
                    Step::Keep => at += 1,
                    Step::Edit(_, j) => {
                        list[at] = new[j].clone();
                        at += 1;
                    }
                    Step::Remove => {
                        list.remove(at);
                    }
                    Step::Insert(j) => {
                        list.insert(at, new[j].clone());
                        at += 1;
                    }
                }
            }
            assert_eq!(list, new, "{old:?} -> {new:?}");
        }
    }

    #[test]
    fn huge_lists_still_align_by_position() {
        let old: Vec<Value> = (0..1000).map(|i| json!(i)).collect();
        let new: Vec<Value> = (0..1000).map(|i| json!(i + 1)).collect();
        let steps = align(&old, &new);
        assert_eq!(steps.len(), 1000);
        assert!(steps.iter().all(|s| matches!(s, Step::Edit(..))));
    }
}
