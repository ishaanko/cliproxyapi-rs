//! Go's `encoding/json` syntax errors, for responses that echo `err.Error()` to the client.
//!
//! [`check_valid`] is a port of the decoder's scanner state machine (`scanner.go`); only the
//! messages matter, so it validates without building anything.

#[derive(Clone, Copy, PartialEq)]
enum Ps {
    ObjectKey,
    ObjectValue,
    ArrayValue,
}

#[derive(Clone, Copy)]
enum St {
    BeginValueOrEmpty,
    BeginValue,
    BeginStringOrEmpty,
    BeginString,
    EndValue,
    EndTop,
    InString,
    InStringEsc,
    InStringEscU(u8),
    Neg,
    Zero,
    One,
    Dot,
    Dot0,
    E,
    ESign,
    E0,
    /// Inside `true`/`false`/`null`: the remaining expected bytes.
    Literal(&'static str, &'static str),
}

/// Go's `quoteChar`.
fn quote_char(c: u8) -> String {
    match c {
        b'\'' => "'\\''".to_string(),
        b'"' => "'\"'".to_string(),
        c if c.is_ascii_graphic() || c == b' ' => format!("'{}'", c as char),
        b'\n' => "'\\n'".to_string(),
        b'\r' => "'\\r'".to_string(),
        b'\t' => "'\\t'".to_string(),
        c if c < 0x20 || c == 0x7f => format!("'\\x{c:02x}'"),
        // Non-ASCII: the scanner sees single bytes, which Go quotes as the byte's rune.
        c => {
            let s = format!("{:?}", c as char);
            format!("'{}'", &s[1..s.len() - 1])
        }
    }
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\r' | b'\n')
}

struct Scanner {
    step: St,
    stack: Vec<Ps>,
    end_top: bool,
}

type Step = Result<(), String>;

impl Scanner {
    fn err(c: u8, context: &str) -> Step {
        Err(format!("invalid character {} {context}", quote_char(c)))
    }

    fn push(&mut self, ps: Ps) {
        self.stack.push(ps);
    }

    /// `stateEndValue` after the enclosing value ended.
    fn end_value(&mut self, c: u8) -> Step {
        let Some(&top) = self.stack.last() else {
            self.step = St::EndTop;
            self.end_top = true;
            return self.feed(c);
        };
        if is_space(c) {
            self.step = St::EndValue;
            return Ok(());
        }
        let n = self.stack.len();
        match top {
            Ps::ObjectKey => {
                if c == b':' {
                    self.stack[n - 1] = Ps::ObjectValue;
                    self.step = St::BeginValue;
                    return Ok(());
                }
                Self::err(c, "after object key")
            }
            Ps::ObjectValue => {
                if c == b',' {
                    self.stack[n - 1] = Ps::ObjectKey;
                    self.step = St::BeginString;
                    return Ok(());
                }
                if c == b'}' {
                    self.stack.pop();
                    self.step = St::EndValue;
                    return Ok(());
                }
                Self::err(c, "after object key:value pair")
            }
            Ps::ArrayValue => {
                if c == b',' {
                    self.step = St::BeginValue;
                    return Ok(());
                }
                if c == b']' {
                    self.stack.pop();
                    self.step = St::EndValue;
                    return Ok(());
                }
                Self::err(c, "after array element")
            }
        }
    }

    fn begin_value(&mut self, c: u8) -> Step {
        if is_space(c) {
            return Ok(());
        }
        match c {
            b'{' => {
                self.step = St::BeginStringOrEmpty;
                self.push(Ps::ObjectKey);
                Ok(())
            }
            b'[' => {
                self.step = St::BeginValueOrEmpty;
                self.push(Ps::ArrayValue);
                Ok(())
            }
            b'"' => {
                self.step = St::InString;
                Ok(())
            }
            b'-' => {
                self.step = St::Neg;
                Ok(())
            }
            b'0' => {
                self.step = St::Zero;
                Ok(())
            }
            b't' => {
                self.step = St::Literal("rue", "true");
                Ok(())
            }
            b'f' => {
                self.step = St::Literal("alse", "false");
                Ok(())
            }
            b'n' => {
                self.step = St::Literal("ull", "null");
                Ok(())
            }
            b'1'..=b'9' => {
                self.step = St::One;
                Ok(())
            }
            _ => Self::err(c, "looking for beginning of value"),
        }
    }

