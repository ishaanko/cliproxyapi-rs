//! Streaming decoder for the `input` field of `apply_patch` function-call arguments (Go:
//! common/apply_patch_input.go).

use cpa_core::applypatch;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Phase {
    #[default]
    BeforeObject,
    BeforeKey,
    InKey,
    BeforeColon,
    BeforeValue,
    InValue,
    AfterValue,
    Complete,
}

/// Decodes the `input` string from streamed function arguments. The arguments must be exactly
/// `{"input":"..."}`. Fragments are scanned once and only complete, validated characters are
/// emitted; whitespace in the patch text is preserved. A decoder belongs to one call.
#[derive(Debug, Default)]
pub struct ApplyPatchInputDecoder {
    phase: Phase,
    key_raw: Vec<u8>,
    escape_raw: Vec<u8>,
    utf8_pending: Vec<u8>,
    high_surrogate: u16,
    input: String,
    finished: bool,
    err: Option<String>,
}

fn json_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\r' | b'\n')
}

fn hex_digit(c: u8) -> Option<u16> {
    char::from(c).to_digit(16).map(|d| d as u16)
}

impl ApplyPatchInputDecoder {
    /// Scans one fragment (UTF-8 may be split across fragments); returns the newly decoded input
    /// text. Any error is sticky: later calls return it again.
    pub fn push(&mut self, fragment: impl AsRef<[u8]>) -> Result<String, String> {
        let fragment = fragment.as_ref();
        if let Some(err) = &self.err {
            return Err(err.clone());
        }
        if self.finished {
            if fragment.is_empty() {
                return Ok(String::new());
            }
            return Err(self.fail("apply_patch arguments received after completion"));
        }
        let start = self.input.len();
        for &c in fragment {
            match self.phase {
                Phase::BeforeObject => {
                    if json_space(c) {
                        continue;
                    }
                    if c != b'{' {
                        return Err(self.fail("apply_patch arguments must be a JSON object"));
                    }
                    self.phase = Phase::BeforeKey;
                }
                Phase::BeforeKey => {
                    if json_space(c) {
                        continue;
                    }
                    if c != b'"' {
                        return Err(self.fail("apply_patch arguments must contain the input field"));
                    }
                    self.key_raw.push(c);
                    self.phase = Phase::InKey;
                }
                Phase::InKey => {
                    self.key_raw.push(c);
                    if !self.escape_raw.is_empty() {
                        self.escape_raw.clear();
                        continue;
                    }
                    if c == b'\\' {
                        self.escape_raw.push(c);
                        continue;
                    }
                    if c < 0x20 {
                        return Err(self.fail("invalid control character in apply_patch input key"));
                    }
                    if c == b'"' {
                        // Go's json.Unmarshal replaces invalid UTF-8 rather than failing.
                        let quoted = String::from_utf8_lossy(&self.key_raw).into_owned();
                        let key = match serde_json::from_str::<String>(&quoted) {
                            Ok(key) => key,
                            Err(err) => {
                                return Err(self.fail(format!("decode apply_patch input key: {err}")));
                            }
                        };
                        if key != "input" {
                            return Err(self.fail("apply_patch arguments must contain the input field"));
                        }
                        self.key_raw = Vec::new();
                        self.phase = Phase::BeforeColon;
                    }
                }
                Phase::BeforeColon => {
                    if json_space(c) {
                        continue;
                    }
                    if c != b':' {
                        return Err(self.fail("apply_patch input key must be followed by a colon"));
                    }
                    self.phase = Phase::BeforeValue;
                }
                Phase::BeforeValue => {
                    if json_space(c) {
                        continue;
                    }
                    if c != b'"' {
                        return Err(self.fail("apply_patch input must be a string"));
                    }
                    self.phase = Phase::InValue;
                }
                Phase::InValue => {
                    if let Err(err) = self.consume_value(c) {
                        return Err(self.fail(err));
                    }
                }
                Phase::AfterValue => {
                    if json_space(c) {
                        continue;
                    }
                    if c != b'}' {
                        return Err(self.fail("apply_patch arguments must contain only one input field"));
                    }
                    self.phase = Phase::Complete;
                }
                Phase::Complete => {
                    if !json_space(c) {
                        return Err(self.fail("apply_patch arguments must not contain trailing JSON"));
                    }
                }
            }
        }
        Ok(self.input[start..].to_string())
    }

