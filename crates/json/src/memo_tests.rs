//! The per-request memo and `parse_valid` must never change results.

use crate::{parse, parse_uncached, parse_valid, scope_sync, to_string, valid};

/// Inside a scope, repeated parses/validity checks of a large document reuse earlier work and
/// still return exactly what an uncached call does; outside a scope nothing changes.
#[test]
fn memo_scope_returns_same_results() {
    let big = format!(r#"{{"pad":"{}","list":[1,2.5e3,"x"],"n":null}}"#, "abc\\n".repeat(20_000));
    let broken = format!(r#"{{"pad":"{}","list":[1,"#, "abc".repeat(20_000));
    let expect_big = to_string(&parse_uncached(big.as_bytes()));
    let expect_broken = to_string(&parse_uncached(broken.as_bytes()));
    let run = || {
        for _ in 0..4 {
            assert_eq!(to_string(&parse(big.as_bytes())), expect_big);
            assert_eq!(to_string(&parse(broken.as_bytes())), expect_broken);
            assert!(valid(big.as_bytes()));
            assert!(!valid(broken.as_bytes()));
            assert!(parse_valid(big.as_bytes()).is_some());
            assert!(parse_valid(broken.as_bytes()).is_none());
        }
    };
    run();
    scope_sync(run);
}

/// `parse_valid` is `valid` + `parse` for every kind of input.
#[test]
fn parse_valid_matches_valid_then_parse() {
    let docs: [&[u8]; 8] = [b"", b"{}", b"[1,2", b"\"x\"", b"nul", b"{\"a\":\"\\ud800\"}", b"{\"$serde_json::private::Number\":\"1\"}", b"  [ ]  "];
    for doc in docs {
        let expect = valid(doc).then(|| parse(doc));
        assert_eq!(parse_valid(doc).map(|v| to_string(&v)), expect.map(|v| to_string(&v)), "{doc:?}");
    }
    let deep = format!("{}1{}", "[".repeat(300), "]".repeat(300));
    assert!(parse_valid(deep.as_bytes()).is_some());
}

/// Serialized output is remembered as valid and parses back to the same value on every
/// sighting, including after eviction by other large documents.
#[test]
fn memo_handles_serialized_output_and_eviction() {
    scope_sync(|| {
        let docs: Vec<String> = (0..6).map(|i| format!(r#"{{"id":{i},"pad":"{}"}}"#, "x\\t".repeat(15_000 + i))).collect();
        let values: Vec<_> = docs.iter().map(|d| parse_uncached(d.as_bytes())).collect();
        for round in 0..3 {
            for (doc, value) in docs.iter().zip(&values) {
                let out = crate::to_vec(value);
                assert_eq!(out, doc.as_bytes(), "round {round}");
                assert!(valid(&out));
                assert_eq!(to_string(&parse(&out)), *doc);
                assert_eq!(to_string(&parse(&out)), *doc);
            }
        }
    });
}
