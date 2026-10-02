//! Source-text access to JSON values.
//!
//! The `Value` model re-serializes compactly, but Go (gjson `Raw`) copies client values
//! verbatim into string fields such as tool arguments. Those spots read the exact source text
//! with [`raw_at`] instead.

/// The exact source text of the value at `path` (object keys and array indexes separated by
/// `.`), like gjson's `Raw`. Keys containing `.` are not supported. First duplicate key wins,
/// as in gjson.
pub fn raw_at(src: &[u8], path: &str) -> Option<String> {
    let mut start = skip_ws(src, 0);
    let mut end = skip_value(src, start)?;
    if !path.is_empty() {
        for comp in path.split('.') {
            let (s, e) = child_span(src, start, end, comp)?;
            start = s;
            end = e;
        }
    }
    std::str::from_utf8(&src[start..end])
        .ok()
        .map(str::to_string)
}

fn skip_ws(src: &[u8], mut i: usize) -> usize {
    while i < src.len() && matches!(src[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// End index (exclusive) of the value starting at `i`.
fn skip_value(src: &[u8], i: usize) -> Option<usize> {
    match *src.get(i)? {
        b'"' => skip_string(src, i),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut j = i;
            while j < src.len() {
                match src[j] {
                    b'"' => {
                        j = skip_string(src, j)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth = depth.checked_sub(1)?;
                        if depth == 0 {
                            return Some(j + 1);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            None
        }
        _ => {
            let mut j = i;
            while j < src.len()
                && !matches!(src[j], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
            {
                j += 1;
            }
            Some(j)
        }
    }
}

fn skip_string(src: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while j < src.len() {
        match src[j] {
            b'\\' => j += 2,
            b'"' => return Some(j + 1),
            _ => j += 1,
        }
    }
    None
}

/// Span of the child `comp` of the container spanning `start..end`.
fn child_span(src: &[u8], start: usize, end: usize, comp: &str) -> Option<(usize, usize)> {
    let is_object = *src.get(start)? == b'{';
    if !is_object && src[start] != b'[' {
        return None;
    }
    let want_index: Option<usize> = if is_object {
        None
    } else {
        Some(comp.parse().ok()?)
    };
    let mut i = skip_ws(src, start + 1);
    let mut idx = 0usize;
    while i < end {
        if matches!(src[i], b'}' | b']') {
            return None;
        }
        let key_matches = if is_object {
            let key_end = skip_string(src, i)?;
            let key: String = serde_json::from_slice(&src[i..key_end]).ok()?;
            i = skip_ws(src, key_end);
            if src.get(i) != Some(&b':') {
                return None;
            }
            i = skip_ws(src, i + 1);
            key == comp
        } else {
            want_index == Some(idx)
        };
        let value_end = skip_value(src, i)?;
        if key_matches {
            return Some((i, value_end));
        }
        i = skip_ws(src, value_end);
        if src.get(i) == Some(&b',') {
            i = skip_ws(src, i + 1);
        }
        idx += 1;
    }
    None
}