    fn feed(&mut self, c: u8) -> Step {
        match self.step {
            St::BeginValueOrEmpty => {
                if is_space(c) {
                    return Ok(());
                }
                if c == b']' {
                    return self.end_value(c);
                }
                self.begin_value(c)
            }
            St::BeginValue => self.begin_value(c),
            St::BeginStringOrEmpty => {
                if is_space(c) {
                    return Ok(());
                }
                if c == b'}' {
                    let n = self.stack.len();
                    self.stack[n - 1] = Ps::ObjectValue;
                    return self.end_value(c);
                }
                self.step = St::BeginString;
                self.feed(c)
            }
            St::BeginString => {
                if is_space(c) {
                    return Ok(());
                }
                if c == b'"' {
                    self.step = St::InString;
                    return Ok(());
                }
                Self::err(c, "looking for beginning of object key string")
            }
            St::EndValue => self.end_value(c),
            St::EndTop => {
                if !is_space(c) {
                    return Self::err(c, "after top-level value");
                }
                Ok(())
            }
            St::InString => {
                match c {
                    b'"' => self.step = St::EndValue,
                    b'\\' => self.step = St::InStringEsc,
                    c if c < 0x20 => return Self::err(c, "in string literal"),
                    _ => {}
                }
                Ok(())
            }
            St::InStringEsc => match c {
                b'b' | b'f' | b'n' | b'r' | b't' | b'\\' | b'/' | b'"' => {
                    self.step = St::InString;
                    Ok(())
                }
                b'u' => {
                    self.step = St::InStringEscU(0);
                    Ok(())
                }
                _ => Self::err(c, "in string escape code"),
            },
            St::InStringEscU(n) => {
                if !c.is_ascii_hexdigit() {
                    return Self::err(c, "in \\u hexadecimal character escape");
                }
                self.step = if n == 3 {
                    St::InString
                } else {
                    St::InStringEscU(n + 1)
                };
                Ok(())
            }
            St::Neg => match c {
                b'0' => {
                    self.step = St::Zero;
                    Ok(())
                }
                b'1'..=b'9' => {
                    self.step = St::One;
                    Ok(())
                }
                _ => Self::err(c, "in numeric literal"),
            },
            St::One => {
                if c.is_ascii_digit() {
                    return Ok(());
                }
                self.zero(c)
            }
            St::Zero => self.zero(c),
            St::Dot => {
                if c.is_ascii_digit() {
                    self.step = St::Dot0;
                    return Ok(());
                }
                Self::err(c, "after decimal point in numeric literal")
            }
            St::Dot0 => {
                if c.is_ascii_digit() {
                    return Ok(());
                }
                if c == b'e' || c == b'E' {
                    self.step = St::E;
                    return Ok(());
                }
                self.end_value(c)
            }
            St::E => {
                if c == b'+' || c == b'-' {
                    self.step = St::ESign;
                    return Ok(());
                }
                self.step = St::ESign;
                self.feed(c)
            }
            St::ESign => {
                if c.is_ascii_digit() {
                    self.step = St::E0;
                    return Ok(());
                }
                Self::err(c, "in exponent of numeric literal")
            }
            St::E0 => {
                if c.is_ascii_digit() {
                    return Ok(());
                }
                self.end_value(c)
            }
            St::Literal(rest, word) => {
                let expect = rest.as_bytes()[0];
                if c != expect {
                    return Self::err(
                        c,
                        &format!("in literal {word} (expecting '{}')", expect as char),
                    );
                }
                self.step = if rest.len() == 1 {
                    St::EndValue
                } else {
                    St::Literal(&rest[1..], word)
                };
                Ok(())
            }
        }
    }

    fn zero(&mut self, c: u8) -> Step {
        if c == b'.' {
            self.step = St::Dot;
            return Ok(());
        }
        if c == b'e' || c == b'E' {
            self.step = St::E;
            return Ok(());
        }
        self.end_value(c)
    }
}

/// `Ok` when `data` is one valid JSON value, else Go's `SyntaxError` text.
pub(crate) fn check_valid(data: &[u8]) -> Result<(), String> {
    let mut s = Scanner {
        step: St::BeginValue,
        stack: Vec::new(),
        end_top: false,
    };
    for &c in data {
        s.feed(c)?;
    }
    // EOF is a trailing space; the value must be complete by then.
    s.feed(b' ')?;
    if s.end_top {
        Ok(())
    } else {
        Err("unexpected end of JSON input".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::check_valid;

    fn msg(s: &str) -> String {
        check_valid(s.as_bytes()).unwrap_err()
    }

    #[test]
    fn go_messages() {
        assert_eq!(
            msg("{broken"),
            "invalid character 'b' looking for beginning of object key string"
        );
        assert_eq!(msg("{\"a\":"), "unexpected end of JSON input");
        assert_eq!(msg(""), "unexpected end of JSON input");
        assert_eq!(msg("{\"a\" 1}"), "invalid character '1' after object key");
        assert_eq!(msg("[1 2]"), "invalid character '2' after array element");
        assert_eq!(msg("{} x"), "invalid character 'x' after top-level value");
        assert_eq!(msg("tru!"), "invalid character '!' in literal true (expecting 'e')");
        assert!(check_valid(br#"{"a":[1,2.5e3,"xy",null,true]}"#).is_ok());
    }
}
