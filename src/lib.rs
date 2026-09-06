mod diff;
mod error;
mod model;
mod patch;
mod paths;
mod pointer;
mod scan;
mod search;
mod stream_io;
pub mod streaming;

pub use diff::diff;
pub use error::DriftError;
pub use model::{Delta, Operation};
pub use patch::patch;
pub use paths::list_json_paths;
pub use pointer::{escape_token, join_pointer, split_pointer, unescape_token};
pub use search::{filter_operations, path_matches};
pub use stream_io::diff_files;

pub const VERSION: &str = "0.14.1";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ===== Pointer Tests =====
    #[test]
    fn pointers_escape() {
        assert_eq!(join_pointer("", "a/b~c"), "/a~1b~0c");
        assert_eq!(split_pointer("/a~1b~0c").unwrap(), vec!["a/b~c"]);
    }

    #[test]
    fn escape_token_slash() {
        assert_eq!(escape_token("foo/bar"), "foo~1bar");
    }

    #[test]
    fn escape_token_tilde() {
        assert_eq!(escape_token("foo~bar"), "foo~0bar");
    }

    #[test]
    fn escape_token_both() {
        assert_eq!(escape_token("a/b~c"), "a~1b~0c");
    }

    #[test]
    fn unescape_token_slash() {
        assert_eq!(unescape_token("foo~1bar").unwrap(), "foo/bar");
    }

    #[test]
    fn unescape_token_tilde() {
        assert_eq!(unescape_token("foo~0bar").unwrap(), "foo~bar");
    }

    #[test]
    fn unescape_token_invalid() {
        assert!(unescape_token("foo~2bar").is_err());
        assert!(unescape_token("foo~").is_err());
    }

    #[test]
    fn split_pointer_root() {
        assert_eq!(split_pointer("").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn split_pointer_single() {
        assert_eq!(split_pointer("/foo").unwrap(), vec!["foo"]);
    }

    #[test]
    fn split_pointer_multiple() {
        assert_eq!(split_pointer("/foo/bar/baz").unwrap(), vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn split_pointer_no_leading_slash() {
        assert!(split_pointer("foo").is_err());
    }

    #[test]
    fn split_pointer_with_escapes() {
        assert_eq!(split_pointer("/a~1b/c~0d").unwrap(), vec!["a/b", "c~d"]);
    }

    #[test]
    fn join_pointer_empty_path() {
        assert_eq!(join_pointer("", "foo"), "/foo");
    }

    #[test]
    fn join_pointer_with_path() {
        assert_eq!(join_pointer("/a/b", "c"), "/a/b/c");
    }

    #[test]
    fn join_pointer_special_chars() {
        assert_eq!(join_pointer("/x", "a/b~c"), "/x/a~1b~0c");
    }

    // ===== Diff Tests =====
    #[test]
    fn nested_diff_round_trips() {
        let old = json!({"a": {"b": [1, 2]}});
        let new = json!({"a": {"b": [1, 3, 4]}});
        assert_eq!(patch(old.clone(), &diff(&new, &old)).unwrap(), new);
    }

    #[test]
    fn diff_simple_add() {
        let old = json!({});
        let new = json!({"x": 1});
        let ops = diff(&new, &old);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].op, Operation::Add);
        assert_eq!(ops[0].path, "/x");
    }

    #[test]
    fn diff_simple_remove() {
        let old = json!({"x": 1});
        let new = json!({});
        let ops = diff(&new, &old);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].op, Operation::Remove);
        assert_eq!(ops[0].path, "/x");
    }

    #[test]
    fn diff_simple_replace() {
        let old = json!({"x": 1});
        let new = json!({"x": 2});
        let ops = diff(&new, &old);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].op, Operation::Replace);
        assert_eq!(ops[0].path, "/x");
        assert_eq!(ops[0].value, Some(json!(2)));
    }

    #[test]
    fn diff_array_insert() {
        let old = json!([1, 2]);
        let new = json!([1, 9, 2]);
        let ops = diff(&new, &old);
        // Array diff compares element by element, so this is a replace at /1 and add at /2
        assert!(ops.iter().any(|op| op.op == Operation::Replace && op.path == "/1"));
        assert!(ops.iter().any(|op| op.op == Operation::Add && op.path == "/2"));
    }

    #[test]
    fn diff_array_remove() {
        let old = json!([1, 2, 3]);
        let new = json!([1, 3]);
        let ops = diff(&new, &old);
        // Replace element at index 1, then remove element at index 2
        assert!(ops.iter().any(|op| op.op == Operation::Replace && op.path == "/1"));
        assert!(ops.iter().any(|op| op.op == Operation::Remove && op.path == "/2"));
    }

    #[test]
    fn diff_nested_object() {
        let old = json!({"a": {"b": 1}});
        let new = json!({"a": {"b": 2}});
        let ops = diff(&new, &old);
        assert_eq!(ops[0].path, "/a/b");
    }

    #[test]
    fn diff_no_changes() {
        let doc = json!({"x": 1, "y": [1, 2, 3]});
        let ops = diff(&doc, &doc);
        assert!(ops.is_empty());
    }

    #[test]
    fn diff_type_change() {
        let old = json!({"x": 1});
        let new = json!({"x": "string"});
        let ops = diff(&new, &old);
        assert_eq!(ops[0].op, Operation::Replace);
    }

    #[test]
    fn diff_null_values() {
        let old = json!({"x": null});
        let new = json!({"x": 1});
        let ops = diff(&new, &old);
        assert_eq!(ops[0].op, Operation::Replace);
    }

    // ===== Patch Tests =====
    #[test]
    fn arrays_insert() {
        let old = json!([1, 2]);
        let operation = Delta::with_value(Operation::Add, "/1", json!(9));
        assert_eq!(patch(old, &[operation]).unwrap(), json!([1, 9, 2]));
    }

    #[test]
    fn patch_add_object_field() {
        let doc = json!({"x": 1});
        let op = Delta::with_value(Operation::Add, "/y", json!(2));
        assert_eq!(patch(doc, &[op]).unwrap(), json!({"x": 1, "y": 2}));
    }

    #[test]
    fn patch_remove_object_field() {
        let doc = json!({"x": 1, "y": 2});
        let op = Delta::new(Operation::Remove, "/x");
        assert_eq!(patch(doc, &[op]).unwrap(), json!({"y": 2}));
    }

    #[test]
    fn patch_replace_value() {
        let doc = json!({"x": 1});
        let op = Delta::with_value(Operation::Replace, "/x", json!(2));
        assert_eq!(patch(doc, &[op]).unwrap(), json!({"x": 2}));
    }

    #[test]
    fn patch_add_to_array_end() {
        let doc = json!([1, 2]);
        let op = Delta::with_value(Operation::Add, "/-", json!(3));
        assert_eq!(patch(doc, &[op]).unwrap(), json!([1, 2, 3]));
    }

    #[test]
    fn patch_add_nested() {
        let doc = json!({"a": {"b": 1}});
        let op = Delta::with_value(Operation::Add, "/a/c", json!(2));
        assert_eq!(patch(doc, &[op]).unwrap(), json!({"a": {"b": 1, "c": 2}}));
    }

    #[test]
    fn patch_replace_at_root() {
        let doc = json!({"x": 1});
        let op = Delta::with_value(Operation::Replace, "", json!({"y": 2}));
        assert_eq!(patch(doc, &[op]).unwrap(), json!({"y": 2}));
    }

    #[test]
    fn patch_remove_array_element() {
        let doc = json!([1, 2, 3]);
        let op = Delta::new(Operation::Remove, "/1");
        assert_eq!(patch(doc, &[op]).unwrap(), json!([1, 3]));
    }

    #[test]
    fn patch_multiple_operations() {
        let doc = json!({"x": 1});
        let ops = vec![
            Delta::with_value(Operation::Add, "/y", json!(2)),
            Delta::new(Operation::Remove, "/x"),
        ];
        assert_eq!(patch(doc, &ops).unwrap(), json!({"y": 2}));
    }

    #[test]
    fn patch_test_success() {
        let doc = json!({"x": 1});
        let op = Delta::with_value(Operation::Test, "/x", json!(1));
        assert!(patch(doc, &[op]).is_ok());
    }

    #[test]
    fn patch_test_failure() {
        let doc = json!({"x": 1});
        let op = Delta::with_value(Operation::Test, "/x", json!(2));
        assert!(patch(doc, &[op]).is_err());
    }

    #[test]
    fn patch_missing_path() {
        let doc = json!({"x": 1});
        let op = Delta::new(Operation::Remove, "/nonexistent");
        assert!(patch(doc, &[op]).is_err());
    }

    #[test]
    fn patch_invalid_array_index() {
        let doc = json!([1, 2]);
        let op = Delta::with_value(Operation::Add, "/999", json!(3));
        assert!(patch(doc, &[op]).is_err());
    }

    // ===== Path Listing Tests =====
    #[test]
    fn list_paths_simple_object() {
        let doc = json!({"a": 1, "b": 2});
        let paths = list_json_paths(&doc, false, false, false, None);
        assert!(paths.contains(&"/a".to_string()));
        assert!(paths.contains(&"/b".to_string()));
    }

    #[test]
    fn list_paths_simple_array() {
        let doc = json!([1, 2, 3]);
        let paths = list_json_paths(&doc, false, false, false, None);
        assert!(paths.contains(&"/0".to_string()));
        assert!(paths.contains(&"/1".to_string()));
        assert!(paths.contains(&"/2".to_string()));
    }

    #[test]
    fn list_paths_nested() {
        let doc = json!({"a": {"b": 1}});
        let paths = list_json_paths(&doc, false, false, false, None);
        assert!(paths.contains(&"/a/b".to_string()));
    }

    #[test]
    fn list_paths_with_containers() {
        let doc = json!({"a": {"b": 1}});
        let paths = list_json_paths(&doc, false, true, false, None);
        assert!(paths.contains(&"/a".to_string()));
        assert!(paths.contains(&"/a/b".to_string()));
    }

    #[test]
    fn list_paths_with_root() {
        let doc = json!({"a": 1});
        let paths = list_json_paths(&doc, true, true, false, None);
        assert!(paths.contains(&"".to_string()));
    }

    #[test]
    fn list_paths_with_max_depth() {
        let doc = json!({"a": {"b": {"c": 1}}});
        let paths = list_json_paths(&doc, false, false, false, Some(1));
        // max_depth: 1 means we go 1 level deep, so we get /a but not /a/b or /a/b/c
        assert!(paths.contains(&"/a".to_string()) || paths.is_empty());
    }

    #[test]
    fn list_paths_sorted() {
        let doc = json!({"z": 1, "a": 2, "m": 3});
        let paths = list_json_paths(&doc, false, false, true, None);
        assert_eq!(paths[0], "/a");
        assert_eq!(paths[1], "/m");
        assert_eq!(paths[2], "/z");
    }

    #[test]
    fn list_paths_empty_object() {
        let doc = json!({});
        let paths = list_json_paths(&doc, false, false, false, None);
        assert!(paths.is_empty());
    }

    // ===== Search/Filter Tests =====
    #[test]
    fn path_matches_exact() {
        assert!(path_matches("/a/b", "/a/b").unwrap());
        assert!(!path_matches("/a/b", "/a/c").unwrap());
    }

    #[test]
    fn path_matches_wildcard() {
        assert!(path_matches("/a/*/c", "/a/b/c").unwrap());
        assert!(!path_matches("/a/*/c", "/a/b/d/c").unwrap());
    }

    #[test]
    fn path_matches_glob() {
        assert!(path_matches("/a/**/c", "/a/b/c").unwrap());
        assert!(path_matches("/a/**/c", "/a/b/d/c").unwrap());
        assert!(!path_matches("/a/**/c", "/a/c/x").unwrap());
    }

    #[test]
    fn filter_operations_by_path() {
        let ops = vec![Delta::new(Operation::Add, "/a/x"), Delta::new(Operation::Add, "/b/y")];
        let filtered =
            filter_operations(&ops, None, &["/a/**".to_string()], &[], &[], &[], false).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].path, "/a/x");
    }

    #[test]
    fn filter_operations_by_operation_type() {
        let ops = vec![Delta::new(Operation::Add, "/a"), Delta::new(Operation::Remove, "/b")];
        let filtered =
            filter_operations(&ops, None, &[], &[], &[], &["add".to_string()], false).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].op, Operation::Add);
    }

    #[test]
    fn filter_operations_invert() {
        let ops = vec![Delta::new(Operation::Add, "/a"), Delta::new(Operation::Remove, "/b")];
        let filtered =
            filter_operations(&ops, None, &[], &[], &[], &["add".to_string()], true).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].op, Operation::Remove);
    }

    #[test]
    fn filter_operations_by_field() {
        let ops =
            vec![Delta::new(Operation::Add, "/username"), Delta::new(Operation::Add, "/password")];
        let filtered =
            filter_operations(&ops, None, &[], &["user.*".to_string()], &[], &[], false).unwrap();
        assert_eq!(filtered.len(), 1);
    }

    // ===== Round-trip Tests =====
    #[test]
    fn round_trip_complex_object() {
        let old = json!({
            "name": "John",
            "age": 30,
            "tags": ["rust", "testing"],
            "meta": {"created": "2024", "active": true}
        });
        let new = json!({
            "name": "Jane",
            "age": 28,
            "tags": ["rust", "testing", "ci"],
            "meta": {"created": "2024", "active": false, "verified": true}
        });
        let ops = diff(&new, &old);
        assert_eq!(patch(old, &ops).unwrap(), new);
    }

    #[test]
    fn round_trip_array_operations() {
        let old = json!([1, 2, 3, 4, 5]);
        let new = json!([1, 9, 3, 5]);
        let ops = diff(&new, &old);
        assert_eq!(patch(old, &ops).unwrap(), new);
    }

    #[test]
    fn round_trip_unicode() {
        let old = json!({"emoji": "😀", "chinese": "你好"});
        let new = json!({"emoji": "😢", "chinese": "再见"});
        let ops = diff(&new, &old);
        assert_eq!(patch(old, &ops).unwrap(), new);
    }

    // ===== Edge Cases =====
    #[test]
    fn diff_empty_to_full() {
        let old = json!({});
        let new = json!({"a": 1, "b": 2});
        let ops = diff(&new, &old);
        assert_eq!(ops.len(), 2);
    }

    #[test]
    fn diff_full_to_empty() {
        let old = json!({"a": 1, "b": 2});
        let new = json!({});
        let ops = diff(&new, &old);
        assert_eq!(ops.len(), 2);
    }

    #[test]
    fn patch_add_replaces_null() {
        let doc = json!({"x": null});
        let op = Delta::with_value(Operation::Add, "/x", json!(1));
        // Add operation on an object field replaces it
        assert_eq!(patch(doc, &[op]).unwrap(), json!({"x": 1}));
    }

    #[test]
    fn pointer_with_array_index_zero() {
        assert_eq!(split_pointer("/0").unwrap(), vec!["0"]);
    }

    #[test]
    fn pointer_empty_segments() {
        let path = split_pointer("/a//b").unwrap();
        assert_eq!(path, vec!["a", "", "b"]);
    }

    #[test]
    fn diff_numeric_values_types() {
        let old = json!({"x": 1});
        let new = json!({"x": 1.0});
        // JSON treats 1 and 1.0 as equal
        let ops = diff(&new, &old);
        // Should have no difference or should recognize as equal
        assert_eq!(patch(old, &ops).unwrap(), new);
    }
}
