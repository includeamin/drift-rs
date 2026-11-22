//! Property tests for the three-way merge.
use drift::{diff, merge, merge_with, patch, MergeOptions, Merged, Side};
use proptest::prelude::*;
use serde_json::{json, Map, Value};

// `arb_value` and `arb_keyed` are copied from tests/roundtrip.rs: integration
// test files cannot share modules without a `common` directory.
fn arb_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        (-50i64..50).prop_map(|n| json!(n)),
        "[a-c~/ ]{0,3}".prop_map(Value::String),
    ];
    leaf.prop_recursive(4, 40, 5, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..5).prop_map(Value::Array),
            prop::collection::btree_map("[a-d~/]{1,2}", inner, 0..5)
                .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    })
}

/// Arrays of objects with an `id`, to exercise key-based matching and moves.
fn arb_keyed() -> impl Strategy<Value = Value> {
    prop::collection::btree_map(0u8..8, (-3i64..3, "[ab]{0,2}"), 0..7).prop_flat_map(|m| {
        let items: Vec<Value> =
            m.into_iter().map(|(id, (n, s))| json!({"id": id, "n": n, "s": s})).collect();
        Just(items).prop_shuffle().prop_map(|v| json!({ "items": v }))
    })
}

fn clean(v: &Value) -> Merged {
    Merged::Clean(v.clone())
}

fn is_clean(m: &Merged) -> bool {
    matches!(m, Merged::Clean(_))
}

/// An object with keys `a..d`, and replacement values for each.
fn arb_disjoint() -> impl Strategy<Value = (Value, [Value; 4])> {
    (prop::array::uniform4(arb_value()), prop::array::uniform4(arb_value())).prop_map(
        |(base, edits)| {
            let mut m = Map::new();
            for (k, v) in ["a", "b", "c", "d"].iter().zip(base) {
                m.insert((*k).to_string(), v);
            }
            (Value::Object(m), edits)
        },
    )
}

/// Keyed items as `(id, n)` pairs, wrapped in a document.
fn keyed_doc(items: &[(u8, i64)]) -> Value {
    let items: Vec<Value> = items.iter().map(|(id, n)| json!({"id": id, "n": n})).collect();
    json!({ "items": items })
}

/// One side derived from `base`: each item is dropped, modified or kept, the
/// list is sometimes shuffled, and new ids come from a pool both sides share,
/// so both sides can add the same id at different places.
fn derive_keyed(base: Vec<u8>) -> impl Strategy<Value = Vec<(u8, i64)>> {
    let n = base.len();
    let actions = prop::collection::vec(0u8..8, n);
    let additions = prop::collection::vec((8u8..11, 0i64..2, 0usize..8), 0..3);
    (actions, additions, prop::bool::weighted(0.15), Just(base))
        .prop_flat_map(|(actions, additions, shuffle, base)| {
            let mut items: Vec<(u8, i64)> = Vec::new();
            for (id, action) in base.into_iter().zip(actions) {
                match action {
                    0 => {}
                    1 | 2 => items.push((id, i64::from(action))),
                    _ => items.push((id, 0)),
                }
            }
            let items =
                if shuffle { Just(items).prop_shuffle().boxed() } else { Just(items).boxed() };
            (items, Just(additions))
        })
        .prop_map(|(mut items, additions)| {
            for (id, n, at) in additions {
                if !items.iter().any(|(i, _)| *i == id) {
                    let at = at.min(items.len());
                    items.insert(at, (id, n));
                }
            }
            items
        })
}

/// A keyed base and two sides derived from it.
fn arb_keyed_triple() -> impl Strategy<Value = (Value, Value, Value)> {
    prop::collection::btree_set(0u8..8, 0..6)
        .prop_flat_map(|ids| Just(ids.into_iter().collect::<Vec<_>>()).prop_shuffle())
        .prop_flat_map(|base| (Just(base.clone()), derive_keyed(base.clone()), derive_keyed(base)))
        .prop_map(|(base, a, b)| {
            let base: Vec<(u8, i64)> = base.into_iter().map(|id| (id, 0)).collect();
            (keyed_doc(&base), keyed_doc(&a), keyed_doc(&b))
        })
}

/// A tiny deterministic generator, so one seed describes a whole set of edits.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn pick<'a>(&mut self, pool: &'a [Value]) -> &'a Value {
        &pool[(self.next() % pool.len() as u64) as usize]
    }
}

/// `value` with a few edits: many nodes are kept, some leaves and arrays are
/// replaced, object keys are sometimes removed or added. Replacements come
/// from `pool`, which both sides share. The root is always visited.
fn mutate(value: &Value, rng: &mut Rng, pool: &[Value], depth: usize) -> Value {
    if depth > 0 && rng.chance(50) {
        return value.clone();
    }
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, child) in map {
                if !rng.chance(10) {
                    out.insert(key.clone(), mutate(child, rng, pool, depth + 1));
                }
            }
            if rng.chance(20) {
                let key = ["n", "m", "a"][(rng.next() % 3) as usize];
                out.entry(key).or_insert_with(|| rng.pick(pool).clone());
            }
            Value::Object(out)
        }
        _ => {
            if rng.chance(50) {
                rng.pick(pool).clone()
            } else {
                value.clone()
            }
        }
    }
}

