//! Raw ClientHello parsing and JA3/JA4 fingerprints, used to compare what this crate emits with
//! captures of the Go reference (see `tests/fingerprint.rs`).

use std::fmt::Write as _;

/// A parsed ClientHello handshake message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientHello {
    pub legacy_version: u16,
    pub session_id_len: usize,
    pub ciphers: Vec<u16>,
    pub compression: Vec<u8>,
    /// Extensions in wire order as `(type, data)`.
    pub extensions: Vec<(u16, Vec<u8>)>,
    /// Length of the whole TLS record (header included).
    pub record_len: usize,
}

/// RFC 8701 GREASE values (`0x?a?a`).
pub fn is_grease(v: u16) -> bool {
    v & 0x0f0f == 0x0a0a && (v >> 8) == (v & 0xff)
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Some(head)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_be_bytes([b[0], b[1]]))
    }
}

/// u16 values of a vector whose first `skip` bytes are a length prefix.
fn u16_list(data: &[u8], skip: usize) -> Vec<u16> {
    data.get(skip..)
        .unwrap_or_default()
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect()
}

impl ClientHello {
    /// Parses one TLS handshake record holding a ClientHello.
    pub fn parse(record: &[u8]) -> Option<Self> {
        let mut r = Reader(record);
        if r.u8()? != 22 {
            return None;
        }
        r.take(2)?;
        let rec_len = usize::from(r.u16()?);
        let mut body = Reader(r.take(rec_len)?);
        if body.u8()? != 1 {
            return None;
        }
        body.take(3)?;
        let legacy_version = body.u16()?;
        body.take(32)?;
        let sid = usize::from(body.u8()?);
        body.take(sid)?;
        let cl = usize::from(body.u16()?);
        let ciphers = u16_list(body.take(cl)?, 0);
        let ml = usize::from(body.u8()?);
        let compression = body.take(ml)?.to_vec();
        let mut extensions = Vec::new();
        if let Some(el) = body.u16() {
            let mut ext = Reader(body.take(usize::from(el))?);
            while let Some(t) = ext.u16() {
                let l = usize::from(ext.u16()?);
                extensions.push((t, ext.take(l)?.to_vec()));
            }
        }
        Some(Self {
            legacy_version,
            session_id_len: sid,
            ciphers,
            compression,
            extensions,
            record_len: record.len(),
        })
    }

    pub fn extension(&self, ty: u16) -> Option<&[u8]> {
        self.extensions.iter().find(|(t, _)| *t == ty).map(|(_, d)| d.as_slice())
    }

    /// Extension types in wire order with GREASE collapsed to `0x0a0a`.
    pub fn extension_order(&self) -> Vec<u16> {
        self.extensions.iter().map(|(t, _)| if is_grease(*t) { 0x0a0a } else { *t }).collect()
    }

    fn groups(&self) -> Vec<u16> {
        self.extension(10).map(|d| u16_list(d, 2)).unwrap_or_default()
    }

    /// Key share groups in order, GREASE collapsed.
    pub fn key_share_groups(&self) -> Vec<u16> {
        let Some(d) = self.extension(51) else { return Vec::new() };
        let mut r = Reader(d.get(2..).unwrap_or_default());
        let mut out = Vec::new();
        while let Some(g) = r.u16() {
            let Some(l) = r.u16() else { break };
            if r.take(usize::from(l)).is_none() {
                break;
            }
            out.push(if is_grease(g) { 0x0a0a } else { g });
        }
        out
    }

    /// JA3 string (extension order as sent).
    pub fn ja3_string(&self) -> String {
        let join = |v: Vec<u16>| v.iter().map(u16::to_string).collect::<Vec<_>>().join("-");
        let ciphers: Vec<u16> = self.ciphers.iter().copied().filter(|c| !is_grease(*c)).collect();
        let exts: Vec<u16> = self.extensions.iter().map(|(t, _)| *t).filter(|t| !is_grease(*t)).collect();
        let groups: Vec<u16> = self.groups().into_iter().filter(|g| !is_grease(*g)).collect();
        let points: Vec<u16> = self.extension(11).map(|d| d.iter().skip(1).map(|b| u16::from(*b)).collect()).unwrap_or_default();
        format!("{},{},{},{},{}", self.legacy_version, join(ciphers), join(exts), join(groups), join(points))
    }

    /// JA4 (`t<ver><sni><nc><ne><alpn>_<ciphers>_<exts+sigalgs>`), invariant under extension
    /// permutation and GREASE, so it is comparable for the randomized Chrome profile.
    pub fn ja4(&self) -> String {
        let versions: Vec<u16> =
            self.extension(43).map(|d| u16_list(d, 1)).unwrap_or_default().into_iter().filter(|v| !is_grease(*v)).collect();
        let top = versions.iter().copied().max().unwrap_or(self.legacy_version);
        let ver = match top {
            0x0304 => "13",
            0x0303 => "12",
            0x0302 => "11",
            0x0301 => "10",
            _ => "00",
        };
        let sni = if self.extension(0).is_some() { 'd' } else { 'i' };
        let mut ciphers: Vec<u16> = self.ciphers.iter().copied().filter(|c| !is_grease(*c)).collect();
        ciphers.sort_unstable();
        let exts: Vec<u16> = self.extensions.iter().map(|(t, _)| *t).filter(|t| !is_grease(*t)).collect();
        let alpn = self
            .extension(16)
            .and_then(|d| d.get(3..))
            .filter(|p| !p.is_empty())
            .map(|p| {
                let len = usize::from(p[0]).min(p.len().saturating_sub(1));
                let name = &p[1..1 + len];
                match (name.first(), name.last()) {
                    (Some(a), Some(b)) => format!("{}{}", char::from(*a), char::from(*b)),
                    _ => "00".into(),
                }
            })
            .unwrap_or_else(|| "00".into());
        let a = format!("t{ver}{sni}{:02}{:02}{alpn}", ciphers.len().min(99), exts.len().min(99));
        let b = hex12(&ciphers.iter().map(|c| format!("{c:04x}")).collect::<Vec<_>>().join(","));
        let mut ext_sorted: Vec<u16> = exts.iter().copied().filter(|t| *t != 0 && *t != 16).collect();
        ext_sorted.sort_unstable();
        let mut c_in = ext_sorted.iter().map(|t| format!("{t:04x}")).collect::<Vec<_>>().join(",");
        if let Some(d) = self.extension(13) {
            let sig = u16_list(d, 2).iter().map(|s| format!("{s:04x}")).collect::<Vec<_>>().join(",");
            let _ = write!(c_in, "_{sig}");
        }
        format!("{a}_{b}_{}", hex12(&c_in))
    }
}

fn hex12(input: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(input.as_bytes()).iter().take(6).map(|b| format!("{b:02x}")).collect()
}