    fn consume_value(&mut self, c: u8) -> Result<(), &'static str> {
        if !self.utf8_pending.is_empty() || c >= 0x80 {
            // Pending escapes and surrogate pairs cannot consume raw UTF-8.
            if !self.escape_raw.is_empty() || self.high_surrogate != 0 {
                return Err("invalid Unicode escape in apply_patch input");
            }
            self.utf8_pending.push(c);
            return match std::str::from_utf8(&self.utf8_pending) {
                Ok(text) => {
                    self.input.push_str(text);
                    self.utf8_pending.clear();
                    Ok(())
                }
                // A truncated sequence: wait for more bytes.
                Err(err) if err.error_len().is_none() => Ok(()),
                Err(_) => Err("invalid UTF-8 in apply_patch input"),
            };
        }
        if !self.escape_raw.is_empty() {
            self.escape_raw.push(c);
            if self.escape_raw.len() == 2 {
                if self.high_surrogate != 0 && c != b'u' {
                    return Err("apply_patch input high surrogate requires a low surrogate");
                }
                let decoded = match c {
                    b'u' => return Ok(()),
                    b'"' | b'\\' | b'/' => c,
                    b'b' => 0x08,
                    b'f' => 0x0c,
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    _ => return Err("invalid escape in apply_patch input"),
                };
                self.input.push(char::from(decoded));
                self.escape_raw.clear();
                return Ok(());
            }
            if hex_digit(c).is_none() {
                return Err("invalid Unicode escape in apply_patch input");
            }
            if self.escape_raw.len() < 6 {
                return Ok(());
            }
            let code = self.escape_raw[2..]
                .iter()
                .fold(0u16, |acc, &digit| (acc << 4) | hex_digit(digit).unwrap_or(0));
            self.escape_raw.clear();
            if self.high_surrogate != 0 {
                if !(0xdc00..=0xdfff).contains(&code) {
                    return Err("apply_patch input high surrogate requires a low surrogate");
                }
                let decoded = char::decode_utf16([self.high_surrogate, code])
                    .next()
                    .and_then(Result::ok)
                    .unwrap_or(char::REPLACEMENT_CHARACTER);
                self.input.push(decoded);
                self.high_surrogate = 0;
            } else if (0xd800..=0xdbff).contains(&code) {
                self.high_surrogate = code;
            } else if (0xdc00..=0xdfff).contains(&code) {
                return Err("unpaired low surrogate in apply_patch input");
            } else {
                self.input
                    .push(char::from_u32(u32::from(code)).unwrap_or(char::REPLACEMENT_CHARACTER));
            }
            return Ok(());
        }
        if self.high_surrogate != 0 && c != b'\\' {
            return Err("apply_patch input high surrogate requires a low surrogate");
        }
        match c {
            b'\\' => self.escape_raw.push(c),
            b'"' => self.phase = Phase::AfterValue,
            c if c < 0x20 => return Err("invalid control character in apply_patch input"),
            c => self.input.push(char::from(c)),
        }
        Ok(())
    }

    /// Validates the final wrapper (`arguments` must be exactly `{"input":"..."}`) and returns only
    /// the previously unsent suffix of the input.
    pub fn finish(&mut self, arguments: &str) -> Result<String, String> {
        if let Some(err) = &self.err {
            return Err(err.clone());
        }
        let input = match applypatch::unwrap_input(arguments) {
            Ok(input) => input,
            Err(err) => return Err(self.fail(err)),
        };
        // json decoding replaces invalid UTF-8 and unpaired surrogates; the final snapshot needs
        // the same strict character validation as streamed fragments.
        let mut strict = ApplyPatchInputDecoder::default();
        if let Err(err) = strict.push(arguments) {
            return Err(self.fail(err));
        }
        if self.finished {
            if input != self.input {
                return Err(self.fail("conflicting apply_patch arguments completion"));
            }
            return Ok(String::new());
        }
        let Some(tail) = input.strip_prefix(self.input.as_str()) else {
            return Err(self.fail("final apply_patch input conflicts with streamed input"));
        };
        let tail = tail.to_string();
        self.input.push_str(&tail);
        self.finished = true;
        self.phase = Phase::Complete;
        self.key_raw = Vec::new();
        self.escape_raw = Vec::new();
        self.utf8_pending = Vec::new();
        self.high_surrogate = 0;
        Ok(tail)
    }

    /// The decoded input, preserving its original whitespace.
    pub fn input(&self) -> &str {
        &self.input
    }

    fn fail(&mut self, err: impl Into<String>) -> String {
        let err = err.into();
        self.err = Some(err.clone());
        err
    }
}
