//! Tiny deterministic generator for differential tests of the fast paths: random JSON trees
//! emitted with varying whitespace and string escaping (canonical and non-canonical encodings).

/// xorshift64* pseudo-random source.
pub struct Gen(u64);

impl Gen {
    pub fn new(seed: u64) -> Self {
        Gen(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `0..n` (`n > 0`).
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() >> 33) as usize % n
    }

    /// True with probability `pct` percent.
    pub fn chance(&mut self, pct: usize) -> bool {
        self.below(100) < pct
    }

    pub fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len())]
    }
}

/// A JSON tree; `Raw` is emitted verbatim (numbers, literals, deliberately odd text).
#[derive(Clone)]
pub enum Json {
    Str(String),
    Raw(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn s(v: &str) -> Json {
        Json::Str(v.to_string())
    }

    pub fn obj(fields: Vec<(&str, Json)>) -> Json {
        Json::Obj(fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    /// Serializes with a random whitespace style and string-escaping style per document.
    pub fn emit(&self, g: &mut Gen) -> String {
        let style = Style { ws: g.below(3), escape: g.below(3) };
        let mut out = String::new();
        self.write(g, &style, &mut out);
        out
    }

    fn write(&self, g: &mut Gen, st: &Style, out: &mut String) {
        match self {
            Json::Raw(r) => out.push_str(r),
            Json::Str(s) => write_str(g, st, s, out),
            Json::Arr(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    st.space(out);
                    item.write(g, st, out);
                }
                st.space(out);
                out.push(']');
            }
            Json::Obj(fields) => {
                out.push('{');
                for (i, (k, v)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    st.space(out);
                    write_str(g, st, k, out);
                    out.push(':');
                    st.space(out);
                    v.write(g, st, out);
                }
                st.space(out);
                out.push('}');
            }
        }
    }
}

struct Style {
    /// 0 compact, 1 single spaces, 2 newlines.
    ws: usize,
    /// 0 canonical, 1 escape every non-ASCII char and `/`, 2 mixed per character.
    escape: usize,
}

impl Style {
    fn space(&self, out: &mut String) {
        match self.ws {
            1 => out.push(' '),
            2 => out.push_str("\n  "),
            _ => {}
        }
    }
}

fn write_str(g: &mut Gen, st: &Style, s: &str, out: &mut String) {
    if st.escape == 0 {
        out.push_str(&serde_json::to_string(s).unwrap_or_default());
        return;
    }
    out.push('"');
    for c in s.chars() {
        let aggressive = st.escape == 1 || g.chance(50);
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '/' if aggressive => out.push_str("\\/"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c if aggressive && !c.is_ascii() || (c as u32) == 0x7f && aggressive => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{:04X}", unit));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}