/// A document and two sides derived from it by a few edits each.
fn arb_plain_triple() -> impl Strategy<Value = (Value, Value, Value)> {
    let base = prop::collection::btree_map("[a-d]", arb_value(), 1..5)
        .prop_map(|m| Value::Object(m.into_iter().collect()));
    let pool = prop::collection::vec(arb_value(), 1..4);
    (base, pool, any::<u64>(), any::<u64>()).prop_map(|(base, pool, sa, sb)| {
        let a = mutate(&base, &mut Rng(sa | 1), &pool, 0);
        let b = mutate(&base, &mut Rng(sb | 1), &pool, 0);
        (base, a, b)
    })
}

/// Merging (a, b) and (b, a) are both clean or both conflicted, and clean
/// results are equal; preferring ours with (a, b) equals preferring theirs
/// with (b, a).
fn check_symmetric(
    base: &Value,
    a: &Value,
    b: &Value,
    options: &MergeOptions,
) -> Result<(), TestCaseError> {
    let ab = merge_with(base, a, b, options);
    let ba = merge_with(base, b, a, options);
    prop_assert_eq!(is_clean(&ab), is_clean(&ba), "base={} a={} b={}", base, a, b);
    if let (Merged::Clean(x), Merged::Clean(y)) = (&ab, &ba) {
        prop_assert_eq!(x, y, "base={} a={} b={}", base, a, b);
    }
    let ours = merge_with(base, a, b, &options.clone().prefer(Side::Ours));
    let theirs = merge_with(base, b, a, &options.clone().prefer(Side::Theirs));
    prop_assert_eq!(ours, theirs, "base={} a={} b={}", base, a, b);
    Ok(())
}

/// 512 cases by default; `PROPTEST_CASES` overrides it (an explicit
/// `with_cases` would otherwise ignore the environment variable).
fn config() -> ProptestConfig {
    let cases = std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(512);
    ProptestConfig::with_cases(cases)
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn unchanged_side_yields_the_other(base in arb_value(), x in arb_value()) {
        prop_assert_eq!(merge(&base, &x, &base), clean(&x));
        prop_assert_eq!(merge(&base, &base, &x), clean(&x));
    }

    #[test]
    fn identical_changes_merge_cleanly(base in arb_value(), x in arb_value()) {
        prop_assert_eq!(merge(&base, &x, &x), clean(&x));
    }

    #[test]
    fn clean_merges_commute(base in arb_value(), a in arb_value(), b in arb_value()) {
        let ab = merge(&base, &a, &b);
        let ba = merge(&base, &b, &a);
        prop_assert_eq!(is_clean(&ab), is_clean(&ba));
        if let (Merged::Clean(x), Merged::Clean(y)) = (&ab, &ba) {
            prop_assert_eq!(x, y);
        }
    }

    #[test]
    fn prefer_is_always_clean(base in arb_value(), a in arb_value(), b in arb_value()) {
        for side in [Side::Ours, Side::Theirs] {
            let options = MergeOptions::new().prefer(side);
            prop_assert!(is_clean(&merge_with(&base, &a, &b, &options)));
        }
    }

    #[test]
    fn disjoint_edits_equal_both_patches((base, edits) in arb_disjoint()) {
        let [ea, eb, ec, ed] = edits;
        let mut ours = base.clone();
        ours["a"] = ea;
        ours["b"] = eb;
        let mut theirs = base.clone();
        theirs["c"] = ec;
        theirs["d"] = ed;
        let step = patch(base.clone(), &diff(&ours, &base)).unwrap();
        let expected = patch(step, &diff(&theirs, &base)).unwrap();
        prop_assert_eq!(merge(&base, &ours, &theirs), Merged::Clean(expected));
    }

    #[test]
    fn keyed_merge_commutes(base in arb_keyed(), a in arb_keyed(), b in arb_keyed()) {
        let options = MergeOptions::new().array_key("id");
        let ab = merge_with(&base, &a, &b, &options);
        let ba = merge_with(&base, &b, &a, &options);
        prop_assert_eq!(is_clean(&ab), is_clean(&ba));
        if let (Merged::Clean(x), Merged::Clean(y)) = (&ab, &ba) {
            prop_assert_eq!(x, y);
        }
    }

    #[test]
    fn derived_keyed_merge_is_symmetric((base, a, b) in arb_keyed_triple()) {
        check_symmetric(&base, &a, &b, &MergeOptions::new().array_key("id"))?;
    }

    #[test]
    fn derived_merge_is_symmetric((base, a, b) in arb_plain_triple()) {
        check_symmetric(&base, &a, &b, &MergeOptions::new())?;
    }
}
