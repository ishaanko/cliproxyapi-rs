//! The direct reader/writer must accept and emit exactly what serde_json does. Seed documents
//! are mutated (byte flips, inserts, deletes, truncation) and both outcomes are compared.

use crate::fast::{self, Fail};
use crate::Value;

#[test]
fn fast_reader_and_writer_match_serde_json() {
    let seeds: Vec<&[u8]> = vec![
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi \"x\" \\ \n\t\u0001 \u00e9 \ud83d\ude00 日本語"}}"#.as_bytes(),
        br#"{"a":[1,-2,3.5,-0.0,1e5,1E+5,2.5e-3,12345678901234567890,1.000,0,[],{}],"b":null,"c":true,"d":false,"a":9}"#,
        br#" [ { "k" : [ "v" , 1 ] } , "\/\b\f\r" , -1 ] "#,
        br#"{"deep":{"a":{"b":{"c":{"d":[[[[1]]]]}}}},"s":"","e":"\u0000\u001f\u007f"}"#,
        br#"[-0,-0.0,-0e1,0e0,1E400,-12345678901234567890123,18446744073709551616,9223372036854775808,-9223372036854775809,1.5E-7]"#,
        r#"{"s":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\"cc\\ éé日本語 dddddddddddddddddddddddddddddddddddddddddddddddd"}"#.as_bytes(),
        b"123",
        b"\"str\"",
        b"true",
        b"[]",
        b"{}",
        b"-",
        b"01",
        b"1.",
        b"1e",
        b"\"\\ud800\"",
        b"{\"a\":1,}",
        b"[1,]",
    ];
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let alphabet = br#"{}[]",:\ntrufalsenull-+.eE0123456789 u"#;
    let mut checked_ok = 0;
    for round in 0..400_000u32 {
        let mut doc = seeds[(next() % seeds.len() as u64) as usize].to_vec();
        for _ in 0..(round % 4) {
            if doc.is_empty() {
                break;
            }
            let at = (next() % doc.len() as u64) as usize;
            match next() % 5 {
                0 => doc[at] = alphabet[(next() % alphabet.len() as u64) as usize],
                1 => doc.insert(at, alphabet[(next() % alphabet.len() as u64) as usize]),
                2 => {
                    doc.remove(at);
                }
                3 => doc.truncate(at),
                _ => doc[at] = next() as u8,
            }
        }
        let serde = serde_json::from_slice::<Value>(&doc);
        match (fast::parse(&doc), serde) {
            (Ok(a), Ok(b)) => {
                let text = String::from_utf8_lossy(&doc);
                assert_eq!(serde_json::to_string(&a).unwrap(), serde_json::to_string(&b).unwrap(), "{text:?}");
                assert_eq!(fast::to_vec(&a), serde_json::to_vec(&b).unwrap(), "{text:?}");
                assert_eq!(fast::validate(&doc), Ok(()), "{text:?}");
                checked_ok += 1;
            }
            (Err(Fail::Syntax), Err(_)) => assert_eq!(fast::validate(&doc), Err(Fail::Syntax), "{:?}", String::from_utf8_lossy(&doc)),
            (Err(Fail::Deep | Fail::Special), _) => {}
            (a, b) => panic!("mismatch on {:?}: fast {:?} serde {:?}", String::from_utf8_lossy(&doc), a.map(|_| ()), b.map(|_| ())),
        }
    }
    assert!(checked_ok > 10_000, "too few valid documents exercised: {checked_ok}");
}
