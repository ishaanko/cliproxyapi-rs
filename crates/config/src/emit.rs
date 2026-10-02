//! A block-style YAML emitter that lays text out like yaml.v3 (`yaml.Marshal` and an `Encoder`
//! with `SetIndent`), which `serde_yaml_ng` does not: indentation of sequences and of mappings
//! inside sequence items, quoting (`""` for empty strings, double quotes where yaml.v3 picks
//! them), literal blocks for multi-line strings, and no line folding.
//!
//! The reference edits `yaml.Node` trees, so a scalar keeps the quoting it had in the source file.
//! A `Value` tree has no such memory; [`Styles`] remembers, per string value, how the source
//! wrote it, which is what makes `"quoted"` survive a save (and a move to another key).

use std::collections::HashSet;
use std::panic::AssertUnwindSafe;

use serde_yaml_ng::{Mapping, Value};
use yaml_rust2::parser::{Event, MarkedEventReceiver, Parser};
use yaml_rust2::scanner::{Marker, TScalarStyle};

/// How a string scalar is written.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Style {
    Plain,
    Single,
    Double,
    Literal,
}

/// Quoting of the string scalars of a source document, keyed by their text.
#[derive(Debug, Clone, Default)]
pub(crate) struct Styles {
    double: HashSet<String>,
    single: HashSet<String>,
    literal: HashSet<String>,
}

struct StyleCollector(Styles);

impl MarkedEventReceiver for StyleCollector {
    fn on_event(&mut self, event: Event, _mark: Marker) {
        if let Event::Scalar(text, style, _, _) = event {
            let set = match style {
                TScalarStyle::DoubleQuoted => &mut self.0.double,
                TScalarStyle::SingleQuoted => &mut self.0.single,
                TScalarStyle::Literal | TScalarStyle::Folded => &mut self.0.literal,
                TScalarStyle::Plain => return,
            };
            set.insert(text);
        }
    }
}

impl Styles {
    /// Collects the quoting of every scalar of the first document of `text`. A document that does
    /// not parse yields no styles.
    pub(crate) fn from_text(text: &str) -> Self {
        let mut collector = StyleCollector(Styles::default());
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _ = Parser::new_from_str(text).load(&mut collector, false);
        }));
        collector.0
    }

    fn get(&self, s: &str) -> Option<Style> {
        if self.double.contains(s) {
            Some(Style::Double)
        } else if self.single.contains(s) {
            Some(Style::Single)
        } else if self.literal.contains(s) {
            Some(Style::Literal)
        } else {
            None
        }
    }

    /// Writes `to` the way the source wrote `from` (a scalar whose value was replaced in place
    /// keeps its quoting).
    pub(crate) fn inherit(&mut self, from: &str, to: &str) {
        match self.get(from) {
            Some(Style::Double) => self.double.insert(to.to_string()),
            Some(Style::Single) => self.single.insert(to.to_string()),
            Some(Style::Literal) => self.literal.insert(to.to_string()),
            _ => false,
        };
    }
}

// ---------------------------------------------------------------------------------------------
// scalar analysis (libyaml `yaml_emitter_analyze_scalar`)
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Analysis {
    multiline: bool,
    block_plain_allowed: bool,
    single_quoted_allowed: bool,
    block_allowed: bool,
}

