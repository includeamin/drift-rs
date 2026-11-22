//! Property tests: the central promise is `patch(old, diff(new, old)) == new`.
use drift::{compose, diff, diff_with, invert, patch, DiffOptions};
use proptest::prelude::*;
use serde_json::{json, Value};

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

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn diff_then_patch_gives_the_new_document(old in arb_value(), new in arb_value()) {
        let ops = diff(&new, &old);
        prop_assert_eq!(patch(old, &ops).unwrap(), new);
    }

    #[test]
    fn identical_documents_have_an_empty_diff(doc in arb_value()) {
        prop_assert!(diff(&doc, &doc).is_empty());
    }

    #[test]
    fn keyed_diff_round_trips(old in arb_keyed(), new in arb_keyed()) {
        let options = DiffOptions::new().array_key("id");
        let ops = diff_with(&new, &old, &options);
        prop_assert_eq!(patch(old, &ops).unwrap(), new);
    }

    #[test]
    fn patches_survive_serde(old in arb_value(), new in arb_value()) {
        let ops = diff(&new, &old);
        let text = serde_json::to_string(&ops).unwrap();
        let back: Vec<drift::Delta> = serde_json::from_str(&text).unwrap();
        prop_assert_eq!(patch(old, &back).unwrap(), new);
    }

    #[test]
    fn invert_undoes_a_patch(old in arb_value(), new in arb_value()) {
        let ops = diff(&new, &old);
        let undo = invert(&old, &ops).unwrap();
        prop_assert_eq!(patch(new, &undo).unwrap(), old);
    }

    #[test]
    fn compose_equals_applying_in_turn(a in arb_value(), b in arb_value(), c in arb_value()) {
        let first = diff(&b, &a);
        let second = diff(&c, &b);
        let all: Vec<_> = first.iter().chain(second.iter()).cloned().collect();
        let squashed = compose(&a, &all, &DiffOptions::default()).unwrap();
        prop_assert_eq!(patch(a, &squashed).unwrap(), c);
    }
}
