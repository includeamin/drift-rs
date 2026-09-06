use crate::DriftError;

pub fn escape_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

pub fn unescape_token(token: &str) -> Result<String, DriftError> {
    let mut out = String::new();
    let mut chars = token.chars();
    while let Some(ch) = chars.next() {
        if ch != '~' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('0') => out.push('~'),
            Some('1') => out.push('/'),
            _ => return Err(DriftError::Pointer(token.into())),
        }
    }
    Ok(out)
}

pub fn join_pointer(path: &str, token: &str) -> String {
    format!("{}/{}", path, escape_token(token))
}

pub fn split_pointer(path: &str) -> Result<Vec<String>, DriftError> {
    if path.is_empty() {
        return Ok(Vec::new());
    }
    if !path.starts_with('/') {
        return Err(DriftError::Pointer(path.into()));
    }
    path[1..].split('/').map(unescape_token).collect()
}