fn is_printable(c: char) -> bool {
    matches!(c, '\n' | '\u{20}'..='\u{7e}' | '\u{85}' | '\u{a0}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
        && c != '\u{feff}'
}

fn is_break(c: char) -> bool {
    matches!(c, '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

fn is_blankz(c: Option<char>) -> bool {
    c.is_none_or(|c| c == ' ' || c == '\t' || is_break(c))
}

fn analyze(value: &str) -> Analysis {
    if value.is_empty() {
        return Analysis {
            multiline: false,
            block_plain_allowed: true,
            single_quoted_allowed: true,
            block_allowed: false,
        };
    }
    let mut block_indicators = false;
    let (mut line_breaks, mut special_characters) = (false, false);
    let (mut leading_space, mut leading_break, mut trailing_space, mut trailing_break) =
        (false, false, false, false);
    let (mut break_space, mut space_break) = (false, false);
    let (mut previous_space, mut previous_break) = (false, false);
    if value.starts_with("---") || value.starts_with("...") {
        block_indicators = true;
    }
    let chars: Vec<char> = value.chars().collect();
    let mut preceded_by_whitespace = true;
    for (i, &c) in chars.iter().enumerate() {
        let first = i == 0;
        let last = i + 1 == chars.len();
        let followed_by_whitespace = is_blankz(chars.get(i + 1).copied());
        if first {
            if "#,[]{}&*!|>'\"%@`".contains(c) {
                block_indicators = true;
            }
            if (c == '?' || c == ':') && followed_by_whitespace {
                block_indicators = true;
            }
            if c == '-' && followed_by_whitespace {
                block_indicators = true;
            }
        } else {
            if c == ':' && followed_by_whitespace {
                block_indicators = true;
            }
            if c == '#' && preceded_by_whitespace {
                block_indicators = true;
            }
        }
        if !is_printable(c) {
            special_characters = true;
        }
        if is_break(c) {
            line_breaks = true;
        }
        if c == ' ' {
            if first {
                leading_space = true;
            }
            if last {
                trailing_space = true;
            }
            if previous_break {
                break_space = true;
            }
            previous_space = true;
            previous_break = false;
        } else if is_break(c) {
            if first {
                leading_break = true;
            }
            if last {
                trailing_break = true;
            }
            if previous_space {
                space_break = true;
            }
            previous_space = false;
            previous_break = true;
        } else {
            previous_space = false;
            previous_break = false;
        }
        preceded_by_whitespace = is_blankz(Some(c));
    }
    let mut a = Analysis {
        multiline: line_breaks,
        block_plain_allowed: true,
        single_quoted_allowed: true,
        block_allowed: true,
    };
    if leading_space || leading_break || trailing_space || trailing_break {
        a.block_plain_allowed = false;
    }
    if trailing_space {
        a.block_allowed = false;
    }
    if break_space {
        a.block_plain_allowed = false;
        a.single_quoted_allowed = false;
    }
    if space_break || special_characters {
        a.block_plain_allowed = false;
        a.single_quoted_allowed = false;
        a.block_allowed = false;
    }
    if line_breaks {
        a.block_plain_allowed = false;
    }
    if block_indicators {
        a.block_plain_allowed = false;
    }
    a
}

/// yaml.v3 `isBase60Float`: `190:20:30.15` style sexagesimal floats.
fn is_base60_float(s: &str) -> bool {
    let body = s.strip_prefix(['-', '+']).unwrap_or(s);
    let (head, frac) = match body.split_once('.') {
        Some((h, f)) => (h, Some(f)),
        None => (body, None),
    };
    if let Some(f) = frac
        && !f.bytes().all(|b| b.is_ascii_digit() || b == b'_')
    {
        return false;
    }
    let mut parts = head.split(':');
    let Some(first) = parts.next() else {
        return false;
    };
    if first.is_empty()
        || !first.as_bytes()[0].is_ascii_digit()
        || !first.bytes().all(|b| b.is_ascii_digit() || b == b'_')
    {
        return false;
    }
    let rest: Vec<&str> = parts.collect();
    !rest.is_empty()
        && rest.iter().all(|p| {
            matches!(p.len(), 1 | 2)
                && p.bytes().all(|b| b.is_ascii_digit())
                && p.parse::<u8>().is_ok_and(|n| n <= 59)
        })
}

/// yaml.v3 `isOldBool`: the YAML 1.1 spellings that old parsers read as booleans.
fn is_old_bool(s: &str) -> bool {
    matches!(
        s,
        "y" | "Y"
            | "yes"
            | "Yes"
            | "YES"
            | "on"
            | "On"
            | "ON"
            | "n"
            | "N"
            | "no"
            | "No"
            | "NO"
            | "off"
            | "Off"
            | "OFF"
    )
}

/// Whether a string that would resolve to a timestamp when unquoted (`2024-01-02`).
fn looks_like_timestamp(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 8 || !b[..4].iter().all(u8::is_ascii_digit) || b[4] != b'-' {
        return false;
    }
    let (date, time) = match s.find(['T', 't', ' ']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let nums: Vec<&str> = date.split('-').collect();
    let digits =
        |p: &str, max: usize| (1..=max).contains(&p.len()) && p.bytes().all(|c| c.is_ascii_digit());
    if nums.len() != 3 || !digits(nums[0], 4) || !digits(nums[1], 2) || !digits(nums[2], 2) {
        return false;
    }
    match time {
        None => true,
        Some(t) => {
            let t = t.trim_end_matches(['Z', 'z']);
            let t = t.split(['+', '-']).next().unwrap_or(t).trim_end();
            let clock = t.split('.').next().unwrap_or(t);
            let parts: Vec<&str> = clock.split(':').collect();
            parts.len() == 3 && parts.iter().all(|p| digits(p, 2))
        }
    }
}

/// yaml.v3 `canUsePlain`: the string resolves to a string when unquoted.
fn can_use_plain(s: &str) -> bool {
    if s == "<<" || is_base60_float(s) || is_old_bool(s) || looks_like_timestamp(s) {
        return false;
    }
    matches!(
        crate::rawparse::resolved(&crate::rawparse::resolve_plain(s)),
        Value::String(_)
    )
}

// ---------------------------------------------------------------------------------------------
// emitter
// ---------------------------------------------------------------------------------------------

/// Go's `strconv.FormatFloat(f, 'g', -1, 64)` with yaml.v3's spellings for the specials.
fn format_float(f: f64) -> String {
    if f.is_nan() {
        return ".nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 {
            ".inf".into()
        } else {
            "-.inf".into()
        };
    }
    if f == 0.0 {
        return if f.is_sign_negative() {
            "-0".into()
        } else {
            "0".into()
        };
    }
    // Shortest round-trip digits and exponent from Rust's `{:e}`.
    let sci = format!("{f:e}");
    let Some((mantissa, exp)) = sci.split_once('e') else {
        return sci;
    };
    let exp: i32 = exp.parse().unwrap_or(0);
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let sign = if negative { "-" } else { "" };
    if !(-4..6).contains(&exp) {
        let frac = if digits.len() > 1 {
            format!(".{}", &digits[1..])
        } else {
            String::new()
        };
        let esign = if exp < 0 { '-' } else { '+' };
        format!("{sign}{}{frac}e{esign}{:02}", &digits[..1], exp.abs())
    } else if exp >= 0 {
        let int_len = exp as usize + 1;
        if digits.len() <= int_len {
            format!("{sign}{digits}{}", "0".repeat(int_len - digits.len()))
        } else {
            format!("{sign}{}.{}", &digits[..int_len], &digits[int_len..])
        }
    } else {
        format!("{sign}0.{}{digits}", "0".repeat((-exp - 1) as usize))
    }
}

struct Emitter<'a> {
    out: String,
    best_indent: i32,
    styles: &'a Styles,
}

/// How a scalar is written.
enum Rendered {
    Inline(String),
    /// A literal block: header hints (`-`, `+`, indent digit) and the content.
    Block {
        hints: String,
        content: String,
    },
}

impl<'a> Emitter<'a> {
    /// libyaml's `increase_indent` as patched in yaml.v3.
    fn increase(&self, indent: i32, in_seq_item: bool) -> i32 {
        if indent < 0 {
            0
        } else if in_seq_item {
            indent + 2
        } else {
            self.best_indent * ((indent + self.best_indent) / self.best_indent)
        }
    }

    fn newline_indent(&mut self, indent: i32) {
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push('\n');
        }
        for _ in 0..indent.max(0) {
            self.out.push(' ');
        }
    }

    fn requested(&self, s: &str) -> Style {
        if let Some(style) = self.styles.get(s) {
            style
        } else if s.contains('\n') {
            Style::Literal
        } else if !can_use_plain(s) {
            Style::Double
        } else {
            Style::Plain
        }
    }

    fn string(&self, s: &str, simple_key: bool) -> Rendered {
        let a = analyze(s);
        let mut style = if simple_key && a.multiline {
            Style::Double
        } else {
            self.requested(s)
        };
        if style == Style::Plain && (!a.block_plain_allowed || (s.is_empty() && simple_key)) {
            style = Style::Single;
        }
        if style == Style::Single && !a.single_quoted_allowed {
            style = Style::Double;
        }
        if style == Style::Literal && (!a.block_allowed || simple_key) {
            style = Style::Double;
        }
        match style {
            Style::Plain => Rendered::Inline(s.to_string()),
            Style::Single => Rendered::Inline(format!("'{}'", s.replace('\'', "''"))),
            Style::Double => Rendered::Inline(double_quoted(s)),
            Style::Literal => Rendered::Block {
                hints: block_hints(s, self.best_indent),
                content: s.to_string(),
            },
        }
    }

    fn scalar(&self, v: &Value, simple_key: bool) -> Rendered {
        match v {
            Value::Null => Rendered::Inline("null".into()),
            Value::Bool(b) => Rendered::Inline(b.to_string()),
            Value::Number(n) => Rendered::Inline(match n.as_f64() {
                Some(f) if n.is_f64() => format_float(f),
                _ => n.to_string(),
            }),
            Value::String(s) => self.string(s, simple_key),
            Value::Tagged(_) => match crate::rawparse::raw_text(v) {
                Some(raw) => Rendered::Inline(raw.to_string()),
                None => match crate::rawparse::resolved(v) {
                    Value::Tagged(t) => self.scalar(&t.value, simple_key),
                    inner => self.scalar(inner, simple_key),
                },
            },
            Value::Sequence(_) | Value::Mapping(_) => Rendered::Inline(String::new()),
        }
    }

    /// Writes a scalar after the text already on the line (`key:` or `-`), `block_indent` being
    /// where the lines of a literal block go.
    fn put_scalar(&mut self, v: &Value, block_indent: i32) {
        match self.scalar(v, false) {
            Rendered::Inline(text) => {
                if !text.is_empty() {
                    self.out.push(' ');
                    self.out.push_str(&text);
                }
            }
            Rendered::Block { hints, content } => {
                self.out.push_str(" |");
                self.out.push_str(&hints);
                self.out.push('\n');
                for piece in content.split_inclusive('\n') {
                    let line = piece.strip_suffix('\n');
                    let text = line.unwrap_or(piece);
                    if !text.is_empty() {
                        for _ in 0..block_indent.max(0) {
                            self.out.push(' ');
                        }
                        self.out.push_str(text);
                    }
                    if line.is_some() {
                        self.out.push('\n');
                    }
                }
            }
        }
    }

    fn put_key(&mut self, k: &Value) {
        match self.scalar(k, true) {
            Rendered::Inline(t) => self.out.push_str(&t),
            Rendered::Block { content, .. } => self.out.push_str(&double_quoted(&content)),
        }
    }

    /// `parent_indent` is the indent of the enclosing collection (-1 at the root).
    fn mapping(
        &mut self,
        map: &Mapping,
        parent_indent: i32,
        in_seq_item: bool,
        inline_first: bool,
    ) {
        let indent = self.increase(parent_indent, in_seq_item);
        for (i, (k, v)) in map.iter().enumerate() {
            if !(i == 0 && inline_first) {
                self.newline_indent(indent);
            }
            self.put_key(k);
            self.out.push(':');
            self.value_after_key(v, indent);
        }
    }

    fn value_after_key(&mut self, v: &Value, map_indent: i32) {
        match v {
            Value::Mapping(m) if m.is_empty() => self.out.push_str(" {}"),
            Value::Sequence(s) if s.is_empty() => self.out.push_str(" []"),
            Value::Mapping(m) => self.mapping(m, map_indent, false, false),
            Value::Sequence(s) => self.sequence(s, map_indent, false, false),
            other => {
                let block_indent = self.increase(map_indent, false);
                self.put_scalar(other, block_indent);
            }
        }
    }

    fn sequence(
        &mut self,
        items: &[Value],
        parent_indent: i32,
        in_seq_item: bool,
        inline_first: bool,
    ) {
        let indent = self.increase(parent_indent, in_seq_item);
        for (i, item) in items.iter().enumerate() {
            if !(i == 0 && inline_first) {
                self.newline_indent(indent);
            }
            self.out.push('-');
            match item {
                Value::Mapping(m) if m.is_empty() => self.out.push_str(" {}"),
                Value::Sequence(s) if s.is_empty() => self.out.push_str(" []"),
                Value::Mapping(m) => {
                    self.out.push(' ');
                    self.mapping(m, indent, true, true);
                }
                Value::Sequence(s) => {
                    self.out.push(' ');
                    self.sequence(s, indent, true, true);
                }
                other => {
                    let block_indent = self.increase(indent, true);
                    self.put_scalar(other, block_indent);
                }
            }
        }
    }
}

/// libyaml `yaml_emitter_write_block_scalar_hints`.
fn block_hints(s: &str, best_indent: i32) -> String {
    let mut hints = String::new();
    if s.starts_with(' ') || s.chars().next().is_some_and(is_break) {
        hints.push_str(&best_indent.to_string());
    }
    let chars: Vec<char> = s.chars().collect();
    match chars.last() {
        None => hints.push('-'),
        Some(&last) if !is_break(last) => hints.push('-'),
        Some(_) => {
            if chars.len() == 1 || chars.get(chars.len() - 2).is_some_and(|c| is_break(*c)) {
                hints.push('+');
            }
        }
    }
    hints
}

/// libyaml `yaml_emitter_write_double_quoted_scalar` (unicode output on).
fn double_quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if !is_printable(c) || is_break(c) || c == '"' || c == '\\' || c == '\u{feff}' {
            out.push('\\');
            match c {
                '\0' => out.push('0'),
                '\u{7}' => out.push('a'),
                '\u{8}' => out.push('b'),
                '\t' => out.push('t'),
                '\n' => out.push('n'),
                '\u{b}' => out.push('v'),
                '\u{c}' => out.push('f'),
                '\r' => out.push('r'),
                '\u{1b}' => out.push('e'),
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '\u{85}' => out.push('N'),
                '\u{a0}' => out.push('_'),
                '\u{2028}' => out.push('L'),
                '\u{2029}' => out.push('P'),
                c if (c as u32) <= 0xff => out.push_str(&format!("x{:02X}", c as u32)),
                c if (c as u32) <= 0xffff => out.push_str(&format!("u{:04X}", c as u32)),
                c => out.push_str(&format!("U{:08X}", c as u32)),
            }
        } else {
            out.push(c);
        }
    }
    out.push('"');
    out
}

