//! The one-pass plain-path walk in `get` must agree with the compiled-path evaluator.

use crate::{eval, get, parse_comps, parse_str, Value};

#[test]
fn plain_walk_matches_compiled_paths() {
    let docs: Vec<Value> = [
        r#"{"a":{"b":[1,{"c":"x"}],"d.e":2},"0":"zero","":"empty","arr":[[1,2],[3]],"n":null,"s":"str","a b":7}"#,
        r#"[{"k":1},{"k":2},"s",[true]]"#,
        r#"{"x":{"y":{"z":{"w":[0,1,2]}}}}"#,
        r#"7"#,
    ]
    .iter()
    .map(|d| parse_str(d))
    .collect();
    let parts = ["a", "b", "0", "1", "c", "d.e", "", "arr", "n", "s", "k", "x", "y", "z", "w", "a b", "#", "#(k==1)", "*", "?", "@this", "@type", "a\\.b", "|", "\"q\"", "-1", "99", "é"];
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..40_000 {
        let n = 1 + (next() % 4) as usize;
        let path: Vec<&str> = (0..n).map(|_| parts[(next() % parts.len() as u64) as usize]).collect();
        let path = path.join(if next() % 8 == 0 { "|" } else { "." });
        for doc in &docs {
            let fast = get(doc, &path);
            let slow = if path.is_empty() { None } else { eval(doc, &parse_comps(&path)) };
            assert_eq!(fast.v().cloned(), slow.map(|c| c.into_owned()), "path {path:?} doc {doc}");
        }
    }
}
