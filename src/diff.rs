use crate::{join_pointer, Delta, Operation};
use serde_json::Value;
use std::collections::BTreeSet;

fn strict_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null, Value::Null)
        | (Value::Bool(_), Value::Bool(_))
        | (Value::String(_), Value::String(_)) => left == right,
        (Value::Number(_), Value::Number(_)) => left == right,
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| strict_equal(x, y))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter().all(|(k, v)| b.get(k).is_some_and(|other| strict_equal(v, other)))
        }
        _ => false,
    }
}

fn diff_inner(new: &Value, old: &Value, path: &str, output: &mut Vec<Delta>) {
    match (new, old) {
        (Value::Object(new_obj), Value::Object(old_obj)) => {
            let old_keys: BTreeSet<_> = old_obj.keys().collect();
            let new_keys: BTreeSet<_> = new_obj.keys().collect();
            for key in old_keys.difference(&new_keys) {
                output.push(Delta::new(Operation::Remove, join_pointer(path, key)));
            }
            for key in new_keys.difference(&old_keys) {
                output.push(Delta::with_value(
                    Operation::Add,
                    join_pointer(path, key),
                    new_obj[*key].clone(),
                ));
            }
            for key in old_keys.intersection(&new_keys) {
                diff_inner(&new_obj[*key], &old_obj[*key], &join_pointer(path, key), output);
            }
        }
        (Value::Array(new_items), Value::Array(old_items)) => {
            for (index, (new_value, old_value)) in new_items.iter().zip(old_items).enumerate() {
                diff_inner(new_value, old_value, &join_pointer(path, &index.to_string()), output);
            }
            for index in (new_items.len()..old_items.len()).rev() {
                output.push(Delta::new(Operation::Remove, join_pointer(path, &index.to_string())));
            }
            for (index, item) in new_items.iter().enumerate().skip(old_items.len()) {
                output.push(Delta::with_value(
                    Operation::Add,
                    join_pointer(path, &index.to_string()),
                    item.clone(),
                ));
            }
        }
        _ if !strict_equal(new, old) => {
            output.push(Delta::with_value(Operation::Replace, path, new.clone()))
        }
        _ => {}
    }
}

pub fn diff(new: &Value, old: &Value) -> Vec<Delta> {
    let mut output = Vec::new();
    diff_inner(new, old, "", &mut output);
    output
}
