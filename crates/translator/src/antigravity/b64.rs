//! Go-flavoured base64 decoding for the signature carriers (`encoding/base64` Std / RawStd):
//! `\r` and `\n` are ignored, non-zero trailing bits are accepted, padding is required for Std
//! and forbidden for RawStd.

use base64::alphabet::STANDARD;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::Engine;

const fn engine(mode: DecodePaddingMode, pad: bool) -> GeneralPurpose {
    GeneralPurpose::new(
        &STANDARD,
        GeneralPurposeConfig::new()
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(mode)
            .with_encode_padding(pad),
    )
}

static STD: GeneralPurpose = engine(DecodePaddingMode::RequireCanonical, true);
static RAW_STD: GeneralPurpose = engine(DecodePaddingMode::RequireNone, false);

fn strip_newlines(s: &str) -> Vec<u8> {
    s.bytes().filter(|b| *b != b'\r' && *b != b'\n').collect()
}

/// `base64.StdEncoding.DecodeString`.
pub fn decode_std(s: &str) -> Option<Vec<u8>> {
    STD.decode(strip_newlines(s)).ok()
}

/// `base64.RawStdEncoding.DecodeString`.
pub fn decode_raw_std(s: &str) -> Option<Vec<u8>> {
    RAW_STD.decode(strip_newlines(s)).ok()
}

/// `base64.RawStdEncoding.EncodeToString`.
pub fn encode_raw_std(b: &[u8]) -> String {
    RAW_STD.encode(b)
}
