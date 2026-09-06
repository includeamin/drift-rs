use crate::join_pointer;
use serde_json::Value;

struct PathOptions {
    include_root: bool,
    include_containers: bool,
    sort_keys: bool,
    max_depth: Option<usize>,
}

fn walk(value: &Value, path: &str, depth: usize, options: &PathOptions, result: &mut Vec<String>) {
    if options.max_depth.is_some_and(|limit| depth > limit) {
        return;
    }
    let is_container = value.is_object() || value.is_array();
    let has_items = value.as_object().is_some_and(|map| !map.is_empty())
        || value.as_array().is_some_and(|items| !items.is_empty());
    if options.include_containers
        && is_container
        && (has_items || (path.is_empty() && options.include_root))
        && (!path.is_empty() || options.include_root)
    {
        result.push(path.into());
    }
    match value {
        Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            if options.sort_keys {
                keys.sort();
            }
            for key in keys {
                walk(&map[key], &join_pointer(path, key), depth + 1, options, result);
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                walk(item, &join_pointer(path, &index.to_string()), depth + 1, options, result);
            }
        }
        _ if !path.is_empty() || options.include_root => result.push(path.into()),
        _ => {}
    }
}

pub fn list_json_paths(
    value: &Value,
    include_root: bool,
    include_containers: bool,
    sort_keys: bool,
    max_depth: Option<usize>,
) -> Vec<String> {
    let options = PathOptions { include_root, include_containers, sort_keys, max_depth };
    let mut result = Vec::new();
    walk(value, "", 0, &options, &mut result);
    result
}
