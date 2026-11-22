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
}