/// Renders `root` as a YAML document in yaml.v3 layout with the given indentation step (4 for
/// `yaml.Marshal`, 2 for the encoders that call `SetIndent(2)`).
pub(crate) fn emit(root: &Value, indent: usize, styles: &Styles) -> String {
    let mut e = Emitter {
        out: String::new(),
        best_indent: indent.max(1) as i32,
        styles,
    };
    match root {
        Value::Mapping(m) if !m.is_empty() => e.mapping(m, -1, false, false),
        Value::Sequence(s) if !s.is_empty() => e.sequence(s, -1, false, false),
        Value::Mapping(_) => e.out.push_str("{}"),
        Value::Sequence(_) => e.out.push_str("[]"),
        scalar => match e.scalar(scalar, false) {
            Rendered::Inline(t) => e.out.push_str(&t),
            Rendered::Block { content, .. } => e.out.push_str(&double_quoted(&content)),
        },
    }
    if !e.out.ends_with('\n') {
        e.out.push('\n');
    }
    e.out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Value {
        crate::rawparse::parse_first_document(text)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn indentation_matches_yaml_v3() {
        let text = "access:\n  api-keys:\n    - a\n    - b\ngroups:\n  - name: x\n    keys:\n      - api-key: k\n";
        let v = parse(text);
        assert_eq!(
            emit(&v, 4, &Styles::default()),
            "access:\n    api-keys:\n        - a\n        - b\ngroups:\n    - name: x\n      keys:\n        - api-key: k\n"
        );
        assert_eq!(emit(&v, 2, &Styles::default()), text);
    }

    #[test]
    fn quoting_follows_the_source_and_yaml_v3_rules() {
        let src = "host: \"127.0.0.1\"\nname: plain\n";
        let styles = Styles::from_text(src);
        let v = parse(
            "host: 127.0.0.1\nname: plain\nempty: ''\nnum: '123'\nflag: 'yes'\ncolon: 'a: b'\n",
        );
        assert_eq!(
            emit(&v, 2, &styles),
            "host: \"127.0.0.1\"\nname: plain\nempty: \"\"\nnum: \"123\"\nflag: \"yes\"\ncolon: 'a: b'\n"
        );
    }

    #[test]
    fn multiline_strings_use_literal_blocks() {
        let text = "a: |\n  one\n  two\nb: |-\n  x\n";
        assert_eq!(emit(&parse(text), 2, &Styles::from_text(text)), text);
    }

    #[test]
    fn floats_follow_go_g_format() {
        assert_eq!(format_float(0.5), "0.5");
        assert_eq!(format_float(3.0), "3");
        assert_eq!(format_float(1e21), "1e+21");
        assert_eq!(format_float(1234567.0), "1.234567e+06");
        assert_eq!(format_float(123456.0), "123456");
        assert_eq!(format_float(0.00001), "1e-05");
    }
}
