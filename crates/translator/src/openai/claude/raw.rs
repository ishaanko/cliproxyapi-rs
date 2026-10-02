//! Byte-exact access to sub-values of a JSON document (gjson `Result.Raw`).
//!
//! `cpa_json` values are re-serialized compactly, but Go copies some sub-values into string
//! fields verbatim (e.g. a tool_use `input` becomes the `arguments` string with the client's
//! whitespace). This scanner finds the original text of one value without re-parsing.

/// One step of a lookup path.
#[derive(Clone, Copy)]
pub enum Seg<'a> {
    Key(&'a str),
    Index(usize),
}

/// The raw text of the value at `path`, or `None` when the path is absent or the document is
/// not well formed enough to scan.
pub fn raw_at<'a>(json: &'a [u8], path: &[Seg<'_>]) -> Option<&'a str> {
    let mut start = skip_ws(json, 0);
    for seg in path {
        start = match (json.get(start)?, seg) {
            (b'{', Seg::Key(want)) => find_member(json, start, want)?,
            (b'[', Seg::Index(want)) => find_element(json, start, *want)?,
            _ => return None,
        };
    }
    let end = value_end(json, start)?;
    std::str::from_utf8(&json[start..end]).ok()
}

fn skip_ws(json: &[u8], mut i: usize) -> usize {
    while json.get(i).is_some_and(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r')) {
        i += 1;
    }
    i
}

/// Index just past the string starting at `i` (which must be a `"`).
fn string_end(json: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while let Some(&b) = json.get(j) {
        match b {
            b'\\' => j += 2,
            b'"' => return Some(j + 1),
            _ => j += 1,
        }
    }
    None
}

/// Index just past the value starting at `i`.
fn value_end(json: &[u8], i: usize) -> Option<usize> {
    match *json.get(i)? {
        b'"' => string_end(json, i),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut j = i;
            while let Some(&b) = json.get(j) {
                match b {
                    b'"' => {
                        j = string_end(json, j)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
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
            while json.get(j).is_some_and(|b| !matches!(b, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')) {
                j += 1;
            }
            (j > i).then_some(j)
        }
    }
}

/// Start of the first member value named `want` in the object at `obj`.
fn find_member(json: &[u8], obj: usize, want: &str) -> Option<usize> {
    let mut i = skip_ws(json, obj + 1);
    while *json.get(i)? != b'}' {
        if *json.get(i)? != b'"' {
            return None;
        }
        let key_end = string_end(json, i)?;
        let key: String = serde_json::from_slice(&json[i..key_end]).ok()?;
        i = skip_ws(json, key_end);
        if *json.get(i)? != b':' {
            return None;
        }
        i = skip_ws(json, i + 1);
        if key == want {
            return Some(i);
        }
        i = skip_ws(json, value_end(json, i)?);
        if *json.get(i)? == b',' {
            i = skip_ws(json, i + 1);
        }
    }
    None
}

/// Start of element `want` of the array at `arr`.
fn find_element(json: &[u8], arr: usize, want: usize) -> Option<usize> {
    let mut i = skip_ws(json, arr + 1);
    let mut n = 0;
    while *json.get(i)? != b']' {
        if n == want {
            return Some(i);
        }
        i = skip_ws(json, value_end(json, i)?);
        if *json.get(i)? == b',' {
            i = skip_ws(json, i + 1);
        }
        n += 1;
    }
    None
}
